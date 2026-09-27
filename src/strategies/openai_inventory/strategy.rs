use super::*;
use anyhow::{Result, ensure};
use rust_decimal::Decimal;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SamplingProgress {
    pub continuity_active: bool,
    pub covered_ms: u64,
    pub required_ms: u64,
    pub ready: bool,
}

/// Statistical coverage only: a real observation represents at most two sample
/// periods. This never changes book/account freshness or synthesizes quotes.
pub fn sampling_progress(s: &Snapshot, now: u64) -> SamplingProgress {
    let start = now.saturating_sub(s.config.mean_window_ms);
    let mut covered_ms = 0;
    let mut previous: Option<u64> = None;
    for &(t, _) in &s.samples {
        if t > now {
            continue;
        }
        if let Some(p) = previous {
            let end = t.min(p + s.config.sample_ms * 2);
            covered_ms += end.saturating_sub(p.max(start));
        }
        previous = Some(t);
    }
    if let Some(p) = previous {
        covered_ms += now
            .min(p + s.config.sample_ms * 2)
            .saturating_sub(p.max(start));
    }
    // Once initialized, retain the established mean across maintenance gaps
    // while at least half the window of genuine recent observations remains.
    // Initial startup requires 95% of the configured window; quote freshness is separate.
    let required_ms = s.config.mean_window_ms * if s.mean_initialized { 50 } else { 95 } / 100;
    let continuity_active = covered_ms < required_ms
        && s.mean_initialized
        && s.continuity_mean
            .is_some_and(|(t, _)| now >= t && now - t <= s.config.mean_window_ms);
    SamplingProgress {
        continuity_active,
        covered_ms,
        required_ms,
        ready: covered_ms >= required_ms || continuity_active,
    }
}

pub fn reference_mean(s: &Snapshot, now: u64) -> Option<Decimal> {
    reference_mean_for(s, now, Direction::LighterLong)
}

pub fn reference_mean_for(s: &Snapshot, now: u64, direction: Direction) -> Option<Decimal> {
    let values: Vec<_> = s
        .samples
        .iter()
        .filter(|(t, _)| *t <= now && *t >= now.saturating_sub(s.config.mean_window_ms))
        .map(|(_, x)| *x * Decimal::from(direction.sign()))
        .collect();
    let rolling = (!values.is_empty())
        .then(|| values.iter().copied().sum::<Decimal>() / Decimal::from(values.len()));
    if sampling_progress(s, now).continuity_active {
        // A maintenance gap must not lower the entry threshold. Retain the
        // established mean or the current rolling estimate, whichever is higher.
        let baseline = s.continuity_mean.unwrap().1 * Decimal::from(direction.sign());
        Some(rolling.map_or(baseline, |m| m.max(baseline)))
    } else {
        rolling
    }
}

/// Opening rules and the monitor use exactly the same raw Bid−Ask history.
/// Historical non-accumulation policies retain their original midpoint rules.
pub fn entry_reference_mean_for(s: &Snapshot, now: u64, direction: Direction) -> Option<Decimal> {
    if s.config.accumulation.is_some() {
        s.entry_mean.mean(now, s.config.mean_window_ms, direction)
    } else {
        reference_mean_for(s, now, direction)
    }
}

pub fn entry_sampling_progress(s: &Snapshot, now: u64) -> SamplingProgress {
    if s.config.accumulation.is_some() {
        s.entry_mean.progress(now, s.config.mean_window_ms)
    } else {
        sampling_progress(s, now)
    }
}

pub fn observe_entry_mean(s: &mut Snapshot, books: &[Book; 2], now: u64) -> bool {
    s.entry_mean.observe(books, now, s.config.book_max_age_ms, s.config.mean_window_ms)
}

fn sample_due(s: &Snapshot, now: u64) -> bool {
    s.last_sample_ms == 0
        || (now / s.config.sample_ms > s.last_sample_ms / s.config.sample_ms
            && now.saturating_sub(s.last_sample_ms) >= s.config.sample_ms / 2)
}

/// An invalid observation must reset both independent entry clocks.
pub(super) fn clear_entry_confirmation(s: &mut Snapshot) {
    s.entry_confirmations = [None; 2];
    s.time_entry_confirmations = [None; 2];
}

/// Clear confirmation history without changing inventory, mean samples or risk latches.
pub fn clear_decision_confirmation(s: &mut Snapshot) {
    clear_entry_confirmation(s);
    s.decision_observation = None;
    s.previous_signal = None;
    s.previous_reverse_signal = None;
    s.previous_exit = None;
    s.previous_group_exit.clear();
}

