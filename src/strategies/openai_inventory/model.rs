use super::InventoryConfig;
use anyhow::{Result, bail, ensure};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Legacy OPENAI fixture conversion. Runtime code uses InventoryConfig.market.
pub fn quantity(units: i64) -> Decimal {
    Decimal::new(units, 4)
}
pub fn units(q: Decimal) -> Result<i64> {
    let v = q * Decimal::from(10_000);
    ensure!(v.fract().is_zero(), "quantity precision loss");
    v.to_i64()
        .ok_or_else(|| anyhow::anyhow!("quantity overflow"))
}
pub fn common_units(notional: Decimal, price: Decimal) -> Result<i64> {
    ensure!(price > Decimal::ZERO, "invalid price");
    Ok(((notional / price * Decimal::from(1000))
        .floor()
        .to_i64()
        .ok_or_else(|| anyhow::anyhow!("size overflow"))?)
        * 10)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Lighter,
    Entropy,
}
impl Venue {
    pub fn index(self) -> usize {
        if self == Self::Lighter { 0 } else { 1 }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}
impl Side {
    pub fn opposite(self) -> Self {
        if self == Self::Buy { Self::Sell } else { Self::Buy }
    }
    pub fn sign(self) -> i64 {
        if self == Self::Buy { 1 } else { -1 }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionPolicy {
    #[default]
    LighterLongOnly,
    Both,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    LighterLong,
    LighterShort,
}
impl Direction {
    pub fn sign(self) -> i64 { if self == Self::LighterLong { 1 } else { -1 } }
    pub fn open_side(self, venue: Venue) -> Side {
        if (venue == Venue::Lighter) == (self == Self::LighterLong) { Side::Buy } else { Side::Sell }
    }
    pub fn side(self, venue: Venue, action: Action) -> Side {
        let side = self.open_side(venue);
        if action == Action::Open { side } else { side.opposite() }
    }
    pub fn long(self) -> usize { if self == Self::LighterLong { 0 } else { 1 } }
    pub fn short(self) -> usize { 1 - self.long() }
    pub fn top_entry(self, books: &[Book; 2]) -> Decimal {
        books[self.short()].bids[0].price - books[self.long()].asks[0].price
    }
    pub fn entry(self, books: &[Book; 2], qty: i64) -> Result<Decimal> {
        Ok(books[self.short()].vwap(Side::Sell, qty)?.0 - books[self.long()].vwap(Side::Buy, qty)?.0)
    }
    pub fn exit(self, books: &[Book; 2], qty: i64) -> Result<Decimal> {
        Ok(books[self.short()].vwap(Side::Buy, qty)?.0 - books[self.long()].vwap(Side::Sell, qty)?.0)
    }
}
fn default_armed() -> bool { true }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Level {
    pub price: Decimal,
    pub units: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Book {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub received_ms: u64,
    pub connected: bool,
}
impl Book {
    pub fn validate(&self, now: u64, ttl: u64) -> Result<()> {
        ensure!(
            self.connected && self.received_ms <= now && now - self.received_ms <= ttl,
            "book disconnected or stale"
        );
        ensure!(!self.bids.is_empty() && !self.asks.is_empty(), "empty book");
        ensure!(self.bids[0].price <= self.asks[0].price, "crossed book");
        for levels in [&self.bids, &self.asks] {
            ensure!(
                levels
                    .iter()
                    .all(|x| x.price > Decimal::ZERO && x.units > 0),
                "invalid book level"
            );
        }
        ensure!(
            self.bids.windows(2).all(|x| x[0].price >= x[1].price)
                && self.asks.windows(2).all(|x| x[0].price <= x[1].price),
            "unsorted book"
        );
        Ok(())
    }
    pub fn mid(&self) -> Option<Decimal> {
        Some((self.bids.first()?.price + self.asks.first()?.price) / Decimal::TWO)
    }
    pub fn vwap(&self, side: Side, wanted: i64) -> Result<(Decimal, Decimal)> {
        ensure!(wanted > 0, "nonpositive size");
        let mut left = wanted;
        let mut cost = Decimal::ZERO;
        let mut worst = Decimal::ZERO;
        for level in if side == Side::Buy {
            &self.asks
        } else {
            &self.bids
        } {
            let take = left.min(level.units);
            cost += level.price * Decimal::from(take);
            left -= take;
            worst = level.price;
            if left == 0 {
                break;
            }
        }
        ensure!(left == 0, "insufficient depth");
        Ok((cost / Decimal::from(wanted), worst))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountEvidence {
    pub venue: Venue,
    pub account: String,
    pub observed_ms: u64,
    pub position_units: i64,
    pub free_margin: Decimal,
    pub equity: Decimal,
    pub leverage: u32,
    pub isolated: bool,
    pub open_orders: usize,
    pub authenticated: bool,
    pub liquidation_price: Option<Decimal>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub id: String,
    pub order_id: String,
    pub venue: Venue,
    pub side: Side,
    pub units: i64,
    pub price: Decimal,
    pub fee: Decimal,
    pub time_ms: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    pub units: i64,
    pub average: Decimal,
    pub realized: Decimal,
    pub fees: Decimal,
    pub funding: Decimal,
}
impl Position {
    pub fn apply(&mut self, f: &Fill) -> Result<()> { self.apply_for(f, super::MarketPair::Openai) }
    pub fn apply_for(&mut self, f: &Fill, market: super::MarketPair) -> Result<()> {
        ensure!(f.units > 0 && f.price > Decimal::ZERO, "invalid fill");
        let signed = f.units * f.side.sign();
        if self.units == 0 || self.units.signum() == signed.signum() {
            self.average = (self.average * Decimal::from(self.units.abs())
                + f.price * Decimal::from(f.units))
                / Decimal::from(self.units.abs() + f.units);
        } else {
            let closed = self.units.abs().min(f.units);
            self.realized +=
                market.quantity(closed) * (f.price - self.average) * Decimal::from(self.units.signum());
            if f.units > self.units.abs() {
                self.average = f.price;
            }
        }
        self.units = self
            .units
            .checked_add(signed)
            .ok_or_else(|| anyhow::anyhow!("position overflow"))?;
        if self.units == 0 {
            self.average = Decimal::ZERO;
        }
        self.fees += f.fee;
        Ok(())
    }
    pub fn unrealized(&self, exit: Decimal) -> Decimal { self.unrealized_for(exit, super::MarketPair::Openai) }
    pub fn unrealized_for(&self, exit: Decimal, market: super::MarketPair) -> Decimal {
        market.quantity(self.units) * (exit - self.average)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lot {
    /// Entry cash flow including opening-operation fees/repairs, per base unit.
    /// None on legacy lots: never infer missing costs for per-group exits.
    #[serde(default)]
    pub entry_net_spread: Option<Decimal>,
    pub id: String,
    pub level: usize,
    pub units: i64,
    pub opened_ms: u64,
    pub entry_spread: Decimal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Stopped,
    Recovering,
    Warming,
    Running,
    PausedEntries,
    Closing,
    RecoveringExposure,
    NeedsAttention,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Open,
    Close,
    Neutralize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub id: String,
    pub venue: Venue,
    pub side: Side,
    pub units: i64,
    pub limit: Decimal,
    /// Same-venue top-of-book midpoint observed when this request was built.
    /// Fills use it to attribute paid spread, market impact, and slippage.
    #[serde(default)]
    pub arrival_mid: Option<Decimal>,
    pub reduce_only: bool,
    pub created_ms: u64,
    pub expires_ms: u64,
    #[serde(default)]
    pub signed_expires_ms: Option<u64>,
}
impl OrderRequest {
    pub fn signed_expiry(&self) -> u64 {
        self.signed_expires_ms.unwrap_or(self.expires_ms)
    }
    /// Used only after the adapter binds an exchange order to this exact request.
    /// Keep the original local timestamp and the actual exchange fill timestamps.
    pub fn verified_exchange_created(&self, exchange_ms: u64) -> Result<u64> {
        ensure!(exchange_ms > 0 && exchange_ms.abs_diff(self.created_ms) <= 300_000,
            "exchange order time outside bounded reconciliation window");
        Ok(exchange_ms)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    /// Exchange creation time from an identity-validated order, never a guessed fill time.
    #[serde(default)]
    pub exchange_created_ms: Option<u64>,
    pub terminal: bool,
    pub fills: Vec<Fill>,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseAllocation {
    pub lot_id: String,
    pub units: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    /// Frozen logical ownership of a merged reduce-only exit. Empty preserves
    /// legacy single-lot/FIFO operations, including requests already in flight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub close_allocations: Vec<CloseAllocation>,
    /// Selected logical lot. None retains round/FIFO exits and emergency closes.
    #[serde(default)]
    pub close_lot_id: Option<String>,
    #[serde(default)]
    pub align_close: Option<OrderRequest>,
    #[serde(default)]
    pub align_close_terminal: bool,
    #[serde(default)]
    pub align_close_filled: i64,
    pub id: String,
    pub action: Action,
    pub level: usize,
    pub requested_units: i64,
    pub created_ms: u64,
    pub first: Option<OrderRequest>,
    pub hedge: Option<OrderRequest>,
    pub repair: Option<OrderRequest>,
    /// Terminal repair retries use a fresh client order id; never reset on restart.
    #[serde(default)]
    pub repair_attempt: u32,
    /// Last reserved recovery slippage in basis points (2..=5); zero is legacy/unstarted.
    #[serde(default)]
    pub recovery_slippage_bps: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recovery_wait_reason: String,
    /// Durable cooldown before a fresh-account-verified reducing repair retry.
    #[serde(default)]
    pub repair_retry_after_ms: Option<u64>,
    pub first_terminal: bool,
    pub hedge_terminal: bool,
    pub repair_terminal: bool,
    pub first_filled: i64,
    pub hedge_filled: i64,
    pub repair_filled: i64,
    pub first_value: Decimal,
    pub hedge_value: Decimal,
    pub failed: bool,
    /// Old persisted operations always used Lighter first. Never reinterpret
    /// an in-flight request when upgrading the entry policy.
    #[serde(default = "legacy_first_venue")]
    pub first_venue: Venue,
    #[serde(default)]
    pub min_entry_spread: Option<Decimal>,
    #[serde(default)]
    pub unwind_hedge: Option<OrderRequest>,
    #[serde(default)]
    pub unwind_hedge_terminal: bool,
    #[serde(default)]
    pub unwind_hedge_filled: i64,
    #[serde(default)]
    pub quote_wait_started_ms: Option<u64>,
}
fn legacy_first_venue() -> Venue {
    Venue::Lighter
}
impl Operation {
    pub fn hedge_venue(&self) -> Venue {
        if self.first_venue == Venue::Entropy {
            Venue::Lighter
        } else {
            Venue::Entropy
        }
    }
    pub fn paired_filled(&self) -> i64 {
        self.hedge_filled - self.unwind_hedge_filled
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LossStop {
    pub at_ms: u64,
    pub net_pnl: Decimal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emergency_exit: Option<super::emergency_exit::EmergencyExit>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub emergency_fill_allocations: BTreeMap<String, Vec<CloseAllocation>>,
    /// Live-only incident: one owned leg vanished at the venue; never infer its PnL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_orphan: Option<super::live_orphan::Incident>,
    /// Completed time additions within the configured round or grid-stage scope.
    #[serde(default)]
    pub time_adds_used: usize,
    #[serde(default)]
    pub last_open_completed: Option<(u64, Decimal)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidation_protection: Option<super::liquidation::Protection>,
    #[serde(default)]
    pub previous_group_exit: BTreeMap<String, u64>,
    /// Actual quantities closed by each operation, allocated in reserved order.
    /// Exchange fills stay aggregated; this ledger never invents individual fills.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub closed_lot_allocations: BTreeMap<String, Vec<CloseAllocation>>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub previous_reverse_signal: Option<(u64, Decimal, Decimal)>,
    #[serde(default = "default_armed")]
    pub reverse_first_armed: bool,
    #[serde(default)]
    pub fill_opening: BTreeMap<String, bool>,
    /// Persisted reservation budget; retries and partial fills belong to the reserved operation.
    #[serde(default)]
    pub entry_attempts_remaining: Option<u32>,
    #[serde(default)]
    pub recovery_after_ms: Option<u64>,
    #[serde(default)]
    pub consecutive_rollbacks: u32,
    #[serde(default)]
    pub mean_initialized: bool,
    /// Last established reference, retained for at most one window during maintenance.
    #[serde(default)]
    pub continuity_mean: Option<(u64, Decimal)>,
    /// Raw opening quotes, separate from the legacy midpoint/exit reference.
    #[serde(default)]
    pub entry_mean: super::entry_mean::EntryMean,
    #[serde(default)]
    pub loss_stop: Option<LossStop>,
    pub funding_synced_ms: u64,
    pub created_ms: u64,
    pub instance_id: String,
    pub previous_exit: Option<(u64, bool)>,
    pub exit_batch_active: bool,
    pub stop_requested: bool,
    pub schema: u32,
    pub config: InventoryConfig,
    pub status: Status,
    pub reason: String,
    pub sequence: u64,
    pub anchor: Option<Decimal>,
    pub lots: Vec<Lot>,
    pub armed: Vec<bool>,
    pub first_armed: bool,
    pub positions: [Position; 2],
    pub pending: Option<Operation>,
    pub fills: BTreeMap<String, Fill>,
    pub funding_ids: BTreeSet<String>,
    /// Settled cash flows retained for read-only lot attribution, including late arrivals.
    #[serde(default)]
    pub funding_records: BTreeMap<String, Funding>,
    pub samples: VecDeque<(u64, Decimal)>,
    pub previous_signal: Option<(u64, Decimal, Decimal)>,
    pub last_sample_ms: u64,
    /// Last accepted decision observation, distinct from statistical samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_observation: Option<(u64, [u64; 2])>,
    /// First eligible observation and grid level, independently for each direction.
    #[serde(default)]
    pub entry_confirmations: [Option<(u64, usize)>; 2],
    /// Time additions confirm independently from the grid in each direction.
    #[serde(default)]
    pub time_entry_confirmations: [Option<(u64, usize)>; 2],
    pub last_action_ms: u64,
    pub opened_groups: u64,
    pub closed_groups: u64,
    pub close_requested: bool,
    pub stop_after_close: bool,
    pub paused: bool,
    pub resume_after_recovery: bool,
    pub peak_pnl: Decimal,
    pub max_drawdown: Decimal,
    pub round_start_pnl: Decimal,
    /// Signed execution cost versus the same venue's arrival midpoint.
    /// Positive is cost; negative is price improvement. Legacy fills without
    /// an arrival midpoint remain explicitly outside this total.
    #[serde(default)]
    pub execution_cost: Decimal,
    #[serde(default)]
    pub execution_cost_started_ms: Option<u64>,
    #[serde(default)]
    pub execution_cost_tracked_fills: u64,
}
impl Snapshot {
    pub fn required_entry_spread(&self, mean:Decimal, level:usize) -> Decimal {
        let base=self.config.entry_threshold(mean);
        if self.config.accumulation.is_some() && level>=self.config.max_groups {
            // Time additions reuse the last completed entry price, not the moving MA gate.
            return self.last_open_completed.map_or(base,|(_,spread)|spread);
        }
        base.max(self.anchor.map_or(base,|a|a+Decimal::from(level)*self.config.grid))
    }
    pub fn new(config: InventoryConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            live_orphan: None,
            time_adds_used: 0,
            last_open_completed: None,
            liquidation_protection: None,
            previous_group_exit: BTreeMap::new(),
            closed_lot_allocations: BTreeMap::new(),
            emergency_exit: None,
            emergency_fill_allocations: BTreeMap::new(),
            direction: Direction::default(),
            previous_reverse_signal: None,
            reverse_first_armed: true,
            fill_opening: BTreeMap::new(),
            entry_attempts_remaining: None,
            recovery_after_ms: None,
            consecutive_rollbacks: 0,
            mean_initialized: false,
            continuity_mean: None,
            entry_mean: super::entry_mean::EntryMean::default(),
            loss_stop: None,
            funding_synced_ms: 0,
            created_ms: crate::domain::now_ms(),
            instance_id: {
                use rand_core::{OsRng, RngCore};
                format!(
                    "{:032x}",
                    ((OsRng.next_u64() as u128) << 64) | OsRng.next_u64() as u128
                )
            },
            previous_exit: None,
            exit_batch_active: false,
            stop_requested: false,
            schema: 1,
            armed: vec![true; config.max_groups],
            config,
            status: Status::Stopped,
            reason: String::new(),
            sequence: 0,
            anchor: None,
            lots: vec![],
            first_armed: true,
            positions: Default::default(),
            pending: None,
            fills: BTreeMap::new(),
            funding_ids: BTreeSet::new(),
            funding_records: BTreeMap::new(),
            samples: VecDeque::new(),
            previous_signal: None,
            last_sample_ms: 0,
            decision_observation: None,
            entry_confirmations: [None; 2],
            time_entry_confirmations: [None; 2],
            last_action_ms: 0,
            opened_groups: 0,
            closed_groups: 0,
            close_requested: false,
            stop_after_close: false,
            paused: false,
            resume_after_recovery: false,
            peak_pnl: Decimal::ZERO,
            max_drawdown: Decimal::ZERO,
            round_start_pnl: Decimal::ZERO,
            execution_cost: Decimal::ZERO,
            execution_cost_started_ms: None,
            execution_cost_tracked_fills: 0,
        })
    }
    pub fn record_fill(&mut self, f: &Fill, arrival_mid: Option<Decimal>) -> Result<bool> {
        let key = format!("{:?}:{}", f.venue, f.id);
        if let Some(existing) = self.fills.get(&key) {
            ensure!(existing == f, "duplicate fill id has different payload");
            return Ok(false);
        }
        ensure!(f.units > 0 && !f.id.is_empty(), "invalid fill identity");
        if let Some(mid) = arrival_mid {
            ensure!(mid > Decimal::ZERO, "invalid execution arrival midpoint");
            self.execution_cost +=
                self.config.quantity(f.units) * (f.price - mid) * Decimal::from(f.side.sign());
            self.execution_cost_tracked_fills = self.execution_cost_tracked_fills.saturating_add(1);
            self.execution_cost_started_ms = Some(
                self.execution_cost_started_ms
                    .map_or(f.time_ms, |started| started.min(f.time_ms)),
            );
        }
        let position = &self.positions[f.venue.index()];
        let opening = position.units == 0 || position.units.signum() == f.side.sign();
        self.positions[f.venue.index()].apply_for(f, self.config.market)?;
        self.fill_opening.insert(key.clone(), opening);
        self.fills.insert(key, f.clone());
        Ok(true)
    }
    pub fn cumulative_fees(&self) -> Decimal {
        self.fills.values().map(|fill| fill.fee).sum()
    }
    pub fn untracked_execution_fills(&self) -> u64 {
        (self.fills.len() as u64).saturating_sub(self.execution_cost_tracked_fills)
    }
    pub fn paired_units(&self) -> i64 {
        self.lots.iter().map(|x| x.units).sum()
    }
    pub fn assert_reconciled(&self, accounts: &[AccountEvidence; 2]) -> Result<()> {
        for (i, a) in accounts.iter().enumerate() {
            ensure!(
                a.venue.index() == i && a.position_units == self.positions[i].units,
                "venue position differs from owned ledger"
            );
        }
        if self.pending.is_none() {
            ensure!(
                self.positions[0].units == self.direction.sign() * self.paired_units()
                    && self.positions[1].units == -self.direction.sign() * self.paired_units(),
                "unpaired inventory"
            );
        }
        Ok(())
    }
    pub fn remaining_net(&self, books: &[Book; 2]) -> Result<Decimal> {
        let mut net = Decimal::ZERO;
        for (i, p) in self.positions.iter().enumerate() {
            if p.units == 0 {
                continue;
            }
            let (price, _) = books[i].vwap(
                if p.units > 0 { Side::Sell } else { Side::Buy },
                p.units.abs(),
            )?;
            let rate = if i == 0 {
                self.config.fee_lighter
            } else {
                self.config.fee_entropy
            };
            let slip = self.config.execution_slippage_bps / Decimal::from(10_000);
            let px = price * (Decimal::ONE - Decimal::from(p.units.signum()) * slip);
            net += p.unrealized_for(px, self.config.market) - self.config.quantity(p.units.abs()) * px * rate;
        }
        net += self
            .positions
            .iter()
            .map(|p| p.realized + p.funding - p.fees)
            .sum::<Decimal>();
        Ok(net - self.round_start_pnl)
    }
    pub fn round_carry(&self) -> Decimal {
        self.positions.iter().map(|p| p.funding - p.fees).sum()
    }
    pub fn total_pnl(&self, books: &[Book; 2]) -> Result<Decimal> {
        Ok(self.remaining_net(books)? + self.round_start_pnl)
    }
    pub fn finish_operation(&mut self, now: u64) -> Result<()> {
        let op = self
            .pending
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no operation"))?;
        if !op.first_terminal
            || (op.hedge.is_some() && !op.hedge_terminal)
            || (op.repair.is_some() && !op.repair_terminal)
            || (op.unwind_hedge.is_some() && !op.unwind_hedge_terminal)
            || (op.align_close.is_some() && !op.align_close_terminal)
        {
            bail!("operation has unresolved order");
        }
        let paired = op.paired_filled();
        ensure!(
            paired >= 0 && paired % self.config.common_step() == 0 && op.first_filled == paired + op.repair_filled,
            "operation has residual exposure"
        );
        // Order counters alone cannot mark an operation complete. The fill
        // ledger on BOTH venues must also equal the resulting lot inventory.
        // Check before changing lots so a mismatch retains its recovery record.
        ensure!(op.first_filled >= 0 && op.first_filled <= op.requested_units
            && op.repair_filled >= 0, "invalid completed operation quantity");
        let held = match op.action {
            Action::Open => self.paired_units().checked_add(paired),
            Action::Close => self.paired_units().checked_sub(op.first_filled),
            Action::Neutralize => bail!("standalone neutralize cannot complete a paired operation"),
        }.filter(|n| *n >= 0).ok_or_else(|| anyhow::anyhow!("invalid completed inventory"))?;
        ensure!([Venue::Lighter, Venue::Entropy].into_iter().all(|v|
            self.positions[v.index()].units == self.direction.open_side(v).sign() * held),
            "completed operation does not match both venue positions");
        if op.action == Action::Open && paired > 0 {
            let first_price = op.first_value / Decimal::from(op.first_filled);
            let hedge_price = op.hedge_value / Decimal::from(op.hedge_filled);
            let spread = Decimal::from(self.direction.sign()) * if op.first_venue == Venue::Entropy {
                first_price - hedge_price
            } else {
                hedge_price - first_price
            };
            if self.anchor.is_none() {
                self.anchor = Some(spread);
            }
            let prefix = format!("{}-v2-", op.id);
            let operation_fills=self.fills.values().filter(|f| f.order_id==op.id || f.order_id.starts_with(&prefix)).collect::<Vec<_>>();
            let known=[Venue::Lighter,Venue::Entropy].into_iter().all(|v|
                operation_fills.iter().filter(|f|f.venue==v).map(|f|f.units*f.side.sign()).sum::<i64>()
                    == paired*self.direction.open_side(v).sign());
            let opening_net = operation_fills.into_iter()
                .map(|f| -Decimal::from(f.side.sign()) * self.config.quantity(f.units) * f.price - f.fee)
                .sum::<Decimal>() / self.config.quantity(paired);
            self.lots.push(Lot {
                entry_net_spread: known.then_some(opening_net),
                id: op.id,
                level: op.level,
                units: paired,
                opened_ms: now,
                entry_spread: spread,
            });
            if let Some(armed)=self.armed.get_mut(op.level) { *armed=false; }
            if let Some(rules) = &self.config.accumulation {
                if op.level >= self.config.max_groups {
                    self.time_adds_used += 1;
                } else if rules.quota_scope == super::config::TimeAddQuotaScope::GridStage {
                    // Reset only after a positive, reconciled paired grid fill.
                    // Reservation, rejection and a fully unwound attempt grant no quota.
                    self.time_adds_used = 0;
                }
                self.last_open_completed=Some((now,spread));
            }
            self.opened_groups += 1;
        } else if op.action == Action::Close {
            let planned = if op.close_allocations.is_empty() {
                self.lots.iter().filter(|lot| op.close_lot_id.as_ref().is_none_or(|id| id == &lot.id))
                    .map(|lot| CloseAllocation { lot_id: lot.id.clone(), units: lot.units }).collect::<Vec<_>>()
            } else {
                ensure!(op.close_allocations.iter().map(|a| a.units).sum::<i64>() == op.requested_units,
                    "batch exit reservation size mismatch");
                op.close_allocations.clone()
            };
            // Validate the complete allocation before mutating any lot. An
            // invalid persisted reservation must not partly consume inventory.
            let mut seen = BTreeSet::new();
            for allocation in &planned {
                ensure!(allocation.units > 0 && allocation.units % self.config.common_step() == 0 && seen.insert(&allocation.lot_id),
                    "invalid or duplicate batch exit allocation");
                ensure!(self.lots.iter().any(|l| l.id == allocation.lot_id && l.units >= allocation.units),
                    "batch exit exceeds selected group inventory");
            }
            ensure!(op.first_filled >= 0 && op.first_filled <= op.requested_units
                && op.first_filled <= planned.iter().map(|a| a.units).sum::<i64>(), "exit exceeds owned inventory");
            let mut left = op.first_filled;
            let mut actual = Vec::new();
            for allocation in planned {
                if left == 0 { break; }
                let lot = self.lots.iter_mut().find(|l| l.id == allocation.lot_id).unwrap();
                let take = left.min(allocation.units);
                lot.units -= take;
                left -= take;
                actual.push(CloseAllocation { lot_id: lot.id.clone(), units: take });
                if lot.units == 0 && take > 0 {
                    if let Some(armed)=self.armed.get_mut(lot.level) { *armed=false; }
                    self.closed_groups += 1;
                }
                if left == 0 {
                    break;
                }
            }
            self.closed_lot_allocations.insert(op.id.clone(), actual);
            self.lots.retain(|x| x.units > 0);
            self.previous_group_exit.retain(|id, _| self.lots.iter().any(|l| &l.id == id));
            ensure!(left == 0, "exit exceeds owned inventory");
        }
        if self.lots.is_empty() {
            self.previous_group_exit.clear();
            self.time_adds_used=0;
            self.last_open_completed=None;
            self.anchor = None;
            self.armed.fill(true);
            if op.action == Action::Close && op.first_filled > 0 {
                if self.direction == Direction::LighterLong { self.first_armed = false; }
                else { self.reverse_first_armed = false; }
            }
            self.close_requested = false;
            self.exit_batch_active = false;
        }
        self.pending = None;
        self.last_action_ms = now;
        self.recovery_after_ms = None;
        if op.failed {
            self.consecutive_rollbacks = self.consecutive_rollbacks.saturating_add(1);
        } else if paired > 0 {
            self.consecutive_rollbacks = 0;
        }
        if self.loss_stop.is_some() {
            self.paused = true;
            self.stop_after_close = true;
            self.close_requested = !self.lots.is_empty();
            self.stop_requested = self.lots.is_empty();
            self.status = if self.lots.is_empty() {
                Status::Stopped
            } else {
                Status::Closing
            };
            self.reason = "total loss limit latched; protected close and stop".into();
        } else if op.failed {
            self.status = Status::NeedsAttention;
            if ((op.action == Action::Open && paired == 0) || op.action == Action::Close)
                && self.consecutive_rollbacks < 3
                && !self.paused
                && !self.stop_requested
                && !self.stop_after_close
            {
                self.recovery_after_ms = Some(now.saturating_add(30_000));
                self.reason = "execution recovered; cooling down before verified resume".into();
            } else {
                self.reason = "execution recovered; review before resume (repeated failure or non-entry recovery)".into();
            }
        } else if self.stop_requested || (self.stop_after_close && self.lots.is_empty()) {
            self.status = Status::Stopped;
        } else {
            self.status = if self.paused {
                Status::PausedEntries
            } else {
                Status::Running
            };
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Funding {
    pub id: String,
    pub venue: Venue,
    pub amount: Decimal,
    pub time_ms: u64,
}
