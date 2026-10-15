//! Operator acknowledgement releases only the consecutive-rollback latch.
//! Queries and persistence run inside the owning actor; this path cannot submit.
use super::{store::Store, venue::{self, AccountWorker}, *};
use anyhow::{ensure, Context, Result};

fn eligible(s: &Snapshot) -> Result<()> {
    ensure!(s.config.mode == Mode::Live, "人工恢复仅适用于实盘连续回滚保护");
    ensure!(s.status == Status::Stopped && s.paused && s.stop_requested,
        "请先点击停止交易，等待状态变为已停止，再确认恢复");
    ensure!(s.consecutive_rollbacks >= 3, "当前没有连续回滚保护需要解除");
    ensure!(s.pending.is_none(), "仍有未完成订单，不能解除保护");
    ensure!(s.live_orphan.is_none() && s.loss_stop.is_none() && s.liquidation_protection.is_none()
        && !s.close_requested && !s.stop_after_close && !emergency_exit::active(s),
        "仍有孤立仓位、亏损保护或全部平仓任务，不能解除保护");
    s.config.validate()?;
    Ok(())
}

fn checked_state(s: &Snapshot, a: &[AccountEvidence; 2], now: u64) -> Result<Snapshot> {
    eligible(s)?;
    ensure!(s.lots.iter().all(|l| l.units > 0), "持仓分组数量无效，不能解除保护");
    s.assert_reconciled(a).context("两平台实际持仓、配对数量与账本不一致，不能解除保护")?;
    for (i, account) in a.iter().enumerate() {
        let expected = if i == 0 { &s.config.lighter_account } else { &s.config.entropy_account };
        ensure!(account.authenticated && account.venue.index() == i && &account.account == expected,
            "账户认证或账户绑定不一致，不能解除保护");
        ensure!(account.observed_ms > 0 && account.observed_ms <= now
            && now - account.observed_ms <= s.config.account_max_age_ms,
            "账户核对结果过期或时间异常，请稍后重试");
        ensure!(account.open_orders == 0, "平台仍有挂单，不能解除保护");
        ensure!(account.isolated && account.leverage == s.config.leverage,
            "保证金模式或杠杆与配置不一致，不能解除保护");
    }
    let mut next = s.clone();
    next.consecutive_rollbacks = 0;
    next.recovery_after_ms = None;
    next.resume_after_recovery = false;
    strategy::clear_decision_confirmation(&mut next);
    next.reason = format!("manual rollback protection recovery verified (previous rollbacks: {}); stopped until explicit start", s.consecutive_rollbacks);
    // Preserve inventory, entry anchors, quotas, funding, PnL and every other latch.
    Ok(next)
}

pub(super) async fn confirm(
    state: &mut Snapshot, store: &mut Store, workers: &[AccountWorker; 2], id: &str,
) -> Result<Option<[AccountEvidence; 2]>> {
    ensure!(!id.is_empty() && id.len() <= 128, "invalid command id");
    let name = serde_json::to_string(&Control::ConfirmRollbackRecovery)?;
    // Replaying an accepted acknowledgement must never clear a later incident.
    if store.command_seen(id, &name)? { return Ok(None); }
    eligible(state)?;
    let accounts = venue::fresh_reconciled_accounts(workers, state.config.account_max_age_ms)
        .await.context("无法取得两个平台的新核对结果，保护未解除，请稍后重试或发送诊断包")?;
    let now = crate::domain::now_ms();
    let next = checked_state(state, &accounts, now)?;
    let evidence = serde_json::json!({
        "previous_rollbacks": state.consecutive_rollbacks,
        "accounts": accounts.iter().map(|a| serde_json::json!({
            "venue":a.venue,"position_units":a.position_units,"open_orders":a.open_orders,
            "authenticated":a.authenticated,"observed_ms":a.observed_ms
        })).collect::<Vec<_>>()
    });
    // A failed transaction leaves both the in-memory latch and durable state intact.
    store.command_with_evidence(&next, id, &name, now, evidence)?;
    *state = next;
    Ok(Some(accounts))
}

#[cfg(test)]
#[path = "manual_recovery_tests.rs"]
mod tests;