fn observe_mean(s: &mut Snapshot, books: &[Book; 2], now: u64) -> Result<bool> {
    for b in books {
        b.validate(now, s.config.book_max_age_ms)?;
    }
    if !sample_due(s, now) {
        return Ok(false);
    }
    while s
        .samples
        .front()
        .is_some_and(|x| x.0 < now.saturating_sub(s.config.mean_window_ms))
    {
        s.samples.pop_front();
    }
    s.samples
        .push_back((now, books[1].mid().unwrap() - books[0].mid().unwrap()));
    s.last_sample_ms = now;
    let progress = sampling_progress(s, now);
    if progress.covered_ms >= progress.required_ms {
        s.mean_initialized = true;
        if let Some(m) = reference_mean(s, now) {
            s.continuity_mean = Some((now, m));
        }
    }
    Ok(true)
}

/// Keep real market history during account/execution halts, without generating signals.
pub fn observe_while_halted(s: &mut Snapshot, books: &[Book; 2], now: u64) -> Result<bool> {
    let entry_sampled = observe_entry_mean(s, books, now);
    if s.config.decision_ms.is_some() { clear_decision_confirmation(s); }
    let sampled = observe_mean(s, books, now)?;
    if sampled { clear_decision_confirmation(s); }
    Ok(sampled || entry_sampled)
}

// These halts have no unresolved fills or unowned inventory. Fresh account risk
// checks still precede every reducing proposal. Other attention causes stay locked.
fn recoverable_attention(s: &Snapshot) -> bool {
    s.pending.is_none()
        && (s.recovery_after_ms.is_some()
            || s.reason.starts_with("execution recovered;")
            || s.reason.starts_with("rollback completed;"))
}

/// Latch before sampling/account I/O and persist before dispatch. Never clears
/// automatically, including after a price rebound or a process restart.
pub fn enforce_loss_limit(s: &mut Snapshot, books: &[Book; 2], now: u64) -> Result<bool> {
    let mut changed = false;
    if s.loss_stop.is_none() {
        for b in books {
            b.validate(now, s.config.book_max_age_ms)?;
        }
        let pnl = s.total_pnl(books)?;
        if pnl > -s.config.max_loss_usdc {
            return Ok(false);
        }
        s.loss_stop = Some(LossStop {
            at_ms: now,
            net_pnl: pnl,
        });
        changed = true;
    }
    // A reserved entry with no durable attempt has never reached the venue.
    // Submitted/unknown first legs must still be reconciled and hedged/repaired.
    if s.pending
        .as_ref()
        .is_some_and(|op| op.action == Action::Open && op.first.is_none())
    {
        s.pending = None;
        changed = true;
    }
    let flat = s.pending.is_none() && s.positions.iter().all(|p| p.units == 0);
    let status = if s.status == Status::NeedsAttention && !recoverable_attention(s) {
        Status::NeedsAttention
    } else if flat {
        Status::Stopped
    } else if s.pending.is_none() && s.status != Status::Recovering {
        Status::Closing
    } else {
        s.status
    };
    changed |= !s.paused
        || !s.stop_after_close
        || s.close_requested == flat
        || s.stop_requested != flat
        || s.status != status;
    s.paused = true;
    s.stop_after_close = true;
    s.close_requested = !flat;
    s.stop_requested = flat;
    s.status = status;
    if changed && s.status != Status::NeedsAttention {
        s.reason = format!(
            "total loss limit {} USDC latched; protected close and stop",
            s.config.max_loss_usdc
        );
    }
    Ok(changed)
}

