//! Transactional, credential-free exception outbox. No HTTP in the trading actor.
use super::*;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS notification_state(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS notification_outbox(id TEXT PRIMARY KEY,at_ms INTEGER NOT NULL,body TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,next_ms INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS notification_counters(id INTEGER PRIMARY KEY CHECK(id=1),dropped INTEGER NOT NULL DEFAULT 0);
INSERT OR IGNORE INTO notification_counters(id,dropped) VALUES(1,0);";
const MAX_PENDING: i64 = 512;
const REMINDER_MS: u64 = 300_000;

#[derive(Clone, Serialize, Deserialize)]
struct Incident { key:String, action:String, started_ms:u64, notice_ms:u64, urgent:bool }

fn issue(s:&Snapshot) -> Option<(&'static str,bool)> {
    if s.live_orphan.is_some() {return Some(("出现未归属或不一致仓位，需要人工核对",true));}
    if s.loss_stop.is_some() && !s.lots.is_empty() {return Some(("已触发总亏损保护，正在受控平仓",true));}
    if s.status==Status::NeedsAttention && !(s.pending.is_none() && s.reason.starts_with("execution recovered;")) {
        return Some(("执行或持仓核对异常，需要处理",true));
    }
    let p=s.pending.as_ref()?;
    if p.failed || p.repair.is_some() || p.unwind_hedge.is_some() || p.align_close.is_some() {
        return Some(("双腿未按原计划成交，正在受控补偿或精度对齐",false));
    }
    if s.status==Status::RecoveringExposure {
        return Some(("订单结果尚未确认，只查询原订单，禁止重复提交",false));
    }
    if (p.first_terminal && p.first.as_ref().is_some_and(|r|p.first_filled!=r.units))
        || (p.hedge_terminal && p.hedge.as_ref().is_some_and(|r|p.hedge_filled!=r.units)) {
        return Some(("订单部分成交或未成交，正在核对实际数量",false));
    }
    if p.quote_wait_started_ms.is_some() && p.first_filled>0 {
        return Some(("一边已成交，另一边行情不可用，正在等待或补偿",false));
    }
    None
}
fn action(s:&Snapshot)->String {
    match s.pending.as_ref().map(|p|p.action) {
        Some(Action::Close)=>"平仓", Some(Action::Open) if !s.lots.is_empty()=>"加仓",
        Some(Action::Open)=>"开仓", _=>"持仓核对",
    }.into()
}
fn message(s:&Snapshot,incident:&Incident,at:u64,title:&str,detail:&str)->String {
    let time=chrono::DateTime::from_timestamp_millis(at as i64).map(|t|t.to_rfc3339()).unwrap_or_default();
    let progress=s.pending.as_ref().map(|p|format!("\n请求数量 {}；首腿确认 {}；配对腿确认 {}；补偿确认 {}",
        s.config.quantity(p.requested_units),s.config.quantity(p.first_filled),s.config.quantity(p.hedge_filled),s.config.quantity(p.repair_filled))).unwrap_or_default();
    format!("【多平台套利 · {title}】\n标的 {} · 模式 {:?} · {}\n{detail}\n账本数量：Lighter {} / Entropy {}{progress}\n订单尚未确认时，账本数量可能未包含最新成交。请在本机控制台核对账户；勿重复启动或手工重复下单。\n事件时间 {time}",
        s.config.market.id().to_uppercase(),s.config.mode,incident.action,
        s.config.quantity(s.positions[0].units),s.config.quantity(s.positions[1].units))
}
pub fn enqueue(tx:&Transaction<'_>,at:u64,body:&str)->Result<String> {
    let id=uuid::Uuid::new_v4().to_string();
    tx.execute("INSERT INTO notification_outbox(id,at_ms,body) VALUES(?1,?2,?3)",params![id,at,body])?;
    // A bounded queue cannot fill the user's disk during a long outage. The
    // public delivery status reports how many oldest notifications overflowed.
    let n=tx.execute("DELETE FROM notification_outbox WHERE id IN (SELECT id FROM notification_outbox ORDER BY at_ms DESC,rowid DESC LIMIT -1 OFFSET ?1)",[MAX_PENDING])?;
    tx.execute("UPDATE notification_counters SET dropped=dropped+?1 WHERE id=1",[n as i64])?;
    Ok(id)
}
pub fn observe(tx:&Transaction<'_>,s:&Snapshot,at:u64)->Result<()> {
    let at=if at==0 {crate::domain::now_ms()}else{at};
    let raw:Option<String>=tx.query_row("SELECT body FROM notification_state WHERE id=1",[],|r|r.get(0)).optional()?;
    let mut active:Option<Incident>=raw.as_ref().map(|v|serde_json::from_str::<Option<Incident>>(v)).transpose()?.flatten();
    if let Some((detail,urgent))=issue(s) {
        let key=s.pending.as_ref().map(|p|p.id.clone()).unwrap_or_else(||format!("{}-account",s.instance_id));
        let fresh=active.as_ref().is_none_or(|a|a.key!=key);
        let notice=fresh || active.as_ref().is_some_and(|a|(urgent&&!a.urgent)||at.saturating_sub(a.notice_ms)>=REMINDER_MS);
        let mut incident=if fresh {Incident{key,action:action(s),started_ms:at,notice_ms:at,urgent}}else{active.take().unwrap()};
        if notice {enqueue(tx,at,&message(s,&incident,at,if urgent {"需要处理"}else{"交易异常"},detail))?;incident.notice_ms=at;}
        incident.urgent|=urgent;active=Some(incident);
    } else if s.pending.is_none() && s.live_orphan.is_none() && s.positions[0].units==-s.positions[1].units {
        if let Some(incident)=active.take() {
            enqueue(tx,at,&message(s,&incident,at,"账本配平结果","该异常的订单账本已恢复配平；这不等于策略已重新启动，仍需以控制台账户核验结果为准。"))?;
        }
    }
    let body=serde_json::to_string(&active)?;
    if raw.as_deref()!=Some(&body) {tx.execute("INSERT INTO notification_state VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[body])?;}
    Ok(())
}
pub fn open_delivery(path:&std::path::Path)->Result<Connection> {
    let db=Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_millis(50))?;
    Ok(db)
}

#[cfg(test)]
#[path="alert_tests.rs"]
mod tests;
