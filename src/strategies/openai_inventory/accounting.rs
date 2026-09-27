//! Reporting only. Funding attribution never participates in order/exit decisions.
use super::*;
use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

fn key(f: &Funding) -> String { format!("{:?}:{}", f.venue, f.id) }

/// Backfilling an old ID adds its detail, never its cash flow a second time.
pub fn record_funding(s: &mut Snapshot, f: Funding) -> Result<()> {
    ensure!(!f.id.is_empty() && f.time_ms > 0, "invalid funding identity/time");
    let id = key(&f);
    if let Some(old) = s.funding_records.get(&id) {
        ensure!(old == &f, "duplicate funding id has different payload");
        ensure!(s.funding_ids.contains(&id), "funding record missing deduplication id");
        return Ok(());
    }
    if s.funding_ids.insert(id.clone()) {
        s.positions[f.venue.index()].funding += f.amount;
    }
    s.funding_records.insert(id, f);
    Ok(())
}

pub fn funding_history_complete(s: &Snapshot) -> bool {
    if s.funding_records.len() != s.funding_ids.len()
        || s.funding_records.iter().any(|(id, f)| !s.funding_ids.contains(id) || *id != key(f)) {
        return false;
    }
    [Venue::Lighter, Venue::Entropy].into_iter().all(|v|
        s.funding_records.values().filter(|f| f.venue == v).map(|f| f.amount).sum::<Decimal>()
            == s.positions[v.index()].funding)
}