pub fn risk_check(
    s: &Snapshot,
    books: &[Book; 2],
    accounts: &[AccountEvidence; 2],
    now: u64,
    qty: i64,
    action: Action,
) -> Result<()> {
    ensure!(qty > 0 && qty % 10 == 0, "invalid common quantity");
    if action == Action::Open {
        ensure!(
            s.loss_stop.is_none(),
            "loss limit latched; new entries disabled"
        );
        ensure!(
            s.total_pnl(books)? > -s.config.max_loss_usdc,
            "total loss limit breached"
        );
    }
    for i in 0..2 {
        books[i].validate(now, s.config.book_max_age_ms)?;
        let a = &accounts[i];
        ensure!(a.venue.index() == i, "account venue mismatch");
        ensure!(
            &a.account
                == if i == 0 {
                    &s.config.lighter_account
                } else {
                    &s.config.entropy_account
                },
            "account binding mismatch"
        );
        ensure!(
            a.authenticated
                && a.observed_ms <= now
                && now - a.observed_ms <= s.config.account_max_age_ms,
            "account evidence stale or unauthenticated"
        );
        ensure!(a.open_orders == 0, "unresolved venue orders");
        if action == Action::Open {
            ensure!(
                a.leverage == s.config.leverage && (i == 0 || a.isolated),
                "leverage/margin mode mismatch"
            );
        }
        ensure!(
            a.position_units == s.positions[i].units,
            "unowned position change"
        );
        let side = s.direction.side(a.venue, action);
        ensure!(a.position_units == 0 || (a.position_units.signum() == side.sign()) == (action == Action::Open),
            "order side conflicts with owned position direction");
        let (px, _) = books[i].vwap(side, qty)?;
        if action == Action::Open {
            ensure!(
                quantity(qty) * px >= Decimal::from(10),
                "below venue minimum notional"
            );
            let ntl = quantity(qty) * px;
            let fee = ntl
                * if i == 0 {
                    s.config.fee_lighter
                } else {
                    s.config.fee_entropy
                };
            ensure!(
                a.free_margin - ntl / Decimal::from(s.config.leverage) - fee
                    >= s.config.min_free_margin,
                "insufficient free margin"
            );
            ensure!(
                quantity(a.position_units.abs() + qty) * px <= s.config.max_notional_per_venue,
                "inventory notional cap"
            );
        } else {
            ensure!(
                qty <= a.position_units.abs(),
                "reduce-only quantity exceeds actual position"
            );
        }
    }
    if action == Action::Open {
        ensure!(
            s.lots.len() < s.config.max_groups,
            "all grid slots occupied"
        );
    }
    Ok(())
}

/// A timed entry must remain executable at every accepted one-second observation.
fn timed_entry_candidates(s: &mut Snapshot, books: &[Book; 2], accounts: &[AccountEvidence; 2],
    now: u64, direction: Direction, mean: Decimal) -> Result<[Option<(usize, i64)>; 2]> {
    let entry = direction.top_entry(books);
    let threshold = s.config.entry_threshold(mean);
    let armed = if direction == Direction::LighterLong { &mut s.first_armed } else { &mut s.reverse_first_armed };
    if entry < threshold { *armed = true; }
    let grid_level = if let Some(anchor) = s.anchor {
        for k in 0..s.armed.len() {
            if entry < anchor + Decimal::from(k) * s.config.grid { s.armed[k] = true; }
        }
        (0..s.config.max_groups).find(|k| s.armed[*k] && !s.lots.iter().any(|l| l.level == *k))
    } else if *armed { Some(0) } else { None };
    if s.lots.len() >= s.config.max_groups { return Ok([None; 2]); }
    let qty = common_units(s.config.group_notional, (books[0].mid().unwrap() + books[1].mid().unwrap()) / Decimal::TWO)?;
    let e = execution::protected_limit(s, books, Venue::Entropy, direction.open_side(Venue::Entropy), qty, now)?;
    let l = execution::protected_limit(s, books, Venue::Lighter, direction.open_side(Venue::Lighter), qty, now)?;
    let entry = entry.min(direction.entry(books, qty)?).min((e - l) * Decimal::from(direction.sign()));
    let grid = grid_level.filter(|level| entry >= s.required_entry_spread(mean, *level));
    let time = s.config.accumulation.as_ref().and_then(|r| {
        let (at, spread) = s.last_open_completed?;
        (s.anchor.is_some() && s.time_adds_used < r.max_time_adds && now >= at
            && now - at >= r.interval_ms && entry >= spread)
            .then_some(s.config.max_groups + s.time_adds_used)
    });
    if grid.is_none() && time.is_none() { return Ok([None; 2]); }
    let original = s.direction;
    s.direction = direction;
    let risk = risk_check(s, books, accounts, now, qty, Action::Open);
    s.direction = original;
    risk?;
    Ok([grid.map(|level| (level, qty)), time.map(|level| (level, qty))])
}

fn confirmed_entry(candidate: Option<(usize, i64)>, clock: &mut Option<(u64, usize)>,
    now: u64, wait_ms: u64, lifetime_ms: u64) -> Option<(usize, i64)> {
    let Some((level, qty)) = candidate else { *clock = None; return None; };
    if let Some((at, _)) = clock.filter(|(at, old_level)|
        *old_level == level && now >= *at && now - *at <= lifetime_ms) {
        if now - at >= wait_ms { return Some((level, qty)); }
    } else { *clock = Some((now, level)); }
    None
}

