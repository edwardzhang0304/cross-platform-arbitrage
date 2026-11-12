//! Recoverable entry admission, independent of the operator's pause intent.
use super::*;
use anyhow::Result;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

const DEPTH_DELAY_MS: u64 = 5_000;
const RECOVERY_MS: u64 = 10_000;
const MAX_CHECK_GAP_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Obstacle { Funding, Depth }
impl std::fmt::Display for Obstacle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { Self::Funding => "本组加仓资金或保证金不足", Self::Depth => "本组盘口深度或滑点条件不足" })
    }
}
impl std::error::Error for Obstacle {}

pub(super) fn classify(error: anyhow::Error, cause: Obstacle) -> anyhow::Error {
    let message = error.to_string();
    error.context(cause).context(message)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomaticEntryPause {
    pub cause: Obstacle,
    pub since_ms: u64,
    pub active: bool,
    pub reason: String,
    // A restart, interrupted feed, or unknown evidence cannot count as a
    // continuously healthy recovery window.
    #[serde(skip)]
    last_check_ms: u64,
    #[serde(skip)]
    healthy_since_ms: Option<u64>,
}

pub fn active(s: &Snapshot) -> bool {
    s.automatic_entry_pause.as_ref().is_some_and(|p| p.active)
}

pub fn invalidate(s: &mut Snapshot) {
    if let Some(p) = &mut s.automatic_entry_pause {
        p.last_check_ms = 0;
        p.healthy_since_ms = None;
    }
}

fn probe_direction(s: &mut Snapshot, books: &[Book; 2], accounts: &[AccountEvidence; 2],
    now: u64, direction: Direction, qty: i64) -> Result<()> {
    let original = s.direction;
    s.direction = direction;
    let result = (|| {
        strategy::risk_check(s, books, accounts, now, qty, Action::Open)?;
        for venue in [Venue::Lighter, Venue::Entropy] {
            execution::protected_limit(s, books, venue, direction.open_side(venue), qty, now)
                .map_err(|e| classify(e, Obstacle::Depth))?;
        }
        Ok(())
    })();
    s.direction = original;
    result
}

fn probe(s: &mut Snapshot, books: &[Book; 2], accounts: &[AccountEvidence; 2], now: u64) -> Result<()> {
    for b in books { b.validate(now, s.config.book_max_age_ms)?; }
    let qty = s.config.common_units(s.config.group_notional,
        (books[0].mid().unwrap() + books[1].mid().unwrap()) / Decimal::TWO)?;
    if s.paired_units() > 0 || s.config.direction_policy != DirectionPolicy::Both {
        let direction = if s.paired_units() > 0 { s.direction } else { Direction::LighterLong };
        return probe_direction(s, books, accounts, now, direction, qty);
    }
    let first = probe_direction(s, books, accounts, now, Direction::LighterLong, qty);
    if first.is_ok() { return first; }
    let second = probe_direction(s, books, accounts, now, Direction::LighterShort, qty);
    if second.is_ok() { return second; }
    // Unknown failures must not be presented as temporary funding/depth pauses.
    if first.as_ref().unwrap_err().downcast_ref::<Obstacle>().is_none() { first } else { second }
}

/// Checks one group's executability even while its spread signal is not ready.
/// Never changes manual pauses, stop requests, latched protection, or orders.
pub fn update(s: &mut Snapshot, books: &[Book; 2], accounts: &[AccountEvidence; 2], now: u64) {
    if s.paused || s.stop_requested || s.close_requested || s.stop_after_close
        || s.pending.is_some() || s.entry_margin_block.is_some() || s.loss_stop.is_some()
        || s.live_orphan.is_some() || s.entry_attempts_remaining == Some(0)
        || !matches!(s.status, Status::Running | Status::Warming | Status::PausedEntries) {
        invalidate(s);
        return;
    }
    if s.lots.len() >= s.config.max_groups {
        invalidate(s);
        return;
    }
    match probe(s, books, accounts, now) {
        Err(error) => {
            let Some(&cause) = error.downcast_ref::<Obstacle>() else { invalidate(s); return; };
            let previous = s.automatic_entry_pause.take();
            let already_active = previous.as_ref().is_some_and(|p| p.active);
            let since = previous.filter(|p| p.cause == cause && p.last_check_ms > 0
                && now >= p.last_check_ms && now - p.last_check_ms <= MAX_CHECK_GAP_MS)
                .map_or(now, |p| p.since_ms);
            let active = already_active || cause == Obstacle::Funding || now.saturating_sub(since) >= DEPTH_DELAY_MS;
            s.automatic_entry_pause = Some(AutomaticEntryPause {
                cause, since_ms: since, active, reason: format!("{error:#}"),
                last_check_ms: now, healthy_since_ms: None,
            });
            strategy::clear_entry_confirmation(s);
        }
        Ok(()) => {
            if let Some(p) = &mut s.automatic_entry_pause {
                if !p.active { s.automatic_entry_pause = None; }
                else {
                    if p.last_check_ms == 0 || now < p.last_check_ms || now - p.last_check_ms > MAX_CHECK_GAP_MS {
                        p.healthy_since_ms = None;
                    }
                    let since = *p.healthy_since_ms.get_or_insert(now);
                    p.last_check_ms = now;
                    if now.saturating_sub(since) >= RECOVERY_MS {
                        s.automatic_entry_pause = None;
                        // Clear only the local preflight cooldown, never a venue rejection latch.
                        s.entry_funding_wait = None;
                        strategy::clear_entry_confirmation(s);
                    }
                }
            }
        }
    }
    if active(s) {
        s.status = Status::PausedEntries;
    } else if s.status == Status::PausedEntries {
        s.status = Status::Running;
    }
}
