//! Durable paired workflow. Every dispatch is persisted before the account worker receives it.
use super::{store::Store, venue::AccountWorker, *};
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;

pub(super) const REPAIR_RETRY_DELAY_MS: u64 = 5_000;
pub(super) const FAST_REPAIR_RETRIES: u32 = 3;
/// Read-only capability used to verify that a user-requested reload took effect.
pub const RESIDUAL_RECOVERY_VERSION: u32 = 2;
const RESIDUAL_HALT: &str = "residual could not be neutralized within protected execution; manual attention required";
const ALIGNMENT_HALT: &str = "close precision alignment incomplete; residual review required";
const UNWIND_HALT: &str = "non-common hedge fill could not be fully unwound; reconciliation required";

#[derive(Clone, Copy, PartialEq)]
enum RecoveryPhase { Repair, Alignment, Unwind }

fn retry_delay(attempt: u32) -> u64 {
    if attempt < FAST_REPAIR_RETRIES { REPAIR_RETRY_DELAY_MS }
    else if attempt < FAST_REPAIR_RETRIES * 2 { 15_000 } else { 30_000 }
}

// A terminal failure can be retried; an unknown request must only be looked up.
fn recovery_phase(s: &Snapshot) -> Option<RecoveryPhase> {
    let p = s.pending.as_ref()?;
    if !p.first_terminal { return None; }
    if s.reason == RESIDUAL_HALT && p.hedge_terminal && p.repair_terminal
        && p.align_close.as_ref().is_none_or(|_| p.align_close_terminal)
        && p.unwind_hedge.as_ref().is_none_or(|_| p.unwind_hedge_terminal
            && p.unwind_hedge_filled == p.hedge_filled)
        && p.first_filled > p.paired_filled() + p.repair_filled {
        Some(RecoveryPhase::Repair)
    } else if s.reason == ALIGNMENT_HALT && p.action == Action::Close
        && p.align_close_terminal && p.first_filled % s.config.common_step() != 0
        && p.hedge.is_none() && p.hedge_filled == 0 && p.repair.is_none() && p.repair_filled == 0 {
        Some(RecoveryPhase::Alignment)
    } else if s.reason == UNWIND_HALT && p.action == Action::Open
        && p.first_venue == Venue::Entropy && p.hedge_terminal
        && p.hedge_filled % s.config.common_step() != 0 && p.unwind_hedge_terminal
        && p.unwind_hedge_filled < p.hedge_filled && p.repair.is_none() && p.repair_filled == 0 {
        Some(RecoveryPhase::Unwind)
    } else { None }
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod repair_tests;

#[cfg(test)]
#[path = "execution_clock_tests.rs"]
mod clock_tests;

/// Explicit reconciliation may construct a fresh reducing repair only after
/// the prior request is terminal and fresh account evidence matches the owned
/// ledger. A submitted/unknown request is never reset or reissued.
pub fn resume_terminal_repair(
    s: &mut Snapshot,
    accounts: Option<&[AccountEvidence; 2]>,
    now: u64,
) -> Result<bool> {
    let a = accounts.context("fresh account evidence required for repair retry")?;
    s.assert_reconciled(a)?;
    ensure!(
        a.iter().all(|x| {
            x.authenticated
                && x.open_orders == 0
                && x.observed_ms <= now
                && now - x.observed_ms <= s.config.account_max_age_ms
        }),
        "fresh reconciled accounts without orders required for repair retry"
    );
    let Some(p) = s.pending.as_mut() else {
        return Ok(false);
    };
    ensure!(
        p.first_terminal && p.hedge_terminal
            && p.align_close.as_ref().is_none_or(|_| p.align_close_terminal)
            && p.unwind_hedge.as_ref().is_none_or(|_| p.unwind_hedge_terminal),
        "paired legs are not terminal"
    );
    ensure!(p.repair_terminal, "repair outcome is not terminal");
    let residual = p.first_filled - p.paired_filled() - p.repair_filled;
    ensure!(residual > 0, "no residual exposure requires repair");
    // Count pre-dispatch failures as well as submitted terminal requests.
    p.repair_attempt = p.repair_attempt.checked_add(1).context("repair retry counter overflow")?;
    p.repair = None;
    p.repair_terminal = false;
    p.repair_retry_after_ms = None;
    p.quote_wait_started_ms = None;
    Ok(true)
}

pub(super) fn automatic_repair_due(s: &Snapshot, now: u64) -> bool {
    s.config.auto_neutralize && s.live_orphan.is_none()
        && s.status == Status::NeedsAttention && recovery_phase(s).is_some()
        && s.pending.as_ref().is_some_and(|p| p.repair_retry_after_ms.is_some_and(|after| now >= after))
}

/// Caller must obtain explicit REST reconciliation first. Only the confirmed
/// excess from this operation may be reduced. An opening repair unwinds its
/// first leg; a closing repair completes its other leg. Existing pairs remain
/// separately owned. Pause/stop suppress new entries, not protective completion.
pub(super) fn resume_protected_repair(
    s: &mut Snapshot, accounts: &[AccountEvidence; 2], books: &[Book; 2], now: u64,
) -> Result<bool> {
    if !automatic_repair_due(s, now) { return Ok(false); }
    let phase = recovery_phase(s).unwrap();
    if accounts.iter().any(|a| !a.authenticated || a.open_orders != 0
        || a.observed_ms > now || now - a.observed_ms > s.config.account_max_age_ms) {
        return Ok(false);
    }
    s.assert_reconciled(accounts)?;
    let p = s.pending.as_ref().unwrap();
    ensure!(p.first_filled > 0 && p.first_filled <= p.requested_units && p.repair_filled >= 0
        && p.paired_filled() >= 0 && p.paired_filled() + p.repair_filled <= p.first_filled,
        "invalid residual accounting");
    let close = p.action == Action::Close;
    if close {
        ensure!(p.first_venue == Venue::Lighter
            && (phase == RecoveryPhase::Alignment || p.first_filled % s.config.common_step() == 0)
            && p.unwind_hedge.is_none(), "close residual precision or leg ordering is unresolved");
        let planned = if p.close_allocations.is_empty() {
            s.lots.iter().filter(|l| p.close_lot_id.as_ref().is_none_or(|id| id == &l.id))
                .map(|l| CloseAllocation { lot_id: l.id.clone(), units: l.units }).collect::<Vec<_>>()
        } else { p.close_allocations.clone() };
        let mut ids = std::collections::BTreeSet::new();
        ensure!(planned.iter().all(|a| a.units > 0 && a.units % s.config.common_step() == 0
            && ids.insert(&a.lot_id)
            && s.lots.iter().any(|l| l.id == a.lot_id && l.units >= a.units)),
            "close residual allocation is not owned inventory");
        let reserved: i64 = planned.iter().map(|a| a.units).sum();
        ensure!(p.first_filled <= reserved && p.requested_units <= reserved
            && (p.close_allocations.is_empty() || reserved == p.requested_units),
            "close residual exceeds reserved inventory");
    } else {
        ensure!(p.align_close.is_none(), "opening operation contains a close alignment");
    }
    for venue in [Venue::Lighter, Venue::Entropy] {
        let held = if close {
            s.paired_units() - if venue == p.first_venue { p.first_filled }
                else { p.paired_filled() + p.repair_filled }
        } else {
            s.paired_units() + if venue == p.first_venue { p.first_filled - p.repair_filled }
                else { p.paired_filled() }
        };
        ensure!(held >= 0, "repair would exceed owned position");
        let expected = s.direction.open_side(venue).sign() * held;
        ensure!(s.positions[venue.index()].units == expected,
            "residual ownership differs from existing paired inventory");
    }
    let (venue, qty, suffix) = match phase {
        RecoveryPhase::Repair => (if close { p.hedge_venue() } else { p.first_venue },
            p.first_filled - p.paired_filled() - p.repair_filled, "repair"),
        RecoveryPhase::Alignment => {
            let remaining = s.config.common_step() - p.first_filled % s.config.common_step();
            ensure!(p.first_filled + remaining <= p.requested_units, "close alignment exceeds original quantity");
            (Venue::Lighter, remaining, "align-close")
        }
        RecoveryPhase::Unwind => (Venue::Lighter, p.hedge_filled - p.unwind_hedge_filled, "unwind-hedge"),
    };
    if request(s, books, venue, s.direction.open_side(venue).opposite(),
        qty, true, suffix, now).is_err() { return Ok(false); }
    if phase == RecoveryPhase::Repair {
        resume_terminal_repair(s, Some(accounts), now)?;
    } else {
        let p = s.pending.as_mut().unwrap();
        p.repair_attempt = p.repair_attempt.checked_add(1).context("recovery retry counter overflow")?;
        p.repair_retry_after_ms = None;
        p.quote_wait_started_ms = None;
        if phase == RecoveryPhase::Alignment { p.align_close = None; p.align_close_terminal = false; }
        else { p.unwind_hedge = None; p.unwind_hedge_terminal = false; }
    }
    s.status = Status::Recovering;
    s.reason = "terminal residual verified; retrying protected reduce-only repair".into();
    Ok(true)
}

/// Same depth/price check at signal reservation and actual dispatch; dispatch always rechecks.
pub(super) fn protected_limit(
    s: &Snapshot,
    books: &[Book; 2],
    venue: Venue,
    side: Side,
    qty: i64,
    now: u64,
) -> Result<Decimal> {
    let b = &books[venue.index()];
    b.validate(now, s.config.book_max_age_ms)?;
    let (_, worst) = b.vwap(side, qty)?;
    let best = if side == Side::Buy {
        b.asks[0].price
    } else {
        b.bids[0].price
    };
    let limit = best
        * (Decimal::ONE
            + Decimal::from(side.sign()) * s.config.execution_slippage_bps / Decimal::from(10_000));
    let limit=if s.config.market==MarketPair::Anth {
        s.config.market.protected_price(venue,limit,side==Side::Buy)?
    }else{limit};
    ensure!(
        if side == Side::Buy {
            worst <= limit
        } else {
            worst >= limit
        },
        "depth exceeds protected execution price"
    );
    Ok(limit)
}

fn request(
    s: &Snapshot,
    books: &[Book; 2],
    venue: Venue,
    side: Side,
    qty: i64,
    reduce: bool,
    suffix: &str,
    now: u64,
) -> Result<OrderRequest> {
    let b = &books[venue.index()];
    b.validate(now, s.config.book_max_age_ms)?;
    ensure!(
        qty > 0 && (venue == Venue::Lighter || qty % s.config.common_step() == 0),
        "unrepresentable hedge quantity"
    );
    let (vwap, worst) = b.vwap(side, qty)?;
    if !reduce {
        ensure!(
            s.config.quantity(qty) * vwap >= Decimal::from(10),
            "hedge below minimum notional"
        );
    }
    let mut limit = protected_limit(s, books, venue, side, qty, now)?;
    let op = s
        .pending
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing operation"))?;
    if suffix == "hedge" && op.action == Action::Open && op.first_venue == Venue::Entropy {
        if let Some(min_spread) = op.min_entry_spread {
            ensure!(
                op.first_filled > 0,
                "missing first-leg fill for hedge budget"
            );
            let first_price = op.first_value / Decimal::from(op.first_filled);
            limit = if side == Side::Buy { limit.min(first_price - min_spread) }
                else { limit.max(first_price + min_spread) };
            ensure!(
                limit > Decimal::ZERO && if side == Side::Buy { worst <= limit } else { worst >= limit },
                "hedge would breach paired entry spread budget"
            );
        }
    }
    let suffix = if matches!(suffix, "repair" | "align-close" | "unwind-hedge") && op.repair_attempt > 0 {
        format!("{suffix}-{}", op.repair_attempt)
    } else {
        suffix.to_string()
    };
    Ok(OrderRequest {
        id: format!("{}-v2-{suffix}", op.id),
        venue,
        side,
        units: qty,
        limit,
        arrival_mid: b.mid(),
        reduce_only: reduce,
        created_ms: now,
        expires_ms: now + s.config.operation_timeout_ms,
        signed_expires_ms: (venue == Venue::Lighter).then_some(now + 599_000),
    })
}

fn apply(s: &mut Snapshot, which: usize, req: &OrderRequest, result: OrderResult) -> Result<()> {
    let created = result.exchange_created_ms
        .map(|at| req.verified_exchange_created(at)).transpose()?.unwrap_or(req.created_ms);
    for f in result.fills {
        ensure!(
            f.order_id == req.id && f.venue == req.venue && f.side == req.side,
            "fill is not owned by request"
        );
        ensure!(
            f.time_ms >= created.saturating_sub(1000),
            "fill predates order"
        );
        ensure!(f.time_ms <= req.signed_expiry().saturating_add(300_000),
            "fill exceeds bounded order lifetime");
        let was_new = s.record_fill(&f, req.arrival_mid)?;
        if was_new {
            let op = s.pending.as_mut().unwrap();
            match which {
                0 => {
                    op.first_filled += f.units;
                    op.first_value += f.price * Decimal::from(f.units);
                }
                1 => {
                    op.hedge_filled += f.units;
                    op.hedge_value += f.price * Decimal::from(f.units);
                }
                2 => op.repair_filled += f.units,
                4 => {
                    op.align_close_filled += f.units;
                    op.first_filled += f.units;
                    op.first_value += f.price * Decimal::from(f.units);
                }
                _ => op.unwind_hedge_filled += f.units,
            }
        }
    }
    // Repair totals span multiple requests. Overfill must be checked against
    // this request's fills, not the cumulative quantity of earlier attempts.
    let request_filled: i64 = s.fills.values().filter(|f| f.order_id == req.id).map(|f| f.units).sum();
    ensure!(request_filled <= req.units, "venue overfill or inconsistent duplicate history");
    let op = s.pending.as_mut().unwrap();
    let qty = match which {
        0 => op.first_filled,
        1 => op.hedge_filled,
        2 => request_filled,
        4 => request_filled,
        _ => request_filled,
    };
    ensure!(
        qty <= req.units,
        "venue overfill or inconsistent duplicate history"
    );
    ensure!(op.repair_filled <= op.first_filled - op.paired_filled(),
        "repair exceeds remaining owned exposure");
    ensure!(op.first_filled <= op.requested_units && op.unwind_hedge_filled <= op.hedge_filled,
        "recovery exceeds original operation quantity");
    match which {
        0 => op.first_terminal = result.terminal,
        1 => op.hedge_terminal = result.terminal,
        2 => op.repair_terminal = result.terminal,
        4 => op.align_close_terminal = result.terminal,
        _ => op.unwind_hedge_terminal = result.terminal,
    };
    s.reason = result.reason.clone();
    if result.terminal && s.status == Status::RecoveringExposure {
        s.status = Status::Recovering;
    }
    if !result.terminal {
        s.status = Status::RecoveringExposure;
        s.reason = result.reason;
    }
    Ok(())
}

/// Only a reservation with no attempt marker and no fills is safe to discard.
/// Pause/stop must retain submitted or unknown orders so they can be reconciled.
pub fn cancel_unsent_entry(s: &mut Snapshot) -> bool {
    let safe = s.pending.as_ref().is_some_and(|p| p.action == Action::Open
        && [&p.first, &p.hedge, &p.repair, &p.align_close, &p.unwind_hedge].iter().all(|r| r.is_none())
        && [p.first_filled,p.hedge_filled,p.repair_filled,p.align_close_filled,p.unwind_hedge_filled].iter().all(|n| *n == 0));
    if !safe { return false; }
    s.pending = None;
    strategy::clear_entry_confirmation(s);
    s.reason = "unsent entry cancelled by risk control".into();
    s.status = if s.close_requested && s.paired_units() > 0 { Status::Closing }
        else if s.stop_requested { Status::Stopped } else { Status::PausedEntries };
    true
}

/// At most one request/query per tick. On uncertain submission: query the SAME id, never resubmit.
pub async fn advance(
    s: &mut Snapshot,
    store: &mut Store,
    workers: &[AccountWorker; 2],
    books: &[Book; 2],
    now: u64,
) -> Result<()> {
    if (s.paused || s.stop_requested || s.close_requested || s.loss_stop.is_some())
        && cancel_unsent_entry(s) {
        store.commit(s, now, "unsent_entry_cancelled")?;
        return Ok(());
    }
    let unsent_entry = s
        .pending
        .as_ref()
        .is_some_and(|op| op.action == Action::Open && op.first.is_none());
    // No attempt marker means no request could have been sent. A transient
    // stale quote must discard this intention, not strand a flat live worker.
    // Never apply this to a submitted/unknown order or to a protective close.
    let entry_ready = || -> Result<()> {
        for b in books {
            b.validate(now, s.config.book_max_age_ms)?;
        }
        let op = s.pending.as_ref().unwrap();
        ensure!(
            now.saturating_sub(op.created_ms) <= s.config.operation_timeout_ms,
            "entry intention expired"
        );
        ensure!(
            strategy::entry_sampling_progress(s, now).ready,
            "mean history no longer ready"
        );
        let mean = strategy::entry_reference_mean_for(s, now, s.direction)
            .ok_or_else(|| anyhow::anyhow!("missing reference mean"))?;
        let threshold = s.required_entry_spread(mean,op.level);
        let entry = s.direction.entry(books, op.requested_units)?;
        ensure!(entry >= threshold, "entry spread moved below threshold");
        Ok(())
    };
    if unsent_entry && s.loss_stop.is_none() && entry_ready().is_err() {
        s.pending = None;
        s.previous_signal = None;
        s.previous_reverse_signal = None;
        s.status = if s.stop_requested {
            Status::Stopped
        } else if s.paused {
            Status::PausedEntries
        } else {
            Status::Running
        };
        s.reason = "unsent entry expired; waiting for two fresh signals".into();
        store.commit(s, now, "unsent_entry_discarded")?;
        return Ok(());
    }
    let loss_check = super::strategy::enforce_loss_limit(s, books, now);
    // New risk needs a current combined-PnL valuation. Reconciliation of an
    // already-submitted leg must remain possible when market data is absent.
    let changed = if unsent_entry {
        loss_check?
    } else {
        loss_check.unwrap_or(false)
    };
    if changed {
        store.commit(s, now, "loss_limit_latched")?;
    }
    if s.pending
        .as_ref()
        .is_some_and(|p| p.action == Action::Close && p.first.is_none())
        && !s.close_requested
        && s.loss_stop.is_none()
    {
        let mut repriced = false;
        let profitable = (|| -> Result<bool> {
            for b in books {
                b.validate(now, s.config.book_max_age_ms)?;
            }
            repriced = strategy::refresh_unsent_close_batch(s, books, now)?;
            strategy::normal_exit_eligible(s, books, now)
        })()
        .unwrap_or(false);
        if repriced { store.commit(s, now, "unsent_profit_batch_repriced")?; }
        if !profitable {
            s.pending = None;
            s.previous_exit = None;
            s.reason = "unsent profit exit no longer eligible; waiting".into();
            store.commit(s, now, "unsent_profit_exit_discarded")?;
            return Ok(());
        }
    }
    let Some(op) = s.pending.clone() else {
        return Ok(());
    };
    let close = op.action == Action::Close;
    ensure!(
        !close || op.first_venue == Venue::Lighter,
        "unsupported close leg ordering"
    );
    let (which, existing, new) = if !op.first_terminal {
        (
            0,
            op.first.clone(),
            if op.first.is_none() {
                Some(request(
                    s,
                    books,
                    op.first_venue,
                    s.direction.side(op.first_venue, op.action),
                    op.requested_units,
                    close,
                    "first",
                    now,
                ))
            } else {
                None
            },
        )
    } else if op.first_filled == 0 {
        s.finish_operation(now)?;
        store.commit(s, now, "operation_empty")?;
        return Ok(());
    } else if close
        && (op.first_filled % s.config.common_step() != 0 || (op.align_close.is_some() && !op.align_close_terminal))
    {
        // A finer-precision IOC may close 0.0195 of the requested 0.0200.
        // Close the remaining 0.0005 on Lighter first, never exceed the original
        // common-size close budget, then reduce the same 0.0200 on Entropy.
        if op.align_close_terminal {
            s.status = Status::NeedsAttention;
            s.reason = ALIGNMENT_HALT.into();
            let p = s.pending.as_mut().unwrap();
            p.repair_retry_after_ms.get_or_insert(now.saturating_add(retry_delay(p.repair_attempt)));
            store.commit(s, now, "unresolved_close_alignment")?;
            return Ok(());
        }
        let remaining = s.config.common_step() - op.first_filled % s.config.common_step();
        ensure!(
            op.first_filled + remaining <= op.requested_units || op.align_close.is_some(),
            "close alignment exceeds original quantity"
        );
        (
            4,
            op.align_close.clone(),
            if op.align_close.is_none() {
                Some(request(
                    s,
                    books,
                    Venue::Lighter,
                    s.direction.open_side(Venue::Lighter).opposite(),
                    remaining,
                    true,
                    "align-close",
                    now,
                ))
            } else {
                None
            },
        )
    } else if !op.hedge_terminal {
        let q = if op.hedge_venue() == Venue::Entropy {
            op.first_filled / s.config.common_step() * s.config.common_step()
        } else {
            op.first_filled
        };
        (
            1,
            op.hedge.clone(),
            if op.hedge.is_none() {
                Some(request(
                    s,
                    books,
                    op.hedge_venue(),
                    s.direction.side(op.hedge_venue(), op.action),
                    q,
                    close,
                    "hedge",
                    now,
                ))
            } else {
                None
            },
        )
    } else if !close
        && op.first_venue == Venue::Entropy
        && op.hedge_filled % s.config.common_step() != 0
        && op.unwind_hedge_filled < op.hedge_filled
    {
        // An IOC on the finer-precision venue can still partially fill a
        // non-common quantity. Unwind this operation's entire Lighter fill,
        // then unwind the Entropy first leg, without touching older inventory.
        if op.unwind_hedge_terminal {
            s.status = Status::NeedsAttention;
            s.reason = UNWIND_HALT.into();
            let p = s.pending.as_mut().unwrap();
            p.repair_retry_after_ms.get_or_insert(now.saturating_add(retry_delay(p.repair_attempt)));
            store.commit(s, now, "unresolved_hedge_unwind")?;
            return Ok(());
        }
        (
            3,
            op.unwind_hedge.clone(),
            if op.unwind_hedge.is_none() {
                Some(request(
                    s,
                    books,
                    Venue::Lighter,
                    s.direction.open_side(Venue::Lighter).opposite(),
                    op.hedge_filled - op.unwind_hedge_filled,
                    true,
                    "unwind-hedge",
                    now,
                ))
            } else {
                None
            },
        )
    } else if op.first_filled > op.paired_filled() + op.repair_filled {
        if op.repair_terminal {
            s.status = Status::NeedsAttention;
            s.reason = RESIDUAL_HALT.into();
            let p = s.pending.as_mut().unwrap();
            p.repair_retry_after_ms.get_or_insert(now.saturating_add(retry_delay(p.repair_attempt)));
            store.commit(s, now, "unresolved_residual")?;
            return Ok(());
        }
        let venue = if close {
            op.hedge_venue()
        } else {
            op.first_venue
        };
        (
            2,
            op.repair.clone(),
            if op.repair.is_none() {
                Some(request(
                    s,
                    books,
                    venue,
                    s.direction.open_side(venue).opposite(),
                    op.first_filled - op.paired_filled() - op.repair_filled,
                    true,
                    "repair",
                    now,
                ))
            } else {
                None
            },
        )
    } else {
        s.finish_operation(now)?;
        store.commit(s, now, "operation_matched")?;
        return Ok(());
    };
    let is_new = existing.is_none();
    let req = if let Some(req) = existing {
        req
    } else {
        match new.unwrap() {
            Ok(req) => req,
            Err(error) => {
                if which == 0 && close && error.to_string() == "book disconnected or stale" {
                    s.reason = "protective close waiting for fresh first-leg book".into();
                    store.commit(s, now, "waiting_close_quote")?;
                    return Ok(());
                }
                if which != 0 && error.to_string() == "book disconnected or stale" {
                    let started = *s
                        .pending
                        .as_mut()
                        .unwrap()
                        .quote_wait_started_ms
                        .get_or_insert(now);
                    if now.saturating_sub(started) <= s.config.operation_timeout_ms {
                        s.reason =
                            "waiting within phase deadline for fresh hedge/repair book".into();
                        store.commit(s, now, "waiting_phase_quote")?;
                        return Ok(());
                    }
                }
                let p = s.pending.as_mut().unwrap();
                p.failed = true;
                p.quote_wait_started_ms = None;
                match which {
                    0 => p.first_terminal = true,
                    1 => p.hedge_terminal = true,
                    2 => p.repair_terminal = true,
                    4 => p.align_close_terminal = true,
                    _ => p.unwind_hedge_terminal = true,
                };
                s.status = Status::RecoveringExposure;
                s.reason = format!("protected request unavailable: {error}");
                store.commit(s, now, "pre_dispatch_rejected")?;
                return Ok(());
            }
        }
    };
    if is_new {
        let p = s.pending.as_mut().unwrap();
        p.quote_wait_started_ms = None;
        match which {
            0 => p.first = Some(req.clone()),
            1 => p.hedge = Some(req.clone()),
            4 => {
                p.align_close = Some(req.clone());
            }
            2 => {
                p.repair = Some(req.clone());
                p.failed = true;
            }
            _ => {
                p.unwind_hedge = Some(req.clone());
                p.failed = true;
            }
        };
        // Write-ahead attempt marker survives a crash between send and acknowledgement.
        store.commit(s, now, "attempt_started")?;
    }
    let result = if is_new {
        workers[req.venue.index()].submit(req.clone()).await
    } else {
        workers[req.venue.index()].lookup_reconciled(req.clone(),
            venue::LookupEvidence::from_snapshot(s, &req)).await
    };
    match result {
        Ok(result) => {
            let mut next = s.clone();
            match apply(&mut next, which, &req, result) {
                Ok(()) => *s = next,
                Err(e) => {
                    s.status = Status::NeedsAttention;
                    s.reason = format!("invalid authenticated fill evidence: {e}");
                }
            }
        }
        Err(error) => {
            s.status = Status::RecoveringExposure;
            s.reason = format!("order result unknown; query only: {error}");
        }
    }
    // The signed order still expires at expires_ms. Only read-only lookup
    // continues through this grace period; no duplicate submission is allowed.
    if now
        > req
            .signed_expires_ms
            .unwrap_or(req.expires_ms)
            .saturating_add(60_000)
        && s.status == Status::RecoveringExposure
    {
        s.status = Status::NeedsAttention;
        s.reason = "order unresolved past execution deadline; reconciliation required".into();
    }
    store.commit(s, now, "order_observed")?;
    Ok(())
}

/// Background recovery after a transport timeout: look up only a persisted
/// unresolved request. This function cannot construct or submit an order.
pub async fn recheck_timed_out(
    s: &mut Snapshot,
    store: &mut Store,
    workers: &[AccountWorker; 2],
    now: u64,
) -> Result<String> {
    ensure!(
        s.status == Status::NeedsAttention
            && s.reason
                .starts_with("order unresolved past execution deadline"),
        "not a transport-timeout halt"
    );
    let p = s
        .pending
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing pending request"))?;
    let (which, r) = if !p.first_terminal {
        (0, p.first.as_ref())
    } else if p.align_close.is_some() && !p.align_close_terminal {
        (4, p.align_close.as_ref())
    } else if !p.hedge_terminal {
        (1, p.hedge.as_ref())
    } else if p.unwind_hedge.is_some() && !p.unwind_hedge_terminal {
        (3, p.unwind_hedge.as_ref())
    } else {
        (2, p.repair.as_ref())
    };
    let r = r.context("no persisted request to query")?.clone();
    let result = workers[r.venue.index()].lookup_reconciled(r.clone(),
        venue::LookupEvidence::from_snapshot(s, &r)).await?;
    let mut lookup_note = result.reason.clone();
    let mut next = s.clone();
    match apply(&mut next, which, &r, result) {
        Ok(()) => {
            let terminal = match which {
                0 => next.pending.as_ref().unwrap().first_terminal,
                1 => next.pending.as_ref().unwrap().hedge_terminal,
                2 => next.pending.as_ref().unwrap().repair_terminal,
                4 => next.pending.as_ref().unwrap().align_close_terminal,
                _ => next.pending.as_ref().unwrap().unwind_hedge_terminal,
            };
            if terminal {
                next.status = Status::Recovering;
            } else {
                next.status = Status::NeedsAttention;
                next.reason = s.reason.clone();
            }
        }
        Err(e) => {
            next = s.clone();
            next.reason = format!("invalid authenticated fill evidence: {e}");
            lookup_note = next.reason.clone();
        }
    }
    store.commit(&next, now, "timed_out_request_rechecked")?;
    *s = next;
    Ok(lookup_note)
}