/// Return a durable proposal, never a venue call. Sample by time, not message count.
pub fn evaluate(
    s: &mut Snapshot,
    books: &[Book; 2],
    accounts: &[AccountEvidence; 2],
    now: u64,
) -> Result<Option<Operation>> {
    observe_entry_mean(s, books, now);
    let result = evaluate_inner(s, books, accounts, now);
    // Errors can occur before candidate selection (for example while valuing
    // held positions against thin books). No entry clock may bridge that gap.
    if result.is_err() { clear_entry_confirmation(s); }
    result
}

fn evaluate_inner(
    s: &mut Snapshot,
    books: &[Book; 2],
    accounts: &[AccountEvidence; 2],
    now: u64,
) -> Result<Option<Operation>> {
    for b in books {
        if let Err(error) = b.validate(now, s.config.book_max_age_ms) {
            // Timed entry experiments cannot bridge a gap with unverifiable prices.
            if s.config.entry_confirmation_ms.is_some() { clear_entry_confirmation(s); }
            // An aged cached book cannot trigger an order, but its earlier fresh
            // observation remains valid evidence until the original timeout.
            // A disconnect or backwards/future timestamp breaks continuity.
            if s.config.decision_ms.is_some() && (!b.connected || b.received_ms > now) {
                clear_decision_confirmation(s);
            }
            return Err(error);
        }
    }
    enforce_loss_limit(s, books, now)?;
    if let Ok(pnl) = s.total_pnl(books) {
        s.peak_pnl = s.peak_pnl.max(pnl);
        s.max_drawdown = s.max_drawdown.max(s.peak_pnl - pnl);
    }
    let margin_emergency = s.paired_units() > 0
        && accounts.iter().enumerate().any(|(i, a)| {
            a.authenticated
                && now.saturating_sub(a.observed_ms) <= s.config.account_max_age_ms
                && (a.equity <= Decimal::ZERO
                    || a.liquidation_price.is_some_and(|p| {
                        p > Decimal::ZERO
                            && books[i].mid().is_some_and(|mid| {
                                let distance = (mid - p) * Decimal::from(a.position_units.signum());
                                distance <= mid * Decimal::new(5, 2)
                            })
                    }))
        });
    if s.pending.is_some()
        || matches!(s.status, Status::Recovering | Status::RecoveringExposure)
        || (s.status == Status::NeedsAttention && !(margin_emergency && recoverable_attention(s)))
        || (s.status == Status::Stopped && !margin_emergency)
    {
        return Ok(None);
    }
    if margin_emergency {
        s.status = Status::Closing;
        s.close_requested = true;
        s.paused = true;
        s.stop_after_close = true;
        s.reason = "liquidation distance reserve breached; protected exit requested".into();
    }
    if !margin_emergency && !s.close_requested {
        let due = s.config.decision_ms.map_or_else(|| sample_due(s, now), |interval| {
            s.decision_observation.is_none_or(|(at, _)| now >= at && now - at >= interval)
        });
        // Statistical observations retain their original cadence even when a
        // decision is not due, or only one of the two venue books has advanced.
        let new_books = s.config.decision_ms.is_none() || s.decision_observation.is_none_or(|(_, previous)| {
            books.iter().zip(previous).all(|(b, at)| b.received_ms > at)
        });
        if !due || !new_books {
            if s.config.decision_ms.is_some() { observe_mean(s, books, now)?; }
            return Ok(None);
        }
    }
    while s
        .samples
        .front()
        .is_some_and(|x| x.0 < now.saturating_sub(s.config.mean_window_ms))
    {
        s.samples.pop_front();
    }
    let progress = sampling_progress(s, now);
    let warm = progress.ready;
    if progress.covered_ms >= progress.required_ms {
        if let Some(m) = reference_mean(s, now) {
            s.continuity_mean = Some((now, m));
        }
    }
    if warm {
        s.mean_initialized = true;
    }
    let mean = if warm { reference_mean(s, now) } else { None };
    let mid = books[1].mid().unwrap() - books[0].mid().unwrap();
    let entry = books[1].bids[0].price - books[0].asks[0].price;
    let funding_ready = s.config.mode == Mode::Paper
        || (s.funding_synced_ms > 0 && now.saturating_sub(s.funding_synced_ms) <= 90_000);
    let previous = [s.previous_signal, s.previous_reverse_signal];
    let exit_means = [mean, warm.then(|| reference_mean_for(s, now, Direction::LighterShort)).flatten()];
    let entry_progress = entry_sampling_progress(s, now);
    let means = [Direction::LighterLong, Direction::LighterShort].map(|d|
        entry_progress.ready.then(|| entry_reference_mean_for(s, now, d)).flatten());
    let sampled = sample_due(s, now);
    if sampled {
        s.samples.push_back((now, mid));
        s.last_sample_ms = now;
    }
    if sampled || s.config.decision_ms.is_some() {
        s.previous_signal = means[0].map(|m| (now, entry, m));
        s.previous_reverse_signal = means[1].map(|m| (now, Direction::LighterShort.top_entry(books), m));
    }
    if s.config.decision_ms.is_some() {
        s.decision_observation = Some((now, books.each_ref().map(|b| b.received_ms)));
    }
    // A new opening MA warming up must not prevent profit-taking on existing lots.
    let per_group_accumulation = s.config.accumulation.is_some() && s.config.exit_policy == ExitPolicy::PerGroup;
    if ((!warm && !per_group_accumulation) || (!entry_progress.ready && s.paired_units() == 0)) && !s.close_requested {
        clear_entry_confirmation(s);
        s.status = if s.paused {
            Status::PausedEntries
        } else {
            Status::Warming
        };
        let progress = if s.config.accumulation.is_some() { entry_progress } else { progress };
        s.reason = format!(
            "warming trailing mean: coverage {} / {} ms",
            progress.covered_ms, progress.required_ms
        );
        return Ok(None);
    }
    let mean = exit_means[s.direction.long()].unwrap_or(mid * Decimal::from(s.direction.sign()));
    if s.status == Status::Warming {
        s.status = Status::Running;
    }
    let held = s.paired_units();
    let mut action = None;
    let mut close_lot_id = None;
    let mut close_allocations = Vec::new();
    if held > 0 {
        if s.config.exit_policy == ExitPolicy::PerGroup && !s.close_requested {
            let previous = std::mem::take(&mut s.previous_group_exit);
            let mut confirmed = Vec::new();
            for lot in &s.lots {
                let mut qty = common_units(s.config.close_slice_notional, books[0].mid().unwrap())?.min(lot.units);
                if s.config.shared_exit_conditions && lot.units-qty>0
                    && quantity(lot.units-qty)*books[0].mid().unwrap()<Decimal::from(10) {qty=lot.units;}
                let eligible = funding_ready && if s.config.shared_exit_conditions {
                    shared_profit_exit_eligible(s,Some(lot),books,qty,mean,now).unwrap_or(false)
                } else {group_exit_eligible(s, lot, books, qty, now).unwrap_or(false)};
                if eligible { s.previous_group_exit.insert(lot.id.clone(), now); }
                if eligible && previous.get(&lot.id)
                    .is_some_and(|t| now > *t && now - *t <= s.config.confirmation_window_ms()) {
                    confirmed.push(CloseAllocation { lot_id: lot.id.clone(), units: qty });
                }
            }
            close_allocations = select_close_batch(s, books, &confirmed, now)?;
            if let Some(first) = close_allocations.first() {
                let level = s.lots.iter().find(|l| l.id == first.lot_id).unwrap().level;
                let qty = close_allocations.iter().map(|a| a.units).sum();
                action = Some((Action::Close, level, qty));
                if close_allocations.len() == 1 { close_lot_id = Some(first.lot_id.clone()); }
            }
        } else {
        let close_price = s.direction.exit(books, held).ok();
        let profitable = s
            .remaining_net(books)
            .is_ok_and(|net| net > s.config.exit_profit_reserve);
        // First trigger freezes the batch. Each subsequent slice is independently repriced.
        let eligible = funding_ready && if s.config.shared_exit_conditions {
            shared_profit_exit_eligible(s, None, books, held, mean, now).unwrap_or(false)
        } else { close_price.is_some_and(|p| p <= mean) && profitable };
        let normal_exit = eligible
            && s.previous_exit
                .is_some_and(|(t, ok)| ok && now.saturating_sub(t) <= s.config.confirmation_window_ms());
        s.previous_exit = Some((now, eligible));
        if normal_exit {
            s.exit_batch_active = true;
        }
        let normal_exit = s.exit_batch_active && eligible;
        if s.close_requested || normal_exit {
            let mut qty =
                common_units(s.config.close_slice_notional, books[0].mid().unwrap())?.min(held);
            if held - qty > 0 && quantity(held - qty) * books[0].mid().unwrap() < Decimal::from(10)
            {
                qty = held;
            }
            if qty > 0 {
                action = Some((Action::Close, 0, qty));
            }
        }
        }
    }
    if action.is_none()
        && funding_ready && !s.paused && !s.close_requested && !s.exit_batch_active
        && s.entry_attempts_remaining != Some(0)
    {
        for (idx, direction) in [Direction::LighterLong, Direction::LighterShort].into_iter().enumerate() {
            if (direction == Direction::LighterShort && s.config.direction_policy != DirectionPolicy::Both)
                || (held > 0 && direction != s.direction) {
                s.entry_confirmations[idx] = None;
                s.time_entry_confirmations[idx] = None;
                continue;
            }
            if let Some(wait_ms) = s.config.entry_confirmation_ms {
                let candidates = means[idx].map(|mean| timed_entry_candidates(s, books, accounts, now, direction, mean)).transpose();
                let candidates = match candidates {
                    Ok(candidates) => candidates.unwrap_or([None; 2]),
                    Err(error) => { clear_entry_confirmation(s); return Err(error); }
                };
                let lifetime = s.config.confirmation_window_ms();
                let grid = confirmed_entry(candidates[0], &mut s.entry_confirmations[idx], now, wait_ms, lifetime);
                let time = confirmed_entry(candidates[1], &mut s.time_entry_confirmations[idx], now, wait_ms, lifetime);
                // Prefer a confirmed grid entry, but an unconfirmed grid must not
                // discard a ready time addition. Reserve exactly one operation.
                if let Some((level, qty)) = grid.or(time) {
                    s.direction = direction;
                    action = Some((Action::Open, level, qty));
                    break;
                }
                continue;
            }
            let Some((t, prev_entry, prev_mean)) = previous[idx] else { continue; };
            if now < t || now - t > s.config.confirmation_window_ms() { continue; }
            let Some(mean) = means[idx] else { continue; };
            let threshold = s.config.entry_threshold(mean);
            let entry = direction.top_entry(books);
            let armed = if direction == Direction::LighterLong { &mut s.first_armed } else { &mut s.reverse_first_armed };
            if entry < threshold { *armed = true; }
            let level = if let Some(anchor) = s.anchor {
                for k in 0..s.armed.len() {
                    if entry < anchor + Decimal::from(k) * s.config.grid { s.armed[k] = true; }
                }
                (0..s.config.max_groups).find(|k| s.armed[*k] && !s.lots.iter().any(|l| l.level == *k))
            } else if *armed { Some(0) } else { None };
            let Some(level) = level else { continue; };
            let grid_threshold = s.anchor.map(|a| a + Decimal::from(level) * s.config.grid).unwrap_or(threshold);
            let required = threshold.max(grid_threshold).max(Decimal::ZERO);
            if entry < required || prev_entry < s.config.entry_threshold(prev_mean).max(grid_threshold) { continue; }
            let qty = common_units(s.config.group_notional, (books[0].mid().unwrap() + books[1].mid().unwrap()) / Decimal::TWO)?;
            if direction.entry(books, qty)? >= required {
                s.direction = direction;
                action = Some((Action::Open, level, qty));
                break;
            }
        }
    } else { clear_entry_confirmation(s); }
    let Some((action, level, qty)) = action else {
        s.reason = if funding_ready {
            "waiting for executable spread"
        } else {
            "funding reconciliation required; automatic entry/profit exit paused"
        }
        .into();
        return Ok(None);
    };
    let mean = means[s.direction.long()].unwrap_or(mid * Decimal::from(s.direction.sign()));
    risk_check(s, books, accounts, now, qty, action)?;
    if action == Action::Open {
        let protected = execution::protected_limit(s, books, Venue::Entropy, s.direction.open_side(Venue::Entropy), qty, now)
            .and_then(|e| {
                execution::protected_limit(s, books, Venue::Lighter, s.direction.open_side(Venue::Lighter), qty, now)
                    .map(|l| (e - l) * Decimal::from(s.direction.sign()))
            });
        let required = s.required_entry_spread(mean,level);
        match protected {
            Ok(spread) if spread >= required => {}
            Ok(_) => {
                s.reason="waiting: protected entry limits do not cover spread threshold; test budget retained".into();
                return Ok(None);
            }
            Err(error) => {
                s.reason = format!("waiting: {error}; test budget retained");
                return Ok(None);
            }
        }
    }
    if action == Action::Open && s.lots.is_empty() {
        s.round_start_pnl = s.total_pnl(books)?;
    }
    if action == Action::Open {
        if let Some(remaining) = &mut s.entry_attempts_remaining {
            ensure!(*remaining > 0, "entry test budget exhausted");
            *remaining -= 1;
        }
    }
    s.sequence += 1;
    clear_entry_confirmation(s);
    let id = format!("openai-{}-{}", s.instance_id, s.sequence);
    Ok(Some(Operation {
        close_allocations,
        close_lot_id,
        align_close: None,
        align_close_terminal: false,
        align_close_filled: 0,
        first_venue: if action == Action::Open {
            Venue::Entropy
        } else {
            Venue::Lighter
        },
        min_entry_spread: if action == Action::Open {
            Some(s.required_entry_spread(mean,level))
        } else {
            None
        },
        unwind_hedge: None,
        unwind_hedge_terminal: false,
        quote_wait_started_ms: None,
        unwind_hedge_filled: 0,
        id,
        action,
        level,
        requested_units: qty,
        created_ms: now,
        first: None,
        hedge: None,
        repair: None,
        repair_attempt: 0,
        repair_retry_after_ms: None,
        first_terminal: false,
        hedge_terminal: false,
        repair_terminal: false,
        first_filled: 0,
        hedge_filled: 0,
        repair_filled: 0,
        first_value: Decimal::ZERO,
        hedge_value: Decimal::ZERO,
        failed: false,
    }))
}