#[derive(Debug, Clone, Serialize)]
pub struct LotNetProfit {
    pub lot_id: String,
    /// Lifetime settled funding, including the portion attributable to units already closed.
    pub settled_funding: Option<Decimal>,
    pub remaining_funding: Option<Decimal>,
    /// Open-position estimate after entry costs, close fees/protection and remaining funding.
    pub estimated_exit_net: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetProfitAccounting {
    /// Completed paired-close portions, using actual fills/fees and their settled funding.
    /// None when the ledger cannot establish the full attribution; excludes open exposure.
    pub closed_net_profit: Option<Decimal>,
    pub settled_funding: Decimal,
    pub funding_complete: bool,
    /// Failed operations/unassignable history; stays in total PnL, never charged to later lots.
    pub unallocated_funding: Decimal,
    pub lots: Vec<LotNetProfit>,
}

#[derive(Default, Clone)]
struct Bucket {
    units: i64,
    total: Decimal,
    remaining: Decimal,
    opening_units: i64,
    closing_units: i64,
    opening_cash: Decimal,
    closing_cash: Decimal,
}
impl Bucket {
    fn fill(&mut self, signed: i64) {
        if self.units != 0 && signed.signum() != self.units.signum() {
            let closed = signed.abs().min(self.units.abs());
            // Round the still-held portion once; the released portion absorbs the remainder.
            self.remaining = self.remaining * Decimal::from(self.units.abs() - closed)
                / Decimal::from(self.units.abs());
        }
        self.units += signed;
    }
}

#[derive(Default)]
struct Attribution {
    complete: bool,
    groups: BTreeMap<String, [Bucket; 2]>,
    unallocated: Decimal,
}

fn owns(order: &str, operation: &str) -> bool {
    order == operation || order.starts_with(&format!("{operation}-v2-"))
}

/// Replay settlement-time exposure, not the current group's size or fetch time.
/// Actual batch-close allocations are consumed in their recorded order, per venue.
fn attribute(s: &Snapshot) -> Attribution {
    let mut ids: BTreeSet<String> = s.lots.iter().map(|l| l.id.clone()).collect();
    ids.extend(s.closed_lot_allocations.values().flatten().map(|a| a.lot_id.clone()));
    let mut result = Attribution { complete: funding_history_complete(s)
        && s.pending.is_none() && s.live_orphan.is_none(), ..Default::default() };
    for id in &ids { result.groups.insert(id.clone(), Default::default()); }
    let mut other: [Bucket; 2] = Default::default();
    let mut closed_used: BTreeMap<(String, usize), i64> = BTreeMap::new();
    let mut fills: Vec<_> = s.fills.values().collect();
    fills.sort_by(|a, b| (a.time_ms, &a.id).cmp(&(b.time_ms, &b.id)));
    let mut funding: Vec<_> = s.funding_records.values().collect();
    funding.sort_by(|a, b| (a.time_ms, &a.id).cmp(&(b.time_ms, &b.id)));
    let mut cursor = 0;
    for f in funding {
        while cursor < fills.len() && fills[cursor].time_ms < f.time_ms {
            apply_fill(fills[cursor], s, &ids, &mut result, &mut other, &mut closed_used);
            cursor += 1;
        }
        let i = f.venue.index();
        // Without venue sequence numbers, a fill in the settlement millisecond is ambiguous.
        let boundary_ambiguous = fills[cursor..].iter().take_while(|x| x.time_ms == f.time_ms)
            .any(|x| x.venue == f.venue);
        let total = other[i].units + result.groups.values().map(|b| b[i].units).sum::<i64>();
        let mixed = result.groups.values().map(|b| b[i].units).chain([other[i].units])
            .any(|u| u != 0 && u.signum() != total.signum());
        if f.amount.is_zero() { continue; }
        if boundary_ambiguous || total == 0 || mixed {
            result.complete = false;
            result.unallocated += f.amount;
            continue;
        }
        let active: Vec<_> = result.groups.iter().filter(|(_, b)| b[i].units != 0)
            .map(|(id, b)| (id.clone(), b[i].units.abs())).collect();
        let mut left = f.amount;
        for (n, (id, units)) in active.iter().enumerate() {
            let part = if n + 1 == active.len() && other[i].units == 0 { left }
                else { f.amount * Decimal::from(*units) / Decimal::from(total.abs()) };
            let bucket = &mut result.groups.get_mut(id).unwrap()[i];
            bucket.total += part;
            bucket.remaining += part;
            left -= part;
        }
        other[i].total += left;
        other[i].remaining += left;
    }
    for f in &fills[cursor..] {
        apply_fill(f, s, &ids, &mut result, &mut other, &mut closed_used);
    }
    for i in 0..2 {
        let total = other[i].units + result.groups.values().map(|b| b[i].units).sum::<i64>();
        if total != s.positions[i].units { result.complete = false; }
        for (id, buckets) in &result.groups {
            let expected = s.lots.iter().find(|l| &l.id == id).map_or(0, |l|
                l.units * s.direction.open_side(if i == 0 { Venue::Lighter } else { Venue::Entropy }).sign());
            if buckets[i].units != expected { result.complete = false; }
        }
    }
    result.unallocated += other.iter().map(|b| b.total).sum::<Decimal>();
    // Missing historical rows remain visible in the total, not silently assigned as zero.
    result.unallocated += s.positions.iter().map(|p| p.funding).sum::<Decimal>()
        - s.funding_records.values().map(|f| f.amount).sum::<Decimal>();
    result
}

fn apply_fill(f: &Fill, s: &Snapshot, ids: &BTreeSet<String>, result: &mut Attribution,
    other: &mut [Bucket; 2], used: &mut BTreeMap<(String, usize), i64>) {
    let i = f.venue.index();
    let cash = -s.config.quantity(f.units) * f.price * Decimal::from(f.side.sign()) - f.fee;
    if let Some(id) = ids.iter().find(|id| owns(&f.order_id, id)) {
        let b = &mut result.groups.get_mut(id).unwrap()[i];
        b.opening_units += f.units * f.side.sign();
        b.opening_cash += cash;
        b.fill(f.units * f.side.sign());
        return;
    }
    if let Some((op, allocations)) = s.closed_lot_allocations.iter().find(|(op, _)| owns(&f.order_id, op)) {
        let offset = used.entry((op.clone(), i)).or_default();
        let mut skip = *offset;
        let mut left = f.units;
        let mut cash_left = cash;
        for a in allocations {
            let available = a.units - skip.min(a.units);
            skip = (skip - a.units).max(0);
            let take = left.min(available);
            if take > 0 {
                let b = &mut result.groups.get_mut(&a.lot_id).unwrap()[i];
                if b.units == 0 || b.units.signum() == f.side.sign() || take > b.units.abs() {
                    result.complete = false;
                }
                // The final allocation absorbs decimal division residue, preserving cash.
                let part = if take == left { cash_left }
                    else { cash * Decimal::from(take) / Decimal::from(f.units) };
                b.closing_units += take * f.side.sign();
                b.closing_cash += part;
                cash_left -= part;
                b.fill(take * f.side.sign());
                left -= take;
            }
        }
        *offset += f.units;
        if left > 0 { result.complete = false; other[i].fill(left * f.side.sign()); }
    } else {
        other[i].fill(f.units * f.side.sign());
    }
}

fn closed_net_profit(s: &Snapshot, a: &Attribution) -> Option<Decimal> {
    if !a.complete { return None; }
    let mut closed: BTreeMap<&str, i64> = BTreeMap::new();
    for allocation in s.closed_lot_allocations.values().flatten() {
        if allocation.units <= 0 { return None; }
        let total = closed.entry(&allocation.lot_id).or_default();
        *total = total.checked_add(allocation.units)?;
    }
    // Old ledgers may count closed groups without retaining their allocation records.
    let complete_groups = closed.keys().filter(|id| !s.lots.iter().any(|l| l.id == **id)).count();
    if (complete_groups as u64) < s.closed_groups { return None; }
    let mut net = Decimal::ZERO;
    for (id, closed_units) in closed {
        let remaining = s.lots.iter().find(|l| l.id == id).map_or(0, |l| l.units);
        let original = remaining.checked_add(closed_units)?;
        let buckets = a.groups.get(id)?;
        if original <= 0 || buckets[0].opening_units != -buckets[1].opening_units {
            return None;
        }
        let mut opening_cash = Decimal::ZERO;
        for b in buckets {
            if b.opening_units.abs() != original
                || b.closing_units != -b.opening_units.signum() * closed_units {
                return None;
            }
            opening_cash += b.opening_cash;
            net += b.closing_cash + b.total - b.remaining;
        }
        // Allocate entry fees/repair costs by the actual quantity closed, not account average.
        net += opening_cash * Decimal::from(closed_units) / Decimal::from(original);
    }
    Some(net)
}

#[derive(PartialEq)]
struct CacheKey {
    records: usize,
    ids: usize,
    fills: usize,
    closes: usize,
    lots: Vec<(String, i64)>,
    units: [i64; 2],
    funding: [Decimal; 2],
    unresolved: bool,
}

#[derive(Default)]
pub struct AccountingCache {
    key: Option<CacheKey>,
    attribution: Attribution,
}
impl AccountingCache {
    pub fn report(&mut self, s: &Snapshot, books: &[Book; 2], now: u64) -> NetProfitAccounting {
        let key = CacheKey { records: s.funding_records.len(), ids: s.funding_ids.len(),
            fills: s.fills.len(), closes: s.closed_lot_allocations.len(),
            lots: s.lots.iter().map(|l| (l.id.clone(), l.units)).collect(),
            units: s.positions.each_ref().map(|p| p.units),
            funding: s.positions.each_ref().map(|p| p.funding),
            unresolved: s.pending.is_some() || s.live_orphan.is_some() };
        if self.key.as_ref() != Some(&key) {
            self.attribution = attribute(s);
            self.key = Some(key);
        }
        let a = &self.attribution;
        NetProfitAccounting {
            closed_net_profit: closed_net_profit(s, a),
            settled_funding: s.positions.iter().map(|p| p.funding).sum(),
            funding_complete: a.complete,
            unallocated_funding: a.unallocated,
            lots: s.lots.iter().map(|lot| {
                let b = a.groups.get(&lot.id);
                let total = a.complete.then(|| b.map_or(Decimal::ZERO, |b| b.iter().map(|x| x.total).sum()));
                let remaining = a.complete.then(|| b.map_or(Decimal::ZERO, |b| b.iter().map(|x| x.remaining).sum()));
                LotNetProfit { lot_id: lot.id.clone(), settled_funding: total, remaining_funding: remaining,
                    estimated_exit_net: remaining.and_then(|funding| estimate_lot(s, lot, books, now, funding).ok()) }
            }).collect(),
        }
    }
}

fn estimate_lot(s: &Snapshot, lot: &Lot, books: &[Book; 2], now: u64, funding: Decimal) -> Result<Decimal> {
    let opening = lot.entry_net_spread.ok_or_else(|| anyhow::anyhow!("opening costs unavailable"))?;
    let mut net = s.config.quantity(lot.units) * opening + funding;
    for venue in [Venue::Lighter, Venue::Entropy] {
        let book = &books[venue.index()];
        book.validate(now, s.config.book_max_age_ms)?;
        let side = s.direction.side(venue, Action::Close);
        let (vwap, _) = book.vwap(side, lot.units)?;
        let price = vwap * (Decimal::ONE + Decimal::from(side.sign())
            * s.config.execution_slippage_bps / Decimal::from(10_000));
        let rate = if venue == Venue::Lighter { s.config.fee_lighter } else { s.config.fee_entropy };
        net -= s.config.quantity(lot.units) * price * (Decimal::from(side.sign()) + rate);
    }
    Ok(net)
}
