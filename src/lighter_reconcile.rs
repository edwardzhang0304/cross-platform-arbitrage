//! Authenticated Lighter account observations and deterministic gap repair.
//!
//! Private WebSocket frames are the realtime source of truth. Bounded REST
//! snapshots use the same parser and are reserved for startup/reconnect
//! reconciliation, as required by the V2 runtime architecture.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::{
    lighter::{LighterClient, LighterEndpoints, connect_lighter_websocket},
    lighter_runtime::LighterApiCredential,
};

const PRIVATE_RECONNECT_BASE_MS: u64 = 250;
const PRIVATE_RECONNECT_MAX_MS: u64 = 8_000;
const PRIVATE_AUTH_TTL_SECS: i64 = 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LighterRemoteOrderState {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LighterOrderObservation {
    pub client_order_index: i64,
    pub market_index: i32,
    pub state: LighterRemoteOrderState,
    pub filled_base_size: f64,
    pub remaining_base_size: f64,
    pub exchange_time_ms: i64,
    pub venue_status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LighterFillObservation {
    pub trade_id: i64,
    pub client_order_index: i64,
    pub market_index: i32,
    pub base_size: f64,
    pub exchange_time_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LighterPositionObservation {
    pub market_index: i32,
    pub signed_base_size: f64,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub position_value_usd: f64,
    #[serde(default)]
    pub unrealized_pnl_usd: f64,
    #[serde(default)]
    pub realized_pnl_usd: f64,
    #[serde(default)]
    pub open_order_count: u32,
    #[serde(default)]
    pub pending_order_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LighterAccountObservation {
    Ready { channel: String },
    Order(LighterOrderObservation),
    Fill(LighterFillObservation),
    Position(LighterPositionObservation),
}

/// Runs authenticated order/fill/position streams with bounded reconnect.
/// A fresh API-key-bound token is minted for each connection and never logged.
pub async fn run_private_account_stream(
    endpoints: LighterEndpoints,
    credential: Arc<LighterApiCredential>,
    sender: mpsc::Sender<std::result::Result<LighterAccountObservation, String>>,
) -> Result<()> {
    endpoints.validate()?;
    let mut reconnect_attempt = 0_u32;
    loop {
        let auth = credential.auth_token(PRIVATE_AUTH_TTL_SECS)?;
        let mut reached_ready = false;
        let result = stream_private_account_once(
            &endpoints.ws_url,
            credential.account_index,
            auth.as_str(),
            &sender,
            &mut reached_ready,
        )
        .await;
        if sender.is_closed() {
            return Ok(());
        }
        reconnect_attempt = next_private_reconnect_attempt(reconnect_attempt, reached_ready);
        let error = result
            .err()
            .unwrap_or_else(|| anyhow::anyhow!("Lighter private websocket closed"));
        let _ = sender.send(Err(format!("{error:#}"))).await;
        let exponent = reconnect_attempt.saturating_sub(1).min(5);
        let delay = PRIVATE_RECONNECT_BASE_MS
            .saturating_mul(2_u64.saturating_pow(exponent))
            .min(PRIVATE_RECONNECT_MAX_MS);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

fn next_private_reconnect_attempt(previous: u32, reached_ready: bool) -> u32 {
    if reached_ready {
        1
    } else {
        previous.saturating_add(1)
    }
}

async fn stream_private_account_once(
    ws_url: &str,
    account_index: i64,
    auth_token: &str,
    sender: &mpsc::Sender<std::result::Result<LighterAccountObservation, String>>,
    reached_ready: &mut bool,
) -> Result<()> {
    ensure!(account_index >= 0, "account_index must be non-negative");
    let (stream, _) = connect_lighter_websocket(ws_url)
        .await
        .with_context(|| format!("failed to connect to Lighter private websocket {ws_url}"))?;
    let (mut writer, mut reader) = stream.split();
    let mut subscriptions_sent = false;

    while let Some(message) = reader.next().await {
        match message.context("Lighter private websocket read failed")? {
            Message::Text(text) => {
                let value: Value = serde_json::from_str(&text)
                    .context("invalid Lighter private websocket JSON")?;
                if value.get("type").and_then(Value::as_str) == Some("connected")
                    && !subscriptions_sent
                {
                    for channel in [
                        format!("account_all_orders/{account_index}"),
                        format!("account_all_trades/{account_index}"),
                        format!("account_all_positions/{account_index}"),
                    ] {
                        writer
                            .send(Message::Text(
                                json!({
                                    "type": "subscribe",
                                    "channel": channel,
                                    "auth": auth_token
                                })
                                .to_string(),
                            ))
                            .await
                            .context("failed to subscribe to Lighter private account stream")?;
                    }
                    subscriptions_sent = true;
                    continue;
                }
                if value.get("type").and_then(Value::as_str) == Some("ping") {
                    writer
                        .send(Message::Text(json!({"type": "pong"}).to_string()))
                        .await
                        .context("failed to reply to Lighter application ping")?;
                    continue;
                }
                for observation in parse_private_account_value(&value, account_index)? {
                    if matches!(observation, LighterAccountObservation::Ready { .. }) {
                        *reached_ready = true;
                    }
                    if sender.send(Ok(observation)).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Message::Ping(payload) => writer
                .send(Message::Pong(payload))
                .await
                .context("failed to reply to Lighter websocket ping")?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    bail!("Lighter private websocket closed")
}

/// Fetches the bounded REST snapshots used at startup or after a stream gap.
pub async fn fetch_reconciliation_snapshot(
    client: &LighterClient,
    auth_token: &str,
    account_index: i64,
    market_index: i32,
) -> Result<Vec<LighterAccountObservation>> {
    let active = client
        .account_active_orders(auth_token, account_index, market_index)
        .await?;
    let inactive = client
        .account_inactive_orders(auth_token, account_index, market_index, 100)
        .await?;
    let trades = client
        .account_trades(auth_token, account_index, market_index, 100)
        .await?;
    let mut observations = parse_rest_orders(&active)?;
    observations.extend(parse_rest_orders(&inactive)?);
    observations.extend(parse_rest_trades(&trades, account_index)?);
    Ok(observations)
}

pub fn parse_private_account_message(
    text: &str,
    account_index: i64,
) -> Result<Vec<LighterAccountObservation>> {
    let value: Value = serde_json::from_str(text).context("invalid Lighter private JSON")?;
    parse_private_account_value(&value, account_index)
}

fn parse_private_account_value(
    value: &Value,
    account_index: i64,
) -> Result<Vec<LighterAccountObservation>> {
    let message_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match message_type {
        "subscribed/account_all_orders" | "update/account_all_orders" => {
            with_subscription_ready(value, parse_orders_container(value.get("orders"))?)
        }
        "subscribed/account_all_trades" | "update/account_all_trades" => with_subscription_ready(
            value,
            parse_trades_container(value.get("trades"), account_index)?,
        ),
        "subscribed/account_all_positions" | "update/account_all_positions" => {
            with_subscription_ready(value, parse_positions_container(value.get("positions"))?)
        }
        _ => Ok(Vec::new()),
    }
}

fn with_subscription_ready(
    value: &Value,
    mut observations: Vec<LighterAccountObservation>,
) -> Result<Vec<LighterAccountObservation>> {
    let subscribed = value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("subscribed/"));
    if subscribed {
        let channel = value
            .get("channel")
            .and_then(Value::as_str)
            .context("Lighter subscribed frame is missing channel")?;
        observations.insert(
            0,
            LighterAccountObservation::Ready {
                channel: channel.to_string(),
            },
        );
    }
    Ok(observations)
}

#[cfg(test)]
mod reconnect_tests {
    use super::next_private_reconnect_attempt;

    #[test]
    fn healthy_private_subscription_resets_reconnect_backoff() {
        assert_eq!(next_private_reconnect_attempt(6, true), 1);
        assert_eq!(next_private_reconnect_attempt(0, true), 1);
    }

    #[test]
    fn failed_private_connection_preserves_bounded_backoff_progression() {
        assert_eq!(next_private_reconnect_attempt(0, false), 1);
        assert_eq!(next_private_reconnect_attempt(5, false), 6);
        assert_eq!(next_private_reconnect_attempt(u32::MAX, false), u32::MAX);
    }
}

pub fn parse_rest_orders(value: &Value) -> Result<Vec<LighterAccountObservation>> {
    parse_orders_container(value.get("orders"))
}

pub fn parse_rest_trades(
    value: &Value,
    account_index: i64,
) -> Result<Vec<LighterAccountObservation>> {
    parse_trades_container(value.get("trades"), account_index)
}

fn parse_orders_container(container: Option<&Value>) -> Result<Vec<LighterAccountObservation>> {
    let Some(container) = container else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    collect_rows(container, &mut rows)?;
    rows.into_iter().map(parse_order).collect()
}

fn parse_trades_container(
    container: Option<&Value>,
    account_index: i64,
) -> Result<Vec<LighterAccountObservation>> {
    let Some(container) = container else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    collect_rows(container, &mut rows)?;
    rows.into_iter()
        .filter_map(|row| match parse_trade(row, account_index) {
            Ok(Some(observation)) => Some(Ok(observation)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn parse_positions_container(container: Option<&Value>) -> Result<Vec<LighterAccountObservation>> {
    let Some(Value::Object(markets)) = container else {
        return Ok(Vec::new());
    };
    markets
        .iter()
        .map(|(market_key, value)| {
            let market_index = value_i64(value, &["market_id", "market_index"])
                .or_else(|| market_key.parse::<i64>().ok())
                .context("Lighter position is missing market index")?;
            let magnitude = value_f64(value, &["position", "position_size"])
                .context("Lighter position is missing size")?;
            let sign =
                value_i64(value, &["sign"]).unwrap_or_else(|| if magnitude < 0.0 { -1 } else { 1 });
            Ok(LighterAccountObservation::Position(
                LighterPositionObservation {
                    market_index: i32::try_from(market_index)
                        .context("Lighter position market index exceeds i32")?,
                    signed_base_size: magnitude.abs() * sign as f64,
                    symbol: value
                        .get("symbol")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_ascii_uppercase(),
                    position_value_usd: value_f64(value, &["position_value"])
                        .unwrap_or_default()
                        .abs(),
                    unrealized_pnl_usd: value_f64(value, &["unrealized_pnl"]).unwrap_or_default(),
                    realized_pnl_usd: value_f64(value, &["realized_pnl"]).unwrap_or_default(),
                    open_order_count: value_i64(value, &["open_order_count"])
                        .unwrap_or_default()
                        .max(0) as u32,
                    pending_order_count: value_i64(value, &["pending_order_count"])
                        .unwrap_or_default()
                        .max(0) as u32,
                },
            ))
        })
        .collect()
}

fn collect_rows<'a>(value: &'a Value, output: &mut Vec<&'a Value>) -> Result<()> {
    match value {
        Value::Array(items) => output.extend(items),
        Value::Object(markets) => {
            for rows in markets.values() {
                let array = rows
                    .as_array()
                    .context("Lighter account payload market value must be an array")?;
                output.extend(array);
            }
        }
        Value::Null => {}
        _ => bail!("Lighter account payload must be an array or market map"),
    }
    Ok(())
}

fn parse_order(value: &Value) -> Result<LighterAccountObservation> {
    let client_order_index = value_i64(value, &["client_order_index", "client_order_id"])
        .context("Lighter order is missing client_order_index")?;
    let market_index = value_i64(value, &["market_index", "market_id"])
        .context("Lighter order is missing market_index")?;
    let filled = value_f64(value, &["filled_base_amount"]).unwrap_or(0.0);
    let remaining = value_f64(value, &["remaining_base_amount"]).unwrap_or(0.0);
    ensure!(
        filled >= 0.0 && remaining >= 0.0,
        "negative Lighter order size"
    );
    let venue_status = value
        .get("status")
        .and_then(Value::as_str)
        .context("Lighter order is missing status")?
        .to_ascii_lowercase();
    let state = if venue_status == "filled" {
        LighterRemoteOrderState::Filled
    } else if venue_status.starts_with("canceled") || venue_status == "cancelled" {
        LighterRemoteOrderState::Cancelled
    } else if matches!(venue_status.as_str(), "open" | "pending" | "in-progress") {
        if filled > 0.0 {
            LighterRemoteOrderState::PartiallyFilled
        } else {
            LighterRemoteOrderState::Open
        }
    } else {
        bail!("unsupported Lighter order status {venue_status}")
    };
    Ok(LighterAccountObservation::Order(LighterOrderObservation {
        client_order_index,
        market_index: i32::try_from(market_index).context("market index exceeds i32")?,
        state,
        filled_base_size: filled,
        remaining_base_size: remaining,
        exchange_time_ms: value_i64(
            value,
            &["transaction_time", "updated_at", "timestamp", "created_at"],
        )
        .unwrap_or(0),
        venue_status,
    }))
}

fn parse_trade(value: &Value, account_index: i64) -> Result<Option<LighterAccountObservation>> {
    let ask_account = value_i64(value, &["ask_account_id"]);
    let bid_account = value_i64(value, &["bid_account_id"]);
    let client_order_index = if ask_account == Some(account_index) {
        value_i64(value, &["ask_client_id", "ask_client_id_str"])
    } else if bid_account == Some(account_index) {
        value_i64(value, &["bid_client_id", "bid_client_id_str"])
    } else {
        return Ok(None);
    };
    let Some(client_order_index) = client_order_index else {
        return Ok(None);
    };
    let base_size = value_f64(value, &["size"]).context("Lighter trade is missing size")?;
    ensure!(base_size > 0.0, "Lighter trade size must be positive");
    Ok(Some(LighterAccountObservation::Fill(
        LighterFillObservation {
            trade_id: value_i64(value, &["trade_id", "trade_id_str"])
                .context("Lighter trade is missing trade_id")?,
            client_order_index,
            market_index: i32::try_from(
                value_i64(value, &["market_id", "market_index"])
                    .context("Lighter trade is missing market_id")?,
            )
            .context("market index exceeds i32")?,
            base_size,
            exchange_time_ms: value_i64(value, &["transaction_time", "timestamp"]).unwrap_or(0),
        },
    )))
}

fn value_i64(value: &Value, names: &[&str]) -> Option<i64> {
    names.iter().find_map(|name| {
        let candidate = value.get(*name)?;
        candidate
            .as_i64()
            .or_else(|| candidate.as_u64().and_then(|v| i64::try_from(v).ok()))
            .or_else(|| candidate.as_str()?.parse::<i64>().ok())
    })
}

fn value_f64(value: &Value, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        let candidate = value.get(*name)?;
        candidate
            .as_f64()
            .or_else(|| candidate.as_str()?.parse::<f64>().ok())
            .filter(|number| number.is_finite())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_order_snapshot_and_terminal_update() {
        let open = parse_private_account_message(
            r#"{"type":"subscribed/account_all_orders","channel":"account_all_orders:7","orders":{"0":[{"client_order_index":42,"market_index":0,"filled_base_amount":"0.1","remaining_base_amount":"0.9","status":"open","updated_at":1000}]}}"#,
            7,
        )
        .unwrap();
        assert!(matches!(
            &open[0],
            LighterAccountObservation::Ready { channel }
                if channel == "account_all_orders:7"
        ));
        assert!(matches!(
            &open[1],
            LighterAccountObservation::Order(LighterOrderObservation {
                state: LighterRemoteOrderState::PartiallyFilled,
                client_order_index: 42,
                ..
            })
        ));

        let filled = parse_private_account_message(
            r#"{"type":"update/account_all_orders","orders":{"0":[{"client_order_index":"42","market_index":0,"filled_base_amount":"1","remaining_base_amount":"0","status":"filled"}]}}"#,
            7,
        )
        .unwrap();
        assert!(matches!(
            &filled[0],
            LighterAccountObservation::Order(LighterOrderObservation {
                state: LighterRemoteOrderState::Filled,
                ..
            })
        ));
    }

    #[test]
    fn parses_only_the_local_side_of_fill_updates() {
        let observations = parse_private_account_message(
            r#"{"type":"update/account_all_trades","trades":{"0":[{"trade_id":9,"market_id":0,"size":"0.25","ask_account_id":7,"bid_account_id":8,"ask_client_id":42,"bid_client_id":99,"timestamp":1234}]}}"#,
            7,
        )
        .unwrap();
        assert_eq!(
            observations,
            vec![LighterAccountObservation::Fill(LighterFillObservation {
                trade_id: 9,
                client_order_index: 42,
                market_index: 0,
                base_size: 0.25,
                exchange_time_ms: 1234,
            })]
        );
    }

    #[test]
    fn parses_signed_position_and_ignores_control_frames() {
        let positions = parse_private_account_message(
            r#"{"type":"subscribed/account_all_positions","channel":"account_all_positions:7","positions":{"0":{"market_id":0,"position":"2.5","sign":-1}}}"#,
            7,
        )
        .unwrap();
        assert_eq!(
            positions,
            vec![
                LighterAccountObservation::Ready {
                    channel: "account_all_positions:7".to_string(),
                },
                LighterAccountObservation::Position(LighterPositionObservation {
                    market_index: 0,
                    signed_base_size: -2.5,
                    symbol: String::new(),
                    position_value_usd: 0.0,
                    unrealized_pnl_usd: 0.0,
                    realized_pnl_usd: 0.0,
                    open_order_count: 0,
                    pending_order_count: 0,
                })
            ]
        );
        assert!(
            parse_private_account_message(r#"{"type":"connected"}"#, 7)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn protocol_drift_in_order_status_fails_closed() {
        let error = parse_rest_orders(&json!({
            "orders": [{
                "client_order_index": 1,
                "market_index": 0,
                "status": "new-unknown-state"
            }]
        }))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported Lighter order status")
        );
    }
}