/// Each group uses its actual opening cash flows and both protected exit prices.
/// Missing legacy costs block this optional paper-only policy rather than inventing fees.
pub fn group_exit_eligible(s: &Snapshot, lot: &Lot, books: &[Book; 2], qty: i64, now: u64) -> Result<bool> {
    ensure!(qty > 0 && qty <= lot.units, "invalid group exit size");
    if s.config.shared_exit_conditions {
        let mean = if s.config.accumulation.is_some() { Decimal::ZERO } else {
            reference_mean_for(s,now,s.direction).ok_or_else(|| anyhow::anyhow!("missing close reference mean"))?
        };
        return shared_profit_exit_eligible(s,Some(lot),books,qty,mean,now);
    }
    let entry_net = lot.entry_net_spread.ok_or_else(|| anyhow::anyhow!("group opening costs unavailable"))?;
    let long = s.direction.long(); let short = s.direction.short();
    let prices = [Venue::Lighter, Venue::Entropy].map(|v|
        execution::protected_limit(s, books, v, s.direction.side(v, Action::Close), qty, now));
    let [l, e] = prices; let prices = [l?, e?];
    let exit_spread = prices[short] - prices[long];
    let exit_fees = quantity(qty) * (prices[0]*s.config.fee_lighter + prices[1]*s.config.fee_entropy);
    let net = quantity(qty) * (entry_net - exit_spread) - exit_fees;
    Ok(lot.entry_spread - exit_spread >= s.config.group_take_profit && net > s.config.exit_profit_reserve)
}

