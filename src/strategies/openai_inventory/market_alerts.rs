//! Quote evidence only. Never mutates trading state or submits orders.
use super::*;
use std::collections::VecDeque;

const WINDOW_MS: u64 = 60_000;
const BAD_MS: u64 = 48_000;
const MAX_GAP_MS: u64 = 3_000;
const MAX_SAMPLES: usize = 256;
const HISTORY_LIMIT: i64 = 64;

#[derive(Clone)]
pub struct MarketContext {
    pub market: MarketPair,
    pub mode: Mode,
    pub monitoring: bool,
    pub positions: [i64; 2],
    pub max_age_ms: u64,
}
impl MarketContext {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        Self {
            market: s.config.market, mode: s.config.mode,
            monitoring: s.pending.is_some() || s.positions.iter().any(|p| p.units != 0)
                || (!s.paused && !s.stop_requested && matches!(s.status, Status::Running | Status::Warming | Status::Closing)),
            positions: [s.positions[0].units, s.positions[1].units], max_age_ms: s.config.book_max_age_ms,
        }
    }
}
pub struct MarketObservation { pub context: MarketContext, pub books: [Book; 2] }

#[derive(Clone, Serialize, Deserialize)]
struct Sample { at_ms: u64, bad: [bool; 2] }
#[derive(Clone, Serialize, Deserialize)]
struct Evidence {
    at_ms: u64, window_ms: u64, observed_ms: u64, unavailable_ms: [u64; 2],
    current_bad: [bool; 2], eligible: [bool; 2], received_ms: [u64; 2], reasons: [String; 2],
}
#[derive(Serialize, Deserialize)]
struct State {
    schema: u8, samples: VecDeque<Sample>, last_sent_ms: Option<u64>,
    outbox_id: Option<String>, evidence: Option<Evidence>,
}
impl Default for State {
    fn default() -> Self {
        Self { schema: 2, samples: VecDeque::new(), last_sent_ms: None, outbox_id: None, evidence: None }
    }
}
fn load(tx: &Transaction<'_>) -> Result<State> {
    let raw: Option<String> = tx.query_row("SELECT body FROM market_notification_state WHERE id=1", [], |r| r.get(0)).optional()?;
    let Some(raw) = raw else { return Ok(State::default()); };
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    if value.is_null() { return Ok(State::default()); }
    if value.get("schema").and_then(|v| v.as_u64()) == Some(2) { return Ok(serde_json::from_value(value)?); }
    // rc14 used elapsed incident age, not observed bad time. Its queued market
    // messages cannot be reused. Trade incidents/outbox entries stay untouched.
    tx.execute("DELETE FROM notification_outbox WHERE body LIKE '【多平台套利 · 行情持续异常】%'", [])?;
    Ok(State::default())
}
fn save(tx: &Transaction<'_>, s: &State) -> Result<()> {
    tx.execute("INSERT INTO market_notification_state VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body WHERE body<>excluded.body",
        [serde_json::to_string(s)?])?;
    Ok(())
}
fn history(tx: &Transaction<'_>, kind: &str, at: u64, evidence: &Option<Evidence>) -> Result<()> {
    let body = serde_json::json!({"kind":kind,"at_ms":at,"evidence":evidence});
    tx.execute("INSERT INTO market_notification_history(body) VALUES(?1)", [body.to_string()])?;
    tx.execute("DELETE FROM market_notification_history WHERE seq NOT IN (SELECT seq FROM market_notification_history ORDER BY seq DESC LIMIT ?1)", [HISTORY_LIMIT])?;
    Ok(())
}
fn cancel_queue(tx: &Transaction<'_>, s: &mut State, at: u64) -> Result<()> {
    if let Some(id) = s.outbox_id.take() {
        if tx.execute("DELETE FROM notification_outbox WHERE id=?1", [&id])? > 0 {
            history(tx, "cancelled", at, &s.evidence)?;
        }
    }
    Ok(())
}
/// A process restart is not evidence of continuous quote availability/failure.
/// Keep delivery rate limiting, discard stale observations and queued messages.
pub fn reset_market(tx: &Transaction<'_>, at: u64) -> Result<()> {
    let mut s = load(tx)?;
    cancel_queue(tx, &mut s, at)?;
    s.samples.clear(); s.evidence = None;
    save(tx, &s)
}
pub fn expire_unobserved(tx: &Transaction<'_>, at: u64) -> Result<()> {
    let mut s = load(tx)?;
    if s.samples.back().is_some_and(|v| at < v.at_ms || at - v.at_ms > MAX_GAP_MS) {
        cancel_queue(tx, &mut s, at)?;
        s.samples.clear(); s.evidence = None;
    }
    save(tx, &s)
}
pub fn delivered(tx: &Transaction<'_>, id: &str, at: u64) -> Result<()> {
    let mut s = load(tx)?;
    if s.outbox_id.as_deref() == Some(id) {
        s.outbox_id = None; s.last_sent_ms = Some(at);
        history(tx, "delivered", at, &s.evidence)?;
        save(tx, &s)?;
    }
    Ok(())
}
/// HTTP may already be in flight when a fresh quote cancels the queue row.
/// A confirmed send must still start the five-minute rate limit in that race.
pub fn market_send_confirmed(tx: &Transaction<'_>, at: u64) -> Result<()> {
    let mut s = load(tx)?;
    cancel_queue(tx, &mut s, at)?;
    s.last_sent_ms = Some(at);
    history(tx, "delivered", at, &s.evidence)?;
    save(tx, &s)
}
pub fn observe_market(tx: &Transaction<'_>, s: &Snapshot, books: &[Book; 2], at: u64) -> Result<()> {
    observe_market_context(tx, &MarketContext::from_snapshot(s), books, at)
}
pub fn observe_market_context(tx: &Transaction<'_>, c: &MarketContext, books: &[Book; 2], at: u64) -> Result<()> {
    let mut s = load(tx)?;
    if !c.monitoring {
        cancel_queue(tx, &mut s, at)?;
        s.samples.clear(); s.evidence = None;
        return save(tx, &s);
    }
    let bad = books.each_ref().map(|b| b.validate(at, c.max_age_ms).is_err());
    if s.samples.back().is_some_and(|v| at < v.at_ms || at - v.at_ms > MAX_GAP_MS) {
        // Do not count actor/network/CPU delays as quote failures or recovery.
        cancel_queue(tx, &mut s, at)?;
        s.samples.clear();
    }
    if s.samples.back().is_some_and(|v| v.at_ms == at) { s.samples.pop_back(); }
    s.samples.push_back(Sample { at_ms: at, bad });
    let cutoff = at.saturating_sub(WINDOW_MS);
    while s.samples.len() > 1 && s.samples[1].at_ms <= cutoff { s.samples.pop_front(); }
    if s.samples.len() > MAX_SAMPLES {
        cancel_queue(tx, &mut s, at)?;
        s.samples.clear(); s.samples.push_back(Sample { at_ms: at, bad });
    }
    let mut observed_ms = 0;
    let mut unavailable_ms = [0u64; 2];
    for (previous, next) in s.samples.iter().zip(s.samples.iter().skip(1)) {
        let dt = next.at_ms.saturating_sub(previous.at_ms.max(cutoff));
        observed_ms += dt;
        for i in 0..2 { if previous.bad[i] { unavailable_ms[i] += dt; } }
    }
    let eligible = [0, 1].map(|i| bad[i] && observed_ms == WINDOW_MS && unavailable_ms[i] >= BAD_MS);
    let old_eligible = s.evidence.as_ref().map(|e| e.eligible).unwrap_or([false; 2]);
    let evidence = Evidence {
        at_ms: at, window_ms: WINDOW_MS, observed_ms, unavailable_ms, current_bad: bad, eligible,
        received_ms: books.each_ref().map(|b| b.received_ms),
        reasons: books.each_ref().map(|b| (if !b.connected { "disconnected" }
            else if b.received_ms > at { "future_timestamp" }
            else if at - b.received_ms > c.max_age_ms { "stale" }
            else if b.validate(at, c.max_age_ms).is_err() { "invalid_book" } else { "healthy" }).into()),
    };
    // Recovery immediately retracts unsent notices. Each venue qualifies on
    // its own; alternating short Lighter/Entropy faults cannot add together.
    if eligible != old_eligible || !eligible.iter().any(|v| *v) { cancel_queue(tx, &mut s, at)?; }
    if old_eligible.iter().any(|v| *v) && !eligible.iter().any(|v| *v) {
        history(tx, "recovered_or_below_threshold", at, &Some(evidence.clone()))?;
    }
    s.evidence = Some(evidence.clone());
    if eligible.iter().any(|v| *v) && s.last_sent_ms.is_none_or(|t| at >= t && at - t >= REMINDER_MS) {
        let queued = match &s.outbox_id {
            Some(id) => tx.query_row("SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE id=?1)", [id], |r| r.get::<_, bool>(0))?,
            None => false,
        };
        if !queued {
            let detail = ["Lighter RH", "Entropy"].iter().enumerate().filter(|(i, _)| eligible[*i])
                .map(|(i, name)| format!("{name}：最近 60 秒约有 {} 秒盘口不可用，当前仍异常。", unavailable_ms[i] / 1000)).collect::<Vec<_>>().join("\n");
            let time = chrono::DateTime::from_timestamp_millis(at as i64).map(|t| t.to_rfc3339()).unwrap_or_default();
            let body = format!("【多平台套利 · 行情持续异常】\n标的 {} · 模式 {:?}\n{detail}\n正在自动恢复；常规交易仍需要有效报价，已有订单继续按原机制核对。\n账本数量：Lighter {} / Entropy {}\n这是采样统计，不表示期间完全没有更新。\n检测时间 {time}\n请运行程序目录中的一键诊断，发送生成的诊断包。",
                c.market.id().to_uppercase(), c.mode, c.market.quantity(c.positions[0]), c.market.quantity(c.positions[1]));
            s.outbox_id = Some(enqueue(tx, at, &body)?);
            history(tx, "queued", at, &s.evidence)?;
        }
    }
    save(tx, &s)
}
