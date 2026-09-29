//! Explicit operator stop-loss. Venue positions drive reducing orders, never PnL bookkeeping.
//! Original requests remain queryable while each venue is independently flattened.
use super::{store::Store, venue::{AccountWorker, LookupEvidence}, *};
use anyhow::{ensure, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::{Arc, RwLock}};

pub const SLIPPAGE_BPS: u32 = 500;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedOrder { pub request: OrderRequest, pub terminal: bool }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmergencyExit {
    pub id: String,
    pub requested_ms: u64,
    pub completed_ms: Option<u64>,
    pub flat_confirmed_ms: Option<u64>,
    pub prior: Vec<TrackedOrder>,
    pub orders: Vec<TrackedOrder>,
    pub fills: BTreeMap<String, Fill>,
    pub remaining_units: [Option<i64>; 2],
    pub observed_ms: [u64; 2],
    pub warnings: [String; 2],
    pub next_attempt_ms: [u64; 2],
    pub accounting_error: String,
    pub next_sequence: u64,
}
pub fn active(s: &Snapshot) -> bool { s.emergency_exit.as_ref().is_some_and(|x| x.completed_ms.is_none()) }
pub fn latch(s: &mut Snapshot, now: u64) -> Result<()> {
    if active(s) { return Ok(()); }
    // Completed journals stay in snapshot history; new incidents use fresh IDs.
    let mut prior=Vec::new();
    if let Some(p)=&s.pending {
        for (r,terminal) in [(&p.first,p.first_terminal),(&p.hedge,p.hedge_terminal),
            (&p.repair,p.repair_terminal),(&p.align_close,p.align_close_terminal),(&p.unwind_hedge,p.unwind_hedge_terminal)] {
            if let Some(r)=r { prior.push(TrackedOrder{request:r.clone(),terminal}); }
        }
    }
    if let Some(p)=&s.live_orphan {
        for a in &p.attempts { prior.push(TrackedOrder{request:a.request.clone(),terminal:a.result.as_ref().is_some_and(|r|r.terminal)}); }
    }
    if let Some(p)=&s.liquidation_protection {
        for a in p.orders.iter().flatten() { prior.push(TrackedOrder{request:a.request.clone(),terminal:a.terminal}); }
    }
    s.emergency_exit=Some(EmergencyExit{id:format!("{}-emergency-{}",s.instance_id,uuid::Uuid::new_v4()),
        requested_ms:now,completed_ms:None,flat_confirmed_ms:None,prior,orders:vec![],fills:BTreeMap::new(),
        remaining_units:[None,None],observed_ms:[0,0],warnings:Default::default(),next_attempt_ms:[0,0],accounting_error:String::new(),next_sequence:0});
    s.paused=true;s.stop_requested=true;s.resume_after_recovery=false;s.close_requested=true;s.stop_after_close=true;
    s.recovery_after_ms=None;s.entry_attempts_remaining=Some(0);s.status=Status::Closing;
    s.reason="紧急全部平仓已启动：5% 滑点上限，两平台独立减仓".into();
    Ok(())
}
fn trusted(s:&Snapshot,a:&AccountEvidence,now:u64)->Result<()> {
    let expected=if a.venue==Venue::Lighter {&s.config.lighter_account} else {&s.config.entropy_account};
    ensure!(a.authenticated && &a.account==expected && a.observed_ms<=now
        && now-a.observed_ms<=s.config.account_max_age_ms,"紧急平仓等待本账户的新鲜可信持仓");
    ensure!(a.open_orders==0,"账户仍有挂单，等待原订单终态；不会重复提交");
    Ok(())
}
fn request(s:&Snapshot,a:&AccountEvidence,b:&Book,now:u64)->Result<OrderRequest> {
    trusted(s,a,now)?;b.validate(now,s.config.book_max_age_ms)?;
    let units=a.position_units.checked_abs().ok_or_else(||anyhow::anyhow!("invalid position"))?;
    ensure!(units>0 && units%s.config.market.venue_step(a.venue)==0,"账户数量不符合平台精度");
    let side=if a.position_units>0 {Side::Sell}else{Side::Buy};
    let top=if side==Side::Buy {b.asks[0].price}else{b.bids[0].price};
    let limit=top*(Decimal::ONE+Decimal::from(side.sign())*Decimal::from(SLIPPAGE_BPS)/Decimal::from(10_000));
    let e=s.emergency_exit.as_ref().unwrap();
    Ok(OrderRequest{id:format!("{}-v2-{}-{:?}",e.id,e.next_sequence,a.venue),venue:a.venue,side,units,
        limit:s.config.market.protected_price(a.venue,limit,side==Side::Buy)?,arrival_mid:b.mid(),reduce_only:true,
        created_ms:now,expires_ms:now+s.config.operation_timeout_ms,
        signed_expires_ms:(a.venue==Venue::Lighter).then_some(now+599_000)})
}
fn save_result(s:&mut Snapshot,r:&OrderRequest,result:OrderResult)->Result<()> {
    let start=result.exchange_created_ms.map(|t|r.verified_exchange_created(t)).transpose()?.unwrap_or(r.created_ms);
    let mut next=s.emergency_exit.clone().unwrap();
    let mut seen=std::collections::BTreeSet::new();
    for f in &result.fills {
        ensure!(f.order_id==r.id && f.venue==r.venue && f.side==r.side && f.units>0 && f.price>Decimal::ZERO
            && f.fee>=Decimal::ZERO && !f.id.is_empty() && f.time_ms>=start.saturating_sub(1000)
            && f.time_ms<=r.signed_expiry().saturating_add(300_000),"紧急平仓回执身份、数量或时间校验失败");
        let key=format!("{:?}:{}",f.venue,f.id);
        ensure!(seen.insert(key.clone()),"duplicate fill in response");
        if let Some(old)=s.fills.get(&key).or(next.fills.get(&key)) {ensure!(old==f,"changed duplicate emergency fill");}
        if !s.fills.contains_key(&key) {next.fills.insert(key,f.clone());}
    }
    let total:i64=s.fills.values().chain(next.fills.values()).filter(|f|f.venue==r.venue && f.order_id==r.id).map(|f|f.units).sum();
    ensure!(total<=r.units,"emergency fills exceed request");
    let order=next.orders.iter_mut().chain(next.prior.iter_mut()).find(|o|o.request.id==r.id).ok_or_else(||anyhow::anyhow!("untracked emergency request"))?;
    order.terminal |= result.terminal;
    s.emergency_exit=Some(next);Ok(())
}
fn evidence(s:&Snapshot,r:&OrderRequest)->LookupEvidence {
    let e=s.emergency_exit.as_ref().unwrap();
    let mut proof=LookupEvidence::from_snapshot(s,r);
    for f in e.fills.values().filter(|f|f.venue==r.venue) {
        proof.position_units+=f.units*f.side.sign();
        if f.time_ms>=r.created_ms.saturating_sub(300_000) {proof.known_fills.push(f.clone());}
    }
    proof
}
/// Fills are committed in exchange-time order only after all original orders are known.
/// Account snapshots NEVER create fictional fills or overwrite ledger positions.
fn finish(s:&mut Snapshot,now:u64)->Result<()> {
    let e=s.emergency_exit.as_ref().unwrap();
    if !e.prior.iter().chain(e.orders.iter()).all(|o|o.terminal) {return Ok(());}
    if e.remaining_units!=[Some(0),Some(0)] || e.observed_ms.iter().any(|t|*t>now || now-*t>s.config.account_max_age_ms) {return Ok(());}
    let mut next=s.clone();
    let mut fills=e.fills.values().collect::<Vec<_>>();fills.sort_by(|a,b|(a.time_ms,&a.id).cmp(&(b.time_ms,&b.id)));
    for f in fills {
        let r=e.orders.iter().chain(e.prior.iter()).find(|o|o.request.id==f.order_id).unwrap();
        next.record_fill(f,r.request.arrival_mid)?;
    }
    ensure!(next.positions.iter().all(|p|p.units==0),"两平台已无持仓，但历史成交仍不完整；保留账本等待核对");
    // Attribute known closing cash to old lots per venue, including a first leg already
    // filled before the emergency. Excess from an unfinished entry remains unallocated.
    let mut lots=s.lots.clone();
    if let Some(p)=s.pending.as_ref().filter(|p|p.action==Action::Close) {
        lots.sort_by_key(|l|p.close_allocations.iter().position(|a|a.lot_id==l.id)
            .unwrap_or(if p.close_lot_id.as_ref()==Some(&l.id){0}else{usize::MAX}));
    }
    let mut remaining: [Vec<i64>;2]=[lots.iter().map(|l|l.units).collect(),lots.iter().map(|l|l.units).collect()];
    let prior_closes_inventory=s.pending.as_ref().is_none_or(|p|p.action==Action::Close);
    let mut closes=next.fills.values().filter(|f|e.orders.iter().chain(e.prior.iter().filter(|_|prior_closes_inventory)).any(|o|
        o.request.id==f.order_id && o.request.reduce_only)).cloned().collect::<Vec<_>>();
    closes.sort_by(|a,b|(a.time_ms,&a.id).cmp(&(b.time_ms,&b.id)));
    for f in closes {
        let mut left=f.units;let mut allocation=vec![];
        for (j,l) in lots.iter().enumerate() {
            let take=left.min(remaining[f.venue.index()][j]);
            if take>0 {allocation.push(CloseAllocation{lot_id:l.id.clone(),units:take});remaining[f.venue.index()][j]-=take;left-=take;}
        }
        next.emergency_fill_allocations.insert(format!("{:?}:{}",f.venue,f.id),allocation);
    }
    ensure!(remaining.iter().flatten().all(|q|*q==0),"历史持仓的平仓成交仍未完整归属");
    if !lots.is_empty() {next.closed_lot_allocations.insert(e.id.clone(),lots.iter().map(|l|CloseAllocation{lot_id:l.id.clone(),units:l.units}).collect());}
    // All original paired ownership is retired only after authentic fills and actual flatness agree.
    next.closed_groups+=next.lots.len() as u64;
    next.lots.clear();next.pending=None;next.anchor=None;next.last_open_completed=None;next.time_adds_used=0;
    next.previous_group_exit.clear();next.previous_exit=None;next.exit_batch_active=false;
    next.close_requested=false;next.stop_after_close=false;next.status=Status::Stopped;
    next.emergency_exit.as_mut().unwrap().completed_ms=Some(now);
    next.reason="紧急全部平仓完成：两平台持仓为零，已停止交易".into();
    *s=next;Ok(())
}

