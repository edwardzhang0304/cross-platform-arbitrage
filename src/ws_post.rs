use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const WS_POST_CHANNEL_CAPACITY: usize = 256;
const WS_POST_REQUEST_TIMEOUT_SECS: u64 = 15;
const WS_POST_RECONNECT_BASE_MS: u64 = 500;
const WS_POST_RECONNECT_MAX_MS: u64 = 10_000;
const WS_POST_HEARTBEAT_SECS: u64 = 30;

static WS_POST_CLIENTS: OnceLock<Mutex<HashMap<String, WsPostChannels>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct WsPostChannels {
    urgent: mpsc::Sender<WsPostRequest>,
    high: mpsc::Sender<WsPostRequest>,
    low: mpsc::Sender<WsPostRequest>,
}

#[derive(Debug, Clone)]
pub struct WsPostClient {
    sender: mpsc::Sender<WsPostRequest>,
}

#[derive(Debug)]
struct WsPostRequest {
    request_type: &'static str,
    payload: Value,
    response: oneshot::Sender<Result<Value, String>>,
    dispatched: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct WsPostNotDispatched(pub String);

impl std::fmt::Display for WsPostNotDispatched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "websocket post not dispatched: {}", self.0)
    }
}

impl std::error::Error for WsPostNotDispatched {}

#[derive(Debug, Deserialize)]
struct WsEnvelope {
    channel: String,
    #[serde(default)]
    data: Value,
}

#[derive(Debug, Deserialize)]
struct WsPostData {
    id: u64,
    response: WsPostResponse,
}

#[derive(Debug, Deserialize)]
struct WsPostResponse {
    #[serde(rename = "type")]
    response_type: String,
    #[serde(default)]
    payload: Value,
}

impl WsPostClient {
    pub fn for_environment(environment: &str) -> Self {
        Self::for_priority(environment, 1)
    }

    /// Risk exits use the highest-priority queue on the process-wide physical
    /// post socket. This avoids exhausting the venue connection quota while
    /// ensuring ordinary orders and monitoring reads cannot sit in front of an
    /// emergency request.
    pub fn for_emergency_environment(environment: &str) -> Self {
        Self::for_priority(environment, 0)
    }

    /// Monitoring reads share the process-wide physical socket at low priority.
    pub fn for_monitor_environment(environment: &str) -> Self {
        Self::for_priority(environment, 2)
    }

    fn for_priority(environment: &str, priority: u8) -> Self {
        let environment = environment.trim().to_ascii_lowercase();
        let registry_key = environment.clone();
        let clients = WS_POST_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = clients.lock().expect("ws post client registry poisoned");
        if let Some(channels) = guard.get(&registry_key) {
            return Self {
                sender: match priority {
                    0 => channels.urgent.clone(),
                    1 => channels.high.clone(),
                    _ => channels.low.clone(),
                },
            };
        }

        let (urgent, urgent_rx) = mpsc::channel(WS_POST_CHANNEL_CAPACITY);
        let (high, high_rx) = mpsc::channel(WS_POST_CHANNEL_CAPACITY);
        let (low, low_rx) = mpsc::channel(WS_POST_CHANNEL_CAPACITY);
        let channels = WsPostChannels { urgent, high, low };
        tokio::spawn(run_ws_post_worker(
            environment,
            registry_key.clone(),
            urgent_rx,
            high_rx,
            low_rx,
        ));
        let client = Self {
            sender: match priority {
                0 => channels.urgent.clone(),
                1 => channels.high.clone(),
                _ => channels.low.clone(),
            },
        };
        guard.insert(registry_key, channels);
        client
    }

    pub async fn post_action(&self, payload: Value) -> Result<Value> {
        self.post("action", payload).await
    }

    pub async fn post_info(&self, payload: Value) -> Result<Value> {
        self.post("info", payload).await
    }

