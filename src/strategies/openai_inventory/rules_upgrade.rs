//! The same stopped-ledger rule migration is used by both runtimes.
use super::*;
use anyhow::{Context, Result, ensure};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{fs::OpenOptions, path::Path};

/// Change only the user's approved grid/time-add parameters. Account binding,
/// size, fees, exit policy, risk limits and every other setting are retained.
pub fn current_config(previous: &InventoryConfig) -> Result<InventoryConfig> {
    previous.validate()?;
    let mut next = previous.clone();
    let rules = next.accumulation.as_mut().context("升级需要现有同价加仓策略配置")?;
    if next.grid == Decimal::from(5) && rules.quota_scope == TimeAddQuotaScope::GridStage
        && [CURRENT_TIME_ADD_INTERVAL_MS, 3_600_000].contains(&rules.interval_ms) {
        // rc.6 -> rc.7 changes only the interval, including for a smaller quota.
        rules.interval_ms = CURRENT_TIME_ADD_INTERVAL_MS;
        next.validate()?;
        return Ok(next);
    }
    ensure!([Decimal::from(2), Decimal::from(5)].contains(&next.grid)
        && [900_000, 3_600_000].contains(&rules.interval_ms)
        && rules.max_time_adds <= 5,
        "无法识别旧网格参数，需先核对配置，不能直接覆盖");
    next.grid = Decimal::from(5);
    rules.interval_ms = CURRENT_TIME_ADD_INTERVAL_MS;
    rules.max_time_adds = 5;
    rules.quota_scope = TimeAddQuotaScope::GridStage;
    next.validate()?;
    Ok(next)
}

pub fn migrate_snapshot(state: &mut Snapshot, desired: &InventoryConfig) -> Result<bool> {
    // Also makes an interrupted DB/config-file upgrade safe to retry.
    ensure!(current_config(&state.config)? == *desired, "新配置与原账本账户、标的或其他参数不一致");
    if state.config == *desired { return Ok(false); }
    ensure!(state.schema == 1 && state.status == Status::Stopped && state.paused
        && state.stop_requested && state.pending.is_none() && state.live_orphan.is_none()
        && state.loss_stop.is_none() && !state.close_requested && !state.stop_after_close,
        "请先在旧程序停止交易并完成双腿配平，再升级新规则");
    let held = state.paired_units();
    ensure!([Venue::Lighter, Venue::Entropy].into_iter().all(|v|
        state.positions[v.index()].units == state.direction.open_side(v).sign() * held),
        "原账本双腿数量与持仓组数不匹配，不能迁移");
    ensure!(state.time_adds_used <= 5 && state.armed.len() == desired.max_groups,
        "旧加仓计数或网格长度异常，不能迁移");
    // Merge old 2U grid slots into 5U slots. Multiple historical groups may
    // occupy one new slot; all of their IDs, sizes and actual fill prices stay.
    // An interval-only upgrade must not re-arm or disarm any existing grid slot.
    if state.config.grid != desired.grid {
        let level = |old: usize| -> Result<usize> {
            let mapped = (Decimal::from(old) * state.config.grid / desired.grid).floor()
                .to_usize().context("invalid migrated grid slot")?;
            ensure!(mapped < desired.max_groups, "migrated grid outside capacity");
            Ok(mapped)
        };
        let mut armed = vec![true; desired.max_groups];
        for (old, ready) in state.armed.iter().enumerate() {
            if !ready { armed[level(old)?] = false; }
        }
        let mut lots = state.lots.clone();
        for lot in &mut lots {
            if lot.level < state.config.max_groups {
                lot.level = level(lot.level)?;
                armed[lot.level] = false;
            }
        }
        state.lots = lots;
        state.armed = armed;
    }
    state.config = desired.clone();
    // Installing an update does not itself award extra same-price entries.
    // The next completed paired grid entry renews the stage quota normally.
    strategy::clear_decision_confirmation(state);
    strategy::clear_entry_confirmation(state);
    state.entry_attempts_remaining = Some(0);
    state.resume_after_recovery = false;
    state.reason = format!("已升级：5U 网格、同价至少 30 分钟、每档最多 {} 次；原持仓、已用次数和上次成交时间保留，等待账户核对后启动",
        desired.accumulation.as_ref().unwrap().max_time_adds);
    Ok(true)
}

/// No worker or network calls. State + migration event commit atomically.
/// Callers save the config file afterwards; retries accept an already migrated DB.
pub fn migrate_ledger(path: &Path, desired: &InventoryConfig) -> Result<bool> {
    if !path.exists() { return Ok(false); }
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(path.with_extension("lock"))?;
    lock.try_lock().context("旧程序仍占用账本，请先退出旧后台")?;
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.execute_batch("PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let body: Option<String> = tx.query_row("SELECT body FROM state WHERE id=1", [], |r| r.get(0)).optional()?;
    let Some(body) = body else { return Ok(false); };
    let mut state: Snapshot = serde_json::from_str(&body)?;
    let old_grid = state.config.grid;
    let old_interval = state.config.accumulation.as_ref().map(|r| r.interval_ms);
    if !migrate_snapshot(&mut state, desired)? { return Ok(false); }
    let after = serde_json::to_string(&state)?;
    tx.execute("UPDATE state SET body=?1 WHERE id=1", [&after])?;
    let event = serde_json::json!({"from_grid":old_grid,"to_grid":desired.grid,
        "from_interval_ms":old_interval,"to_interval_ms":CURRENT_TIME_ADD_INTERVAL_MS,
        "quota_scope":"grid_stage","time_adds_used":state.time_adds_used,
        "groups":state.lots.len(),"kept_positions":true});
    tx.execute("INSERT INTO events(at_ms,kind,body) VALUES(?1,'rules_interval_30m_v1',?2)",
        params![crate::domain::now_ms(),event.to_string()])?;
    tx.commit()?;
    Ok(true)
}