/// Each venue progresses even if the other venue or the old paired operation is blocked.
/// Every intent is durable before sending; an unknown emergency order is only looked up.
pub async fn advance(s:&mut Snapshot,store:&mut Store,workers:&[AccountWorker;2],books:&Arc<RwLock<[Book;2]>>) -> Result<[Option<AccountEvidence>;2]> {
    let mut observed=[None,None];
    let mut submitted=[false,false];
    if !active(s) {return Ok(observed);}
    s.paused=true;s.stop_requested=true;s.resume_after_recovery=false;
    enum Event { Account(Venue,Result<AccountEvidence>), Order(OrderRequest,Result<OrderResult>) }
    let mut jobs:FuturesUnordered<std::pin::Pin<Box<dyn std::future::Future<Output=Event>+Send>>>=FuturesUnordered::new();
    for venue in [Venue::Lighter,Venue::Entropy] {
        let w=workers[venue.index()].clone();
        let e=s.emergency_exit.as_ref().unwrap();
        if let Some(o)=e.orders.iter().rev().find(|o|o.request.venue==venue && !o.terminal) {
            let r=o.request.clone();let proof=evidence(s,&r);
            jobs.push(Box::pin(async move {let result=w.lookup_reconciled(r.clone(),proof).await;Event::Order(r,result)}));
        } else {
            jobs.push(Box::pin(async move {Event::Account(venue,w.reconcile_account().await)}));
        }
    }
    while let Some(event)=jobs.next().await {
        let now=crate::domain::now_ms();
        match event {
            Event::Account(venue,result)=>{
                let i=venue.index();
                let outcome=(||->Result<Option<OrderRequest>> {
                    let a=result?;ensure!(a.venue==venue,"account venue mismatch");trusted(s,&a,now)?;
                    let e=s.emergency_exit.as_mut().unwrap();
                    e.remaining_units[i]=Some(a.position_units);e.observed_ms[i]=a.observed_ms;e.warnings[i].clear();
                    if a.position_units!=0 {e.flat_confirmed_ms=None;}
                    observed[i]=Some(a.clone());
                    if submitted[i] || a.position_units==0 || now<e.next_attempt_ms[i]
                        || e.orders.iter().any(|o|o.request.venue==venue && !o.terminal) {return Ok(None);}
                    request(s,&a,&books.read().unwrap()[i],now).map(Some)
                })();
                match outcome {
                    Ok(Some(r))=>{
                        submitted[i]=true;
                        let e=s.emergency_exit.as_mut().unwrap();e.flat_confirmed_ms=None;e.next_attempt_ms[i]=now+1000;
                        e.next_sequence=e.next_sequence.checked_add(1).ok_or_else(||anyhow::anyhow!("emergency sequence overflow"))?;
                        e.orders.push(TrackedOrder{request:r.clone(),terminal:false});
                        store.commit(s,now,"emergency_exit_dispatch")?;
                        let w=workers[i].clone();jobs.push(Box::pin(async move {let result=w.submit(r.clone()).await;Event::Order(r,result)}));
                    },
                    Ok(None)=>{},Err(err)=>{s.emergency_exit.as_mut().unwrap().warnings[i]=format!("{err:#}");}
                }
            },
            Event::Order(r,result)=>{
                let i=r.venue.index();
                match result.and_then(|result|save_result(s,&r,result)) {
                    Ok(())=>{
                        let e=s.emergency_exit.as_mut().unwrap();e.warnings[i].clear();
                        let filled=e.fills.values().any(|f|f.venue==r.venue && f.order_id==r.id);
                        e.next_attempt_ms[i]=now+if filled{1000}else{5000};
                    },
                    Err(err)=>{s.emergency_exit.as_mut().unwrap().warnings[i]=format!("订单结果待核对：{err:#}");}
                }
                // Never use the pre-order snapshot as proof that the venue is flat.
                s.emergency_exit.as_mut().unwrap().observed_ms[i]=0;
                store.commit(s,now,"emergency_exit_order_observed")?;
                let w=workers[i].clone();let venue=r.venue;
                jobs.push(Box::pin(async move {Event::Account(venue,w.reconcile_account().await)}));
            }
        }
    }
    // Old unknown orders do not prevent reducing an already observed position above.
    // In particular, a late opening fill is closed on the next pass before completion.
    let e=s.emergency_exit.as_ref().unwrap();
    let old=if e.remaining_units==[Some(0),Some(0)] {e.prior.iter().filter(|o|!o.terminal).cloned().collect::<Vec<_>>()} else {vec![]};
    for o in old {
        let proof=evidence(s,&o.request);
        match workers[o.request.venue.index()].lookup_reconciled(o.request.clone(),proof).await
            .and_then(|r|save_result(s,&o.request,r)) {
            Ok(())=>{}, Err(err)=>{s.emergency_exit.as_mut().unwrap().warnings[o.request.venue.index()]=format!("持仓正在独立减仓；旧订单记录待核对：{err:#}");}
        }
        let e=s.emergency_exit.as_mut().unwrap();e.observed_ms[o.request.venue.index()]=0;e.flat_confirmed_ms=None;
        store.commit(s,crate::domain::now_ms(),"emergency_exit_prior_observed")?;
    }
    let now=crate::domain::now_ms();
    let e=s.emergency_exit.as_mut().unwrap();
    if e.remaining_units==[Some(0),Some(0)] && e.observed_ms.iter().all(|t|*t<=now && now-*t<=s.config.account_max_age_ms) {
        e.flat_confirmed_ms=Some(now);
    }
    if let Err(err)=finish(s,now) {s.emergency_exit.as_mut().unwrap().accounting_error=err.to_string();}
    if active(s) {
        s.status=Status::Closing;s.reason="紧急全部平仓进行中：5% 滑点上限，按实际剩余持仓持续减仓".into();
        if s.emergency_exit.as_ref().unwrap().flat_confirmed_ms.is_some() {s.reason="两平台已无持仓，正在核对原订单和成交记录；新交易保持停止".into();}
    }
    let e=s.emergency_exit.as_mut().unwrap();
    // Bound rejected/empty IOC history; identities are monotonic, never reused.
    let keep_from=e.orders.len().saturating_sub(32);let mut n=0;
    e.orders.retain(|o|{let keep=n>=keep_from || !o.terminal || e.fills.values().any(|f|f.order_id==o.request.id);n+=1;keep});
    let kind=if e.completed_ms.is_some(){"emergency_exit_completed"}else{"sample"};
    store.commit(s,now,kind)?;
    Ok(observed)
}