    async fn post(&self, request_type: &'static str, payload: Value) -> Result<Value> {
        let (response_tx, response_rx) = oneshot::channel();
        let dispatched = Arc::new(AtomicBool::new(false));
        self.sender
            .send(WsPostRequest {
                request_type,
                payload,
                response: response_tx,
                dispatched: dispatched.clone(),
            })
            .await
            .context("websocket post worker is not running")?;

        let response = match tokio::time::timeout(
            Duration::from_secs(WS_POST_REQUEST_TIMEOUT_SECS),
            response_rx,
        )
        .await
        {
            Ok(response) => response.context("websocket post response channel closed")?,
            Err(_) if !dispatched.load(Ordering::Acquire) => {
                return Err(WsPostNotDispatched(
                    "connection unavailable until request deadline".into(),
                )
                .into());
            }
            Err(_) => return Err(anyhow!("websocket post response timed out after dispatch")),
        };

        response.map_err(anyhow::Error::msg)
    }
}

async fn run_ws_post_worker(
    environment: String,
    lane: String,
    mut urgent: mpsc::Receiver<WsPostRequest>,
    mut high: mpsc::Receiver<WsPostRequest>,
    mut low: mpsc::Receiver<WsPostRequest>,
) {
    let mut reconnect_ms = WS_POST_RECONNECT_BASE_MS;
    loop {
        match run_connected_ws_post_worker(&environment, &mut urgent, &mut high, &mut low).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!(
                    environment = %environment,
                    lane = %lane,
                    error = %format!("{error:#}"),
                    reconnect_ms,
                    "websocket post worker disconnected; reconnecting"
                );
                tokio::time::sleep(Duration::from_millis(reconnect_ms)).await;
                reconnect_ms = (reconnect_ms * 2).min(WS_POST_RECONNECT_MAX_MS);
            }
        }
    }
}

async fn run_connected_ws_post_worker(
    environment: &str,
    urgent: &mut mpsc::Receiver<WsPostRequest>,
    high: &mut mpsc::Receiver<WsPostRequest>,
    low: &mut mpsc::Receiver<WsPostRequest>,
) -> Result<()> {
    let (stream, _) = connect_async(ws_url(environment))
        .await
        .with_context(|| format!("failed to connect websocket post channel for {environment}"))?;
    tracing::info!(environment = %environment, "websocket post channel connected");
    let (mut writer, mut reader) = stream.split();
    let mut pending = HashMap::<u64, oneshot::Sender<Result<Value, String>>>::new();
    let mut next_id = 1_u64;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(WS_POST_HEARTBEAT_SECS));

    loop {
        tokio::select! {
            request = receive_request(urgent, high, low) => {
                let Some(request) = request else { return Ok(()); };
                // The caller may have timed out while this worker was reconnecting.
                // Never dispatch an orphaned signed request after its waiter is gone.
                if !request_is_live(&request) {
                    continue;
                }
                let id = next_id;
                next_id = next_id.saturating_add(1).max(1);
                let frame = json!({
                    "method": "post",
                    "id": id,
                    "request": {
                        "type": request.request_type,
                        "payload": request.payload,
                    },
                });
                request.dispatched.store(true, Ordering::Release);
                pending.insert(id, request.response);
                if let Err(error) = writer.send(Message::Text(frame.to_string())).await {
                    fail_pending(&mut pending, format!("failed to send websocket post request: {error}"));
                    return Err(error).context("failed to send websocket post request");
                }
            }
            message = reader.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(error) = handle_ws_post_message(&text, &mut pending) {
                            tracing::warn!(error = %format!("{error:#}"), "failed to handle websocket post message");
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        writer.send(Message::Pong(payload)).await.context("failed to send websocket pong")?;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        fail_pending(&mut pending, "websocket post channel closed".to_string());
                        return Err(anyhow!("websocket post channel closed: {frame:?}"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        fail_pending(&mut pending, format!("websocket post read error: {error}"));
                        return Err(error).context("websocket post read error");
                    }
                    None => {
                        fail_pending(&mut pending, "websocket post stream ended".to_string());
                        return Err(anyhow!("websocket post stream ended"));
                    }
                }
            }
            _ = heartbeat.tick() => {
                writer
                    .send(Message::Text(json!({"method":"ping"}).to_string()))
                    .await
                    .context("failed to send websocket post heartbeat")?;
            }
        }
    }
}