/// Select whole candidate slices in lot order against shared depth, not the same
/// liquidity once per group. Only individually confirmed lots may be candidates.
pub fn select_close_batch(s: &Snapshot, books: &[Book; 2], candidates: &[CloseAllocation], now: u64)
    -> Result<Vec<CloseAllocation>> {
    let mut selected = Vec::new();
    let mut total = 0_i64;
    let mut seen = std::collections::BTreeSet::new();
    for allocation in candidates {
        ensure!(allocation.units > 0 && allocation.units % 10 == 0 && seen.insert(&allocation.lot_id),
            "invalid or duplicate batch exit allocation");
        let lot = s.lots.iter().find(|l| l.id == allocation.lot_id)
            .ok_or_else(|| anyhow::anyhow!("selected group missing"))?;
        ensure!(allocation.units <= lot.units, "batch exit exceeds selected group inventory");
        if !group_exit_eligible(s, lot, books, allocation.units, now).unwrap_or(false) { continue; }
        let combined = total.checked_add(allocation.units).ok_or_else(|| anyhow::anyhow!("batch quantity overflow"))?;
        if [Venue::Lighter, Venue::Entropy].into_iter().all(|v|
            execution::protected_limit(s, books, v, s.direction.side(v, Action::Close), combined, now).is_ok()) {
            selected.push(allocation.clone());
            total = combined;
        }
    }
    Ok(selected)
}

