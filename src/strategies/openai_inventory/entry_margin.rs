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
        let current_required=self.position_value.context("缺少 Lighter 持仓价值")?*fraction;
        ensure!(equity>=current_required,"Lighter 现有逐仓权益不足初始保证金要求；暂停新增开仓");
        // Lighter's isolated allocation uses mark notional minus the new
        // trade's PnL against mark, plus fees. Do not spend positive trade PnL
        // optimistically before it exists (conservative upper bound).
        let adverse=(Decimal::from(side.sign())*(limit-mark)).max(Decimal::ZERO);
        let needed=quantity*(mark*fraction+adverse+limit*fee);
        ensure!(cash>=needed+reserve,
            "Lighter 可划转保证金不足：可用 {} U，本次需 {} U，保留 {} U（平台显示可用 {} U）",
            cash,needed,reserve,self.reported_available.unwrap());
        Ok(())
    }
}

pub fn check_account(a:&AccountEvidence,c:&InventoryConfig,units:i64,limit:Decimal,side:Side)->Result<()> {
    if let Some(e)=&a.entry_margin {
        e.check(c.quantity(units),limit,side,c.fee_lighter,c.min_free_margin)?;
    }
    Ok(())
}

pub fn latch(s:&mut Snapshot,r:&OrderRequest,rejection:EntryRejection) {
    s.entry_margin_block=Some(EntryMarginBlock{venue:r.venue,at_ms:r.created_ms,order_id:r.id.clone(),rejection});
    s.paused=true;
    s.recovery_after_ms=None;
    strategy::clear_entry_confirmation(s);
    if let Some(retry)=s.pending.as_mut().and_then(|p|p.entry_hedge_retry.as_mut()) {
        retry.abandoned=true;retry.after_ms=None;retry.reason=BLOCK_MESSAGE.into();
    }
}

#[cfg(test)]
#[path="entry_margin_tests.rs"]
mod tests;
