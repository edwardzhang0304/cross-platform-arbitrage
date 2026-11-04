//! Entry admission and durable non-price rejection protection. Reducing orders
//! never use this gate; money is never transferred to make an entry pass.
use super::*;
use anyhow::{ensure, Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub const BLOCK_MESSAGE: &str = "保证金拒单保护：禁止开仓和加仓；平仓与止损是否启用，请查看控制台当前状态";
pub(super) const LEGACY_BLOCK_MESSAGE: &str = "保证金拒单保护：已暂停新增开仓；正常平仓和止损仍保留，请核对可划转保证金后人工恢复";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryRejection { MarginNotAllowed, InvalidBalance, PreflightFunding }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMarginBlock {
    pub venue: Venue,
    pub at_ms: u64,
    pub order_id: String,
    pub rejection: EntryRejection,
}

/// A local preflight can expire without operator action. Exchange rejections use
/// EntryMarginBlock instead. Keep the two distinct when loading older ledgers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryFundingWait {
    pub retry_after_ms: u64,
    pub reason: String,
}

pub fn wait(s: &mut Snapshot, now: u64, reason: String) {
    s.entry_funding_wait = Some(EntryFundingWait {
        retry_after_ms: now.saturating_add(30_000),
        reason,
    });
    strategy::clear_entry_confirmation(s);
}

/// Normalized, credential-free evidence. Collateral excludes all isolated
/// allocations on both REST and streaming paths. Missing fields stay unknown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryMarginEvidence {
    pub reported_available: Option<Decimal>,
    pub collateral: Option<Decimal>,
    pub allocated_margin: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub position_value: Option<Decimal>,
    pub initial_margin_fraction: Option<Decimal>,
    pub mark_price: Option<Decimal>,
    pub isolated_only: bool,
}
impl EntryMarginEvidence {
    pub fn spendable(&self) -> Option<Decimal> {
        self.isolated_only.then_some(())?;
        Some(self.reported_available?.min(self.collateral?).max(Decimal::ZERO))
    }
    pub fn check(&self, quantity: Decimal, limit: Decimal, side: Side, fee: Decimal,
        reserve: Decimal) -> Result<()> {
        let cash=self.spendable().context("Lighter 加仓保证金证据不完整或存在未支持的全仓风险；暂停新增开仓")?;
        let fraction=self.initial_margin_fraction.context("缺少 Lighter 初始保证金率")?;
        ensure!(fraction>Decimal::ZERO && fraction<=Decimal::ONE,"invalid initial margin fraction");
        let mark=self.mark_price.or_else(||
            (self.position_value==Some(Decimal::ZERO)).then_some(limit))
            .context("缺少 Lighter 保证金计价依据")?;
        ensure!(mark>Decimal::ZERO && limit>Decimal::ZERO && quantity>Decimal::ZERO,"invalid margin valuation");
        let equity=self.allocated_margin.context("缺少 Lighter 逐仓保证金")?
            + self.unrealized_pnl.context("缺少 Lighter 逐仓未实现盈亏")?;
        let old_value = self.position_value.context("缺少 Lighter 持仓价值")?;
        ensure!(old_value >= Decimal::ZERO && fee >= Decimal::ZERO && reserve >= Decimal::ZERO,
            "invalid margin evidence");
        let added_value = quantity * mark;
        let added_initial = added_value * fraction;
        let trade_net = quantity * (Decimal::from(side.sign()) * (mark - limit) - limit * fee);
        // Do not spend expected favorable execution before it exists.
        let cash_needed = added_initial + (-trade_net).max(Decimal::ZERO);
        ensure!(cash >= cash_needed + reserve,
            "Lighter 可划转保证金不足：可用 {cash} U，本次需 {cash_needed} U，保留 {reserve} U");

        // Same-side growth only. Lighter assigns margin to the increment;
        // unrelated free cash does not appear in this position's equity.
        // lighter-prover apply_trade::calculate_isolated_margin_change and
        // RiskParameters::is_valid_risk_change (fa85b331).
        let allocation = (added_initial - trade_net).max(Decimal::ZERO);
        let after_equity = equity + allocation + trade_net;
        let after_value = old_value + added_value;
        if equity >= old_value * fraction {
            ensure!(after_equity >= after_value * fraction, "Lighter 加仓后整仓权益不足");
        } else {
            // At the same mark and on the same isolated market its maintenance
            // requirement is proportional to position value. Require strict
            // improvement and positive equity, even if the venue allows equal.
            ensure!(old_value > Decimal::ZERO && equity > Decimal::ZERO
                && after_equity / after_value > equity / old_value,
                "Lighter 加仓后整仓保证金状况未改善");
        }
        Ok(())
    }
}

pub fn check_account(a:&AccountEvidence,c:&InventoryConfig,units:i64,limit:Decimal,side:Side)->Result<()> {
    if let Some(e)=&a.entry_margin {
        ensure!(a.isolated && (a.position_units == 0 || a.position_units.signum() == side.sign()),
            "Lighter 加仓需要同向逐仓持仓");
        let cap_price = e.mark_price.unwrap_or(limit).max(limit);
        ensure!(c.quantity(a.position_units.abs() + units) * cap_price <= c.max_notional_per_venue,
            "Lighter 加仓后整仓超过本账户持仓额度");
        let below_initial = e.allocated_margin.zip(e.unrealized_pnl)
            .zip(e.position_value.zip(e.initial_margin_fraction))
            .is_some_and(|((margin, pnl), (value, fraction))| margin + pnl < value * fraction);
        if a.position_units != 0 && below_initial {
            let mark = e.mark_price.context("缺少 Lighter 持仓标记价格")?;
            let liquidation = a.liquidation_price.context("缺少 Lighter 持仓强平价")?;
            ensure!(liquidation >= Decimal::ZERO && (liquidation.is_zero()
                || (mark-liquidation)*Decimal::from(a.position_units.signum()) > mark*Decimal::new(5,2)),
                "Lighter 持仓已接近强平保护线");
        }
        e.check(c.quantity(units),limit,side,c.fee_lighter,c.min_free_margin)?;
    }
    Ok(())
}

pub fn record_rejection(s:&mut Snapshot,r:&OrderRequest,rejection:EntryRejection,now:u64,reason:&str) {
    if let Some(retry)=s.pending.as_mut().and_then(|p|p.entry_hedge_retry.as_mut()) {
        retry.abandoned=true;retry.after_ms=None;retry.reason=reason.to_owned();
    }
    if rejection == EntryRejection::PreflightFunding {
        wait(s,now,reason.to_owned());
        return;
    }
    s.entry_margin_block=Some(EntryMarginBlock{venue:r.venue,at_ms:r.created_ms,order_id:r.id.clone(),rejection});
    s.paused=true;
    s.recovery_after_ms=None;
    strategy::clear_entry_confirmation(s);
}

#[cfg(test)]
#[path="entry_margin_tests.rs"]
mod tests;