/// Reprice only before creating the first request. Once a request exists its
/// quantity/ownership must remain immutable for reconciliation and retry.
pub fn refresh_unsent_close_batch(s: &mut Snapshot, books: &[Book; 2], now: u64) -> Result<bool> {
    let Some(op) = &s.pending else { return Ok(false); };
    if op.action != Action::Close || op.first.is_some() || op.close_allocations.is_empty() { return Ok(false); }
    let selected = select_close_batch(s, books, &op.close_allocations, now)?;
    if selected == op.close_allocations { return Ok(false); }
    let op = s.pending.as_mut().unwrap();
    op.requested_units = selected.iter().map(|a| a.units).sum();
    op.close_lot_id = if selected.len() == 1 { Some(selected[0].lot_id.clone()) } else { None };
    op.close_allocations = selected;
    Ok(true)
}

pub fn normal_exit_eligible(s: &Snapshot, books: &[Book; 2], now: u64) -> Result<bool> {
    if let Some(op) = &s.pending {
        if op.requested_units <= 0 { return Ok(false); }
        if !op.close_allocations.is_empty() {
            ensure!(s.config.exit_policy == ExitPolicy::PerGroup, "group exit policy mismatch");
            ensure!(op.close_allocations.iter().map(|a| a.units).sum::<i64>() == op.requested_units,
                "batch exit reservation size mismatch");
            return Ok(select_close_batch(s, books, &op.close_allocations, now)? == op.close_allocations);
        }
        if let Some(id) = &op.close_lot_id {
            ensure!(s.config.exit_policy == ExitPolicy::PerGroup, "group exit policy mismatch");
            let lot=s.lots.iter().find(|l| &l.id==id).ok_or_else(|| anyhow::anyhow!("selected group missing"))?;
            return group_exit_eligible(s, lot, books, op.requested_units, now);
        }
    }
    let mean=reference_mean_for(s,now,s.direction).ok_or_else(|| anyhow::anyhow!("missing close reference mean"))?;
    if s.config.shared_exit_conditions {
        return Ok(sampling_progress(s,now).ready && shared_profit_exit_eligible(s,None,books,s.paired_units(),mean,now)?);
    }
    Ok(sampling_progress(s,now).ready && s.direction.exit(books,s.paired_units())? <= mean
        && s.remaining_net(books)? > s.config.exit_profit_reserve)
}

