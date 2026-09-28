use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Legacy paper identity, retained for saved simulation ledgers; forbidden for live use.
pub const PAPER_ENTROPY_ADDRESS: &str = "0x0000000000000000000000000000000000000005";
pub const LIVE_CONFIRMATION: &str = "START_OPENAI_INVENTORY_LIVE";
pub const LIVE_STRATEGY_CONFIRMATION: &str = "START_OPENAI_LIVE_STRATEGY";
pub const LIVE_LIMIT_UPGRADE_CONFIRMATION: &str = "UPGRADE_OPENAI_LIVE_LIMITS";
pub const CURRENT_TIME_ADD_INTERVAL_MS: u64 = 1_800_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Paper,
    Live,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitPolicy {
    #[default]
    Round,
    PerGroup,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeAddQuotaScope {
    /// Missing in legacy ledgers: retain their whole-round quota on upgrade.
    #[default]
    Round,
    GridStage,
}
impl TimeAddQuotaScope {
    fn is_round(&self) -> bool { *self == Self::Round }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccumulationRules {
    pub entry_floor: Decimal,
    pub interval_ms: u64,
    pub max_time_adds: usize,
    #[serde(default, skip_serializing_if = "TimeAddQuotaScope::is_round")]
    pub quota_scope: TimeAddQuotaScope,
    pub contraction_ratio: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InventoryConfig {
    /// Omitted only for the original OPENAI ledger format.
    #[serde(skip_serializing_if = "super::MarketPair::is_openai")]
    pub market: super::MarketPair,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lighter_address: Option<String>,
    /// Both aggregation policies use the same mean/net-profit test.
    pub shared_exit_conditions: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accumulation: Option<AccumulationRules>,
    pub exit_policy: ExitPolicy,
    pub group_take_profit: Decimal,
    pub direction_policy: super::DirectionPolicy,
    pub mode: Mode,
    pub lighter_account: String,
    /// Public RH account index, explicitly pinned before loading live credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lighter_account_index: Option<i64>,
    pub entropy_account: String,
    pub entropy_address: String,
    pub sample_ms: u64,
    /// Separate decision cadence; absent preserves legacy sample-driven decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_ms: Option<u64>,
    /// Entry-only minimum confirmation delay; None preserves legacy behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_confirmation_ms: Option<u64>,
    pub mean_window_ms: u64,
    pub entry_offset: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_threshold_cap: Option<Decimal>,
    pub grid: Decimal,
    pub group_notional: Decimal,
    pub max_groups: usize,
    pub leverage: u32,
    pub max_notional_per_venue: Decimal,
    pub max_loss_usdc: Decimal,
    pub min_free_margin: Decimal,
    pub paper_capital_per_venue: Decimal,
    pub fee_entropy: Decimal,
    pub fee_lighter: Decimal,
    pub execution_slippage_bps: Decimal,
    pub exit_profit_reserve: Decimal,
    pub close_slice_notional: Decimal,
    pub book_max_age_ms: u64,
    pub account_max_age_ms: u64,
    pub operation_timeout_ms: u64,
    pub exclusive_positions: bool,
    pub auto_neutralize: bool,
}
impl Default for InventoryConfig {
    fn default() -> Self {
        Self {
            market: super::MarketPair::Openai,
            lighter_address: None,
            shared_exit_conditions: false,
            accumulation: None,
            exit_policy: ExitPolicy::Round,
            group_take_profit: Decimal::from(2),
            direction_policy: super::DirectionPolicy::default(),
            mode: Mode::Paper,
            lighter_account: "o1".into(),
            lighter_account_index: None,
            entropy_account: "o1".into(),
            entropy_address: PAPER_ENTROPY_ADDRESS.into(),
            sample_ms: 15_000,
            decision_ms: None,
            entry_confirmation_ms: None,
            mean_window_ms: 3_600_000,
            entry_offset: Decimal::from(4),
            entry_threshold_cap: None,
            grid: Decimal::from(2),
            group_notional: Decimal::from(15),
            max_groups: 20,
            leverage: 3,
            max_notional_per_venue: Decimal::from(300),
            max_loss_usdc: Decimal::from(10),
            min_free_margin: Decimal::from(10),
            paper_capital_per_venue: Decimal::from(100),
            fee_entropy: Decimal::new(9, 5),
            fee_lighter: Decimal::ZERO,
            execution_slippage_bps: Decimal::ONE,
            exit_profit_reserve: Decimal::ZERO,
            close_slice_notional: Decimal::from(30),
            book_max_age_ms: 1500,
            account_max_age_ms: 3000,
            operation_timeout_ms: 5000,
            exclusive_positions: true,
            auto_neutralize: true,
        }
    }
}
impl InventoryConfig {
    pub fn quantity(&self, n: i64) -> Decimal { self.market.quantity(n) }
    pub fn units(&self, q: Decimal) -> Result<i64> { self.market.units(q) }
    pub fn common_units(&self, notional: Decimal, price: Decimal) -> Result<i64> { self.market.common_units(notional, price) }
    pub fn common_step(&self) -> i64 { self.market.common_step() }
    /// The approved transition changes inventory/loss bounds only, never account,
    /// pricing, sizing, confirmation, or execution protections.
    pub fn validate_live_limit_upgrade(&self, previous: &Self) -> Result<()> {
        self.validate()?;
        ensure!(previous.mode == Mode::Live && previous.max_groups == 1
            && previous.group_notional == Decimal::from(15)
            && previous.max_notional_per_venue == Decimal::from(15)
            && previous.max_loss_usdc == Decimal::from(2), "not a bounded live profile");
        let mut expected = previous.clone();
        expected.max_groups = 20;
        expected.max_notional_per_venue = Decimal::from(300);
        expected.max_loss_usdc = Decimal::from(30);
        ensure!(*self == expected, "live upgrade may only restore the approved 20-group, 300U, 30U bounds");
        Ok(())
    }
    /// Also required for read-only checks against real accounts, regardless of mode.
    pub fn validate_live_identity(&self) -> Result<()> {
        ensure!(self.market != super::MarketPair::Anth || self.lighter_address.as_ref().is_some_and(|a| valid_address(a)
            && a[2..].bytes().any(|b| b != b'0') && !a.eq_ignore_ascii_case(PAPER_ENTROPY_ADDRESS)),
            "ANTH requires an explicitly bound Lighter master address");
        ensure!(valid_address(&self.entropy_address)
            && !self.entropy_address.eq_ignore_ascii_case(PAPER_ENTROPY_ADDRESS)
            && self.entropy_address[2..].bytes().any(|b| b != b'0'),
            "explicit non-placeholder Entropy master address required for real accounts");
        ensure!(self.lighter_account_index.is_some_and(|index| index >= 0),
            "explicit Lighter RH account index required for real accounts");
        Ok(())
    }
    pub fn entry_threshold(&self, mean: Decimal) -> Decimal {
        let raw = mean + self.entry_offset;
        if let Some(rules)=&self.accumulation { return raw.max(rules.entry_floor); }
        self.entry_threshold_cap.map_or(raw, |cap| raw.min(cap)).max(Decimal::ZERO)
    }
    pub fn decision_interval_ms(&self) -> u64 {
        self.decision_ms.unwrap_or(self.sample_ms)
    }
    /// Keep the original confirmation lifetime when only decision cadence changes.
    pub fn confirmation_window_ms(&self) -> u64 {
        self.sample_ms * 2
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(r)=&self.accumulation {
            ensure!(self.shared_exit_conditions
                && self.entry_confirmation_ms.is_some() && self.entry_threshold_cap.is_none()
                && r.entry_floor>Decimal::ZERO
                && r.interval_ms >= if r.quota_scope == TimeAddQuotaScope::GridStage { CURRENT_TIME_ADD_INTERVAL_MS } else { 900_000 }
                && r.max_time_adds<=5
                && r.contraction_ratio>Decimal::ZERO && r.contraction_ratio<=Decimal::ONE,
                "invalid accumulation rules");
        }

        ensure!(self.entry_threshold_cap.is_none_or(|cap| cap > Decimal::ZERO), "entry threshold cap must be positive");
        ensure!(self.entry_confirmation_ms.is_none_or(|ms| self.decision_ms == Some(1000) && (1000..=15_000).contains(&ms)
            && ms < self.confirmation_window_ms()),
            "entry confirmation delay requires one-second decisions and 1000..15000 ms below timeout");
        ensure!(self.decision_ms.is_none_or(|ms| (1000..=60_000).contains(&ms)),
            "separate decision cadence requires 1000..60000 ms");
        ensure!(!self.shared_exit_conditions || self.exit_profit_reserve == Decimal::ZERO,
            "shared exit comparison requires zero net-profit reserve");
        ensure!(self.group_take_profit > Decimal::ZERO, "invalid per-group take-profit spread");
        ensure!(
            self.max_loss_usdc > Decimal::ZERO && self.max_loss_usdc <= Decimal::from(30),
            "strategy loss limit must be positive and at most 30 USDC"
        );
        ensure!(
            [&self.lighter_account, &self.entropy_account].into_iter()
                .all(|id| !id.is_empty() && id.trim() == id && !id.chars().any(char::is_control)),
            "nonempty, unambiguous account aliases required"
        );
        ensure!(
            valid_address(&self.entropy_address),
            "Entropy master address must be 0x followed by 40 hex digits"
        );
        ensure!(self.lighter_account_index.is_none_or(|index| index >= 0),
            "invalid Lighter RH account index");
        if self.mode == Mode::Live {
            self.validate_live_identity()?;
        }
        ensure!(
            self.exclusive_positions && self.auto_neutralize,
            "exclusive ownership and automatic neutralization are required"
        );
        ensure!(
            (1000..=60_000).contains(&self.sample_ms)
                && self.mean_window_ms >= self.sample_ms * 4
                && self.mean_window_ms <= 86_400_000,
            "invalid sampling window"
        );
        ensure!(
            self.entry_offset >= Decimal::ZERO && self.grid > Decimal::ZERO,
            "invalid spread parameters"
        );
        ensure!(
            self.group_notional >= Decimal::from(10) && self.group_notional <= Decimal::from(100),
            "invalid group notional"
        );
        ensure!(
            (1..=20).contains(&self.max_groups) && (1..=3).contains(&self.leverage),
            "inventory/leverage exceeds approved bounds"
        );
        ensure!(
            self.max_notional_per_venue >= self.group_notional
                && self.max_notional_per_venue <= Decimal::from(300),
            "invalid inventory cap"
        );
        ensure!(
            self.min_free_margin >= Decimal::ZERO
                && self.paper_capital_per_venue > self.min_free_margin,
            "invalid collateral"
        );
        ensure!(
            (Decimal::ZERO..=Decimal::new(1, 2)).contains(&self.fee_entropy)
                && (Decimal::ZERO..=Decimal::new(1, 2)).contains(&self.fee_lighter),
            "invalid fees"
        );
        ensure!(
            self.execution_slippage_bps >= Decimal::ZERO
                && self.execution_slippage_bps <= Decimal::from(20),
            "invalid execution price protection"
        );
        ensure!(
            self.exit_profit_reserve >= Decimal::ZERO
                && self.close_slice_notional >= Decimal::from(10)
                && self.close_slice_notional <= self.max_notional_per_venue,
            "invalid exit controls"
        );
        ensure!(
            (100..=1500).contains(&self.book_max_age_ms)
                && (100..=3000).contains(&self.account_max_age_ms),
            "invalid freshness boundary"
        );
        ensure!(
            (500..=10_000).contains(&self.operation_timeout_ms),
            "invalid execution deadline"
        );
        Ok(())
    }
}

fn valid_address(address: &str) -> bool {
    address.len() == 42 && address.starts_with("0x")
        && address[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod threshold_cap_tests {
    use super::*;
    #[test]
    fn capped_threshold_and_grid() {
        let mut c = InventoryConfig::default();
        c.entry_offset = Decimal::ONE;
        c.entry_threshold_cap = Some(Decimal::from(5));
        for (mean, expected) in [(10,5),(3,4),(-2,0),(4,5)] {
            assert_eq!(c.entry_threshold(Decimal::from(mean)), Decimal::from(expected));
        }
        assert_eq!(c.entry_threshold(Decimal::from(10)).max(Decimal::from(6)+c.grid), Decimal::from(8));
        c.entry_threshold_cap = None;
        assert_eq!(c.entry_threshold(Decimal::from(10)), Decimal::from(11));
        c.entry_threshold_cap = Some(Decimal::ZERO);
        assert!(c.validate().is_err());
    }

    #[test]
    fn bounded_live_profile_keeps_current_rules_and_small_loss_cap() {
        let c: InventoryConfig = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-one-entry.json")).unwrap();
        c.validate().unwrap();
        assert_eq!(c.mode, Mode::Live);
        assert_eq!(c.exit_policy, ExitPolicy::PerGroup);
        assert_eq!(c.direction_policy, super::super::DirectionPolicy::Both);
        assert_eq!(c.entry_confirmation_ms, Some(5_000));
        assert_eq!(c.max_groups, 1);
        assert_eq!(c.group_notional, Decimal::from(15));
        assert_eq!(c.max_loss_usdc, Decimal::from(2));
        assert_eq!(c.entry_offset, Decimal::ZERO);
        for (mean, expected) in [(-2,5),(0,5),(4,5),(5,5),(6,6),(10,10)] {
            assert_eq!(c.entry_threshold(Decimal::from(mean)), Decimal::from(expected));
        }
    }
}
