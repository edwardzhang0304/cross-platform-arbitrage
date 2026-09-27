//! Streaming account snapshots. Missing/stale channels fail closed; reconnect clears all evidence.
use super::Venue;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use tokio_tungstenite::tungstenite::Message;
#[derive(Default)]
struct Cache {
    values: std::collections::BTreeMap<String, (u64, Value)>,
    connection_observed_ms: u64,
}
fn evidence_fresh(value_ms: u64, connection_ms: u64, now_ms: u64, ttl_ms: u64) -> bool {
    now_ms.saturating_sub(value_ms.max(connection_ms)) <= ttl_ms
}
pub struct AccountFeed {
    cache: Arc<RwLock<Cache>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for AccountFeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl AccountFeed {
    pub fn start(
        venue: Venue,
        url: String,
        user: String,
        credential: Option<Arc<crate::lighter_runtime::LighterApiCredential>>,
    ) -> Self {
        let cache = Arc::new(RwLock::new(Cache::default()));
        let shared = cache.clone();
        let task = tokio::spawn(async move {
            loop {
                {
                    let mut cache = shared.write().unwrap();
                    cache.values.clear();
                    cache.connection_observed_ms = 0;
                }
                let run=async {
                let (socket,_)=if venue==Venue::Lighter {
                    crate::lighter::connect_lighter_websocket(&url).await?
                } else {
                    // Entropy/Hyperliquid uses its own direct connection, like
                    // its market feed and submission channel. Do not route it
                    // through a proxy selected specifically for Lighter RH.
                    tokio::time::timeout(std::time::Duration::from_secs(10),tokio_tungstenite::connect_async(&url)).await.context("Entropy account connection timeout")??
                };
                let (mut writer,mut reader)=socket.split();
                let subscriptions=if venue==Venue::Lighter {
                    let token=credential.as_ref().context("missing account credential")?.auth_token(600)?;
                    vec![json!({"type":"subscribe","channel":format!("account_all_positions/{user}"),"auth":token.as_str()}),json!({"type":"subscribe","channel":format!("user_stats/{user}"),"auth":token.as_str()})]
                }else{[json!({"type":"clearinghouseState","user":user,"dex":"io"}),json!({"type":"openOrders","user":user,"dex":"io"}),json!({"type":"activeAssetData","user":user,"coin":"io:OAI"}),json!({"type":"spotState","user":user})].into_iter().map(|s|json!({"method":"subscribe","subscription":s})).collect()};
                for sub in subscriptions {writer.send(Message::Text(sub.to_string())).await?;}
                // Account snapshots are event driven and may remain unchanged for
                // minutes. Keep their evidence fresh only while this same private
                // connection continues to answer frequent heartbeats.
                let mut heartbeat=tokio::time::interval(std::time::Duration::from_secs(2));
                let deadline=tokio::time::sleep(std::time::Duration::from_secs(500));tokio::pin!(deadline);
                loop {tokio::select! {
                    _=&mut deadline=>anyhow::bail!("renew account stream"),
                    _=heartbeat.tick()=>writer.send(Message::Text(if venue==Venue::Lighter {json!({"type":"ping"})}else{json!({"method":"ping"})}.to_string())).await?,
                    msg=reader.next()=>{let msg=msg.context("account stream closed")??;shared.write().unwrap().connection_observed_ms=crate::domain::now_ms();match msg {
                        Message::Text(text)=>{let v:Value=serde_json::from_str(&text)?;
                            if v["type"]=="ping" {writer.send(Message::Text(json!({"type":"pong"}).to_string())).await?;continue;}
                            let key=if venue==Venue::Lighter {v["channel"].as_str().unwrap_or("").split(':').next().unwrap_or("")}else{v["channel"].as_str().unwrap_or("")};
                            if key.is_empty(){continue;}
                            let body=if venue==Venue::Lighter {v.clone()}else{v["data"].clone()};
                            let mut cache=shared.write().unwrap();
                            if venue==Venue::Lighter && key=="account_all_positions" {
                                let prior=cache.values.get(key).map(|(_,v)|v);
                                let body=merge_position_frame(prior,&body)?;
                                cache.values.insert(key.to_owned(),(crate::domain::now_ms(),body));
                            } else {cache.values.insert(key.to_owned(),(crate::domain::now_ms(),body));}
                        },Message::Ping(p)=>writer.send(Message::Pong(p)).await?,Message::Close(_)=>anyhow::bail!("account stream closed"),_=>{}}
                    }
                }}
                #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
            }.await;
                if let Err(error) = run {
                    tracing::warn!(?venue,error=%error,"inventory account stream reconnecting");
                }
                {
                    let mut cache = shared.write().unwrap();
                    cache.values.clear();
                    cache.connection_observed_ms = 0;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
        Self { cache, task }
    }
    pub fn get(&self, key: &str, ttl: u64) -> Result<(u64, Value)> {
        let c = self.cache.read().unwrap();
        let (t, v) = c
            .values
            .get(key)
            .context("account stream warming/reconnecting")?;
        anyhow::ensure!(
            evidence_fresh(*t, c.connection_observed_ms, crate::domain::now_ms(), ttl),
            "account stream evidence stale"
        );
        Ok((*t, v.clone()))
    }
}

// A subscription is a full snapshot; update frames omit unchanged markets.
// Require a snapshot before applying deltas, and retain explicit zero rows.
fn merge_position_frame(prior: Option<&Value>, frame: &Value) -> Result<Value> {
    let rows = frame["positions"]
        .as_object()
        .context("invalid position frame")?;
    if frame["type"] == "subscribed/account_all_positions" {
        return Ok(frame.clone());
    }
    anyhow::ensure!(
        frame["type"] == "update/account_all_positions",
        "unknown position frame type"
    );
    let mut next = prior
        .context("position delta before subscription snapshot")?
        .clone();
    let positions = next["positions"]
        .as_object_mut()
        .context("missing base position snapshot")?;
    for (market, row) in rows {
        positions.insert(market.clone(), row.clone());
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::evidence_fresh;

    #[test]
    fn unchanged_snapshot_stays_fresh_only_with_live_private_connection() {
        assert!(evidence_fresh(1_000, 9_000, 10_000, 3_000));
        assert!(!evidence_fresh(1_000, 0, 10_000, 3_000));
        assert!(!evidence_fresh(1_000, 6_000, 10_000, 3_000));
    }
}

#[cfg(test)]
mod position_delta_tests {
    use super::*;
    #[test]
    fn delta_preserves_omitted_market_and_snapshot_replaces_it() {
        let initial = json!({"type":"subscribed/account_all_positions","positions":{"42":{"position":"0.108","sign":1}}});
        let other = json!({"type":"update/account_all_positions","positions":{"1":{"position":"0.2","sign":1}}});
        assert!(merge_position_frame(None, &other).is_err());
        let updated = merge_position_frame(Some(&initial), &other).unwrap();
        assert_eq!(updated["positions"]["42"], initial["positions"]["42"]);
        let zero = json!({"type":"update/account_all_positions","positions":{"42":{"position":"0.0","sign":1}}});
        assert_eq!(
            merge_position_frame(Some(&updated), &zero).unwrap()["positions"]["42"]["position"],
            "0.0"
        );
        let reset = json!({"type":"subscribed/account_all_positions","positions":{}});
        assert_eq!(
            merge_position_frame(Some(&updated), &reset).unwrap()["positions"],
            json!({})
        );
    }
}
