//! Transactional, credential-free exception outbox. No HTTP in the trading actor.
use super::*;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS notification_state(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS notification_outbox(id TEXT PRIMARY KEY,at_ms INTEGER NOT NULL,body TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,next_ms INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS notification_counters(id INTEGER PRIMARY KEY CHECK(id=1),dropped INTEGER NOT NULL DEFAULT 0);
INSERT OR IGNORE INTO notification_counters(id,dropped) VALUES(1,0);
CREATE TABLE IF NOT EXISTS notification_policy(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL);
INSERT OR IGNORE INTO notification_policy(id,version) VALUES(1,0);";
const MAX_PENDING: i64 = 512;
const REMINDER_MS: u64 = 300_000;
pub const EXECUTION_GRACE_MS: u64 = 60_000;

#[derive(Clone, Serialize, Deserialize)]
struct Incident {
    key:String, action:String, started_ms:u64, notice_ms:u64, urgent:bool,
    #[serde(default)] outbox_id:Option<String>,
    #[serde(default)] body:String,
}

fn issue(s:&Snapshot) -> Option<(&'static str,bool)> {
    if super::emergency_exit::active(s) {return Some(("紧急全部平仓仍未完成，请查看两平台剩余持仓和订单核对进度",false));}
    if s.live_orphan.is_some() {return Some(("出现未归属或不一致仓位，需要人工核对",true));}
    if s.loss_stop.is_some() && !s.lots.is_empty() {return Some(("已触发总亏损保护，正在受控平仓",true));}
    if s.status==Status::NeedsAttention && !(s.pending.is_none() && s.reason.starts_with("execution recovered;")) {
        return Some(("执行或持仓核对异常，需要处理",s.pending.is_none()));
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
    if super::emergency_exit::active(s) {return "紧急全部平仓".into();}
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
fn cursor(tx:&Transaction<'_>)->Result<Option<Incident>> {
    let raw:Option<String>=tx.query_row("SELECT body FROM notification_state WHERE id=1",[],|r|r.get(0)).optional()?;
    Ok(raw.as_ref().map(|v|serde_json::from_str::<Option<Incident>>(v)).transpose()?.flatten())
}
fn save(tx:&Transaction<'_>,active:&Option<Incident>)->Result<()> {
    let body=serde_json::to_string(active)?;
    tx.execute("INSERT INTO notification_state VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body WHERE body<>excluded.body",[body])?;
    Ok(())
}
fn cancel(tx:&Transaction<'_>,active:&Option<Incident>)->Result<()> {
    if let Some(id)=active.as_ref().and_then(|a|a.outbox_id.as_ref()) {
        tx.execute("DELETE FROM notification_outbox WHERE id=?1",[id])?;
    }
    Ok(())
}
fn promote(tx:&Transaction<'_>,a:&mut Incident,at:u64)->Result<()> {
    if a.body.is_empty() || (!a.urgent && at.saturating_sub(a.started_ms)<EXECUTION_GRACE_MS) {return Ok(());}
    if let Some(id)=&a.outbox_id {
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE id=?1)",[id],|r|r.get(0))?;
        if exists {return Ok(());}
    }
    if a.notice_ms!=0 && at.saturating_sub(a.notice_ms)<REMINDER_MS {return Ok(());}
    let body=format!("{}\n异常已持续至少 {} 秒；请运行程序目录中的一键诊断，发送生成的诊断包。",a.body,at.saturating_sub(a.started_ms)/1000);
    a.outbox_id=Some(enqueue(tx,at,&body)?);a.notice_ms=at;
    Ok(())
}
/// Delivery timer matures the durable incident even if quotes/actor commits stop.
/// Only notification tables are touched; no strategy state or trade commands.
pub fn tick(tx:&Transaction<'_>,at:u64)->Result<()> {
    let mut active=cursor(tx)?;
    if let Some(a)=&mut active {promote(tx,a,at)?;save(tx,&active)?;}
    Ok(())
}
pub fn delivered(tx:&Transaction<'_>,id:&str,at:u64)->Result<()> {
    let mut active=cursor(tx)?;
    if let Some(a)=&mut active {
        if a.outbox_id.as_deref()==Some(id) {a.outbox_id=None;a.notice_ms=at;save(tx,&active)?;}
    }
    Ok(())
}
pub fn queued(path:&std::path::Path,id:&str)->Result<bool> {
    let db=open_delivery(path)?;
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE id=?1)",[id],|r|r.get(0))?)
}
pub fn observe(tx:&Transaction<'_>,s:&Snapshot,at:u64)->Result<()> {
    let at=if at==0 {crate::domain::now_ms()}else{at};
    let mut active=cursor(tx)?;
    let policy:u32=tx.query_row("SELECT version FROM notification_policy WHERE id=1",[],|r|r.get(0))?;
    if policy<2 {
        // Obsolete rc.8 execution/recovery messages must not be replayed after
        // upgrade. Re-observe the actual durable incident under the new policy.
        tx.execute("DELETE FROM notification_outbox WHERE body LIKE '【多平台套利 · 交易异常】%' OR body LIKE '【多平台套利 · 需要处理】%' OR body LIKE '【多平台套利 · 账本配平结果】%'",[])?;
        if let Some(a)=&mut active {a.notice_ms=0;a.outbox_id=None;}
        tx.execute("UPDATE notification_policy SET version=2 WHERE id=1",[])?;
    }
    if let Some((detail,urgent))=issue(s) {
        let key=s.emergency_exit.as_ref().filter(|_|super::emergency_exit::active(s)).map(|e|e.id.clone()).or_else(||s.pending.as_ref().map(|p|p.id.clone())).unwrap_or_else(||format!("{}-account",s.instance_id));
        let fresh=active.as_ref().is_none_or(|a|a.key!=key);
        if fresh {cancel(tx,&active)?;}
        let mut incident=if fresh {Incident{key,action:action(s),started_ms:at,notice_ms:0,urgent,outbox_id:None,body:String::new()}}else{active.take().unwrap()};
        // A transition to needs_attention during the same operation does not
        // bypass the grace period. Independent emergencies still alert at once.
        if urgent && !incident.urgent {cancel(tx,&Some(incident.clone()))?;incident.outbox_id=None;incident.notice_ms=0;}
        incident.urgent=urgent;
        incident.body=message(s,&incident,at,if urgent {"需要处理"}else{"持续交易异常"},detail);
        promote(tx,&mut incident,at)?;active=Some(incident);
    } else if s.pending.is_none() && s.live_orphan.is_none() && s.positions[0].units==-s.positions[1].units {
        cancel(tx,&active)?;active=None;
    } else if s.pending.as_ref().is_some_and(|p|active.as_ref().is_some_and(|a|a.key!=p.id)) {
        cancel(tx,&active)?;active=None;
    }
    save(tx,&active)?;
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