async fn receive_request(
    urgent: &mut mpsc::Receiver<WsPostRequest>,
    high: &mut mpsc::Receiver<WsPostRequest>,
    low: &mut mpsc::Receiver<WsPostRequest>,
) -> Option<WsPostRequest> {
    tokio::select! {
        biased;
        request = urgent.recv() => request,
        request = high.recv() => request,
        request = low.recv() => request,
    }
}

fn request_is_live(request: &WsPostRequest) -> bool {
    !request.response.is_closed()
}

fn handle_ws_post_message(
    text: &str,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, String>>>,
) -> Result<()> {
    let envelope: WsEnvelope = serde_json::from_str(text).context("invalid websocket envelope")?;
    if envelope.channel != "post" {
        return Ok(());
    }
    let data: WsPostData =
        serde_json::from_value(envelope.data).context("invalid websocket post payload")?;
    let Some(response) = pending.remove(&data.id) else {
        return Ok(());
    };

    let payload = data.response.payload;
    let result = match data.response.response_type.as_str() {
        "action" | "info" => Ok(payload),
        "error" => Err(error_payload_to_string(payload)),
        other => Err(format!("unexpected websocket post response type {other}")),
    };
    let _ = response.send(result);
    Ok(())
}

fn fail_pending(
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, String>>>,
    message: String,
) {
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(message.clone()));
    }
}

fn error_payload_to_string(payload: Value) -> String {
    match payload {
        Value::String(message) => message,
        other => other.to_string(),
    }
}

fn ws_url(environment: &str) -> &'static str {
    if environment.eq_ignore_ascii_case("testnet") {
        "wss://api.hyperliquid-testnet.xyz/ws"
    } else {
        "wss://api.hyperliquid.xyz/ws"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(label: &str) -> (WsPostRequest, oneshot::Receiver<Result<Value, String>>) {
        let (tx, rx) = oneshot::channel();
        (
            WsPostRequest {
                request_type: "info",
                payload: json!({"label":label}),
                response: tx,
                dispatched: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    #[tokio::test]
    async fn high_priority_requests_precede_monitoring_reads() {
        let (_urgent_tx, mut urgent_rx) = mpsc::channel(2);
        let (high_tx, mut high_rx) = mpsc::channel(2);
        let (low_tx, mut low_rx) = mpsc::channel(2);
        let (low, _low_reply) = request("low");
        let (high, _high_reply) = request("high");
        low_tx.send(low).await.unwrap();
        high_tx.send(high).await.unwrap();
        let selected = receive_request(&mut urgent_rx, &mut high_rx, &mut low_rx)
            .await
            .unwrap();
        assert_eq!(selected.payload["label"], "high");
    }

    #[tokio::test]
    async fn emergency_requests_precede_orders_and_monitoring_reads() {
        let (urgent_tx, mut urgent_rx) = mpsc::channel(2);
        let (high_tx, mut high_rx) = mpsc::channel(2);
        let (low_tx, mut low_rx) = mpsc::channel(2);
        let (low, _low_reply) = request("low");
        let (high, _high_reply) = request("high");
        let (urgent, _urgent_reply) = request("urgent");
        low_tx.send(low).await.unwrap();
        high_tx.send(high).await.unwrap();
        urgent_tx.send(urgent).await.unwrap();
        let selected = receive_request(&mut urgent_rx, &mut high_rx, &mut low_rx)
            .await
            .unwrap();
        assert_eq!(selected.payload["label"], "urgent");
    }

    #[test]
    fn abandoned_request_is_never_dispatchable_after_reconnect() {
        let (request, reply) = request("abandoned");
        assert!(request_is_live(&request));
        drop(reply);
        assert!(!request_is_live(&request));
    }
}