/// Identical price, cost and threshold model. Only the inventory being evaluated differs.
/// `None` means all remaining lots; a selected lot uses its own opening cash flow.
fn shared_profit_exit_eligible(s: &Snapshot, lot: Option<&Lot>, books: &[Book;2], qty:i64, mean:Decimal, now:u64) -> Result<bool> {
    ensure!(qty>0,"empty exit inventory");
    let opening = if let Some(lot)=lot {
        ensure!(qty<=lot.units,"invalid group exit size");
        quantity(qty)*lot.entry_net_spread.ok_or_else(|| anyhow::anyhow!("group opening costs unavailable"))?
    } else {
        ensure!(qty==s.paired_units(),"round exit must evaluate all remaining groups");
        s.lots.iter().map(|lot|Ok(quantity(lot.units)*lot.entry_net_spread
            .ok_or_else(|| anyhow::anyhow!("group opening costs unavailable"))?)).collect::<Result<Vec<Decimal>>>()?.into_iter().sum()
    };
    let [l,e]=[Venue::Lighter,Venue::Entropy].map(|v|
        execution::protected_limit(s,books,v,s.direction.side(v,Action::Close),qty,now));
    let prices=[l?,e?];
    let exit_spread=prices[s.direction.short()]-prices[s.direction.long()];
    let fees=quantity(qty)*(prices[0]*s.config.fee_lighter+prices[1]*s.config.fee_entropy);
    let net=opening-quantity(qty)*exit_spread-fees;
    let target_met = lot.is_none_or(|lot| {
        let target=s.config.accumulation.as_ref().map_or(s.config.group_take_profit,
            |r|(lot.entry_spread*r.contraction_ratio).max(s.config.group_take_profit));
        lot.entry_spread-exit_spread>=target
    });
    // Accumulation replaces the MA exit only for individual lots. A round still
    // uses its original mean/positive-net gate, including every subsequent slice.
    Ok(((lot.is_some() && s.config.accumulation.is_some()) || exit_spread<=mean)
        && net>Decimal::ZERO && target_met)
}
