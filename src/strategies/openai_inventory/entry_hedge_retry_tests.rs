use super::*;
use std::sync::{Arc,Mutex};
use std::collections::VecDeque;

fn fixture(now:u64,market:MarketPair,direction:Direction)->(Snapshot,[AccountEvidence;2],[Book;2]) {
    let mut c=InventoryConfig::default();c.market=market;c.direction_policy=DirectionPolicy::Both;
    c.max_loss_usdc=Decimal::from(30);
    let mut s=Snapshot::new(c).unwrap();s.direction=direction;s.status=Status::Recovering;
    s.paused=false;s.stop_requested=false;
    let step=market.common_step();let qty=step*18;
    let l=if direction==Direction::LighterShort {2020}else{1980};
    for v in [Venue::Lighter,Venue::Entropy] {
        s.record_fill(&Fill{id:format!("old-{v:?}"),order_id:"old-v2-first".into(),venue:v,
            side:direction.open_side(v),units:step*10,price:Decimal::from(if v==Venue::Entropy {2000}else{l}),
            fee:Decimal::ZERO,time_ms:now-20_000},None).unwrap();
    }
    s.lots.push(Lot{id:"old".into(),level:0,units:step*10,opened_ms:now-20_000,
        entry_spread:Decimal::from(20),entry_net_spread:Some(Decimal::from(20))});
    s.anchor=Some(Decimal::from(20));s.opened_groups=1;
    let req=|name:&str,v:Venue|OrderRequest{id:format!("retry-operation-v2-{name}"),venue:v,
        side:direction.open_side(v),units:qty,limit:Decimal::from(if v==Venue::Entropy {2000}else{l}),
        arrival_mid:None,reduce_only:false,created_ms:now-1000,expires_ms:now+4000,
        signed_expires_ms:(v==Venue::Lighter).then_some(now+598_000)};
    let first=req("first",Venue::Entropy);let hedge=req("hedge",Venue::Lighter);
    s.record_fill(&Fill{id:"first-fill".into(),order_id:first.id.clone(),venue:Venue::Entropy,
        side:first.side,units:qty,price:Decimal::from(2000),fee:Decimal::ZERO,time_ms:now-900},None).unwrap();
    let mut p:Operation=serde_json::from_value(serde_json::json!({"id":"retry-operation","action":"open",
        "level":1,"requested_units":qty,"created_ms":now-2000,"first":first,"hedge":hedge,"repair":null,
        "first_terminal":true,"hedge_terminal":true,"repair_terminal":false,
        "first_filled":qty,"hedge_filled":0,"repair_filled":0,"first_value":(2000*qty).to_string(),
        "hedge_value":"0","failed":false,"first_venue":"entropy","min_entry_spread":"10"})).unwrap();
    p.entry_hedge_retry=Some(Default::default());s.pending=Some(p);
    let a=evidence(&s,now);
    let b=[l,2000].map(|price|Book{bids:vec![Level{price:Decimal::from(price),units:step*1000}],
        asks:vec![Level{price:Decimal::from(price)+Decimal::new(1,1),units:step*1000}],connected:true,received_ms:now});
    (s,a,b)
}
fn evidence(s:&Snapshot,now:u64)->[AccountEvidence;2] {
    [Venue::Lighter,Venue::Entropy].map(|venue|AccountEvidence{venue,account:"o1".into(),
        observed_ms:now,position_units:s.positions[venue.index()].units,free_margin:Decimal::from(100),
        equity:Decimal::from(200),leverage:3,isolated:true,open_orders:0,authenticated:true,liquidation_price:None})
}
fn fresh(b:&[Book;2],now:u64)->[Book;2] {let mut b=b.clone();for x in &mut b {x.received_ms=now;}b}
fn fill(r:&OrderRequest,units:i64,terminal:bool)->OrderResult {
    OrderResult{exchange_created_ms:None,terminal,reason:"synthetic IOC".into(),fills:if units==0 {vec![]}else{vec![Fill{
        id:format!("{}-fill",r.id),order_id:r.id.clone(),venue:r.venue,side:r.side,units,
        price:r.limit,fee:Decimal::ZERO,time_ms:r.created_ms}]}}
}
type Calls=Arc<Mutex<Vec<(bool,OrderRequest)>>>;
struct Backend {calls:Calls,results:Arc<Mutex<VecDeque<Option<i64>>>>}
impl venue::VenueBackend for Backend {
    fn submit(&mut self,r:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        self.calls.lock().unwrap().push((true,r.clone()));
        let qty=if r.reduce_only {Some(r.units)}else{self.results.lock().unwrap().pop_front().unwrap_or(Some(-1))};
        Box::pin(async move{Ok(fill(&r,qty.map_or(0,|q|if q<0 {r.units}else{q}),qty.is_some()))})
    }
    fn lookup(&mut self,r:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        self.calls.lock().unwrap().push((false,r.clone()));
        Box::pin(async move{Ok(fill(&r,0,false))})
    }
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence> {Box::pin(async{anyhow::bail!("fixture supplies evidence")})}
}
fn workers(results:Vec<Option<i64>>)->([AccountWorker;2],Calls) {
    let calls=Arc::new(Mutex::new(vec![]));let results=Arc::new(Mutex::new(results.into()));
    let w=[Venue::Lighter,Venue::Entropy].map(|v|AccountWorker::spawn(v,Mode::Paper,true,
        Box::new(Backend{calls:calls.clone(),results:results.clone()})).unwrap());(w,calls)
}

#[tokio::test]
async fn tenfold_partial_hedge_retries_only_unfilled_units_with_the_same_slippage_ladder() {
    let mut now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
    s.config.set_initial_capital(10).unwrap();s.config.validate().unwrap();
    for p in &mut s.positions {p.units*=10;}for l in &mut s.lots {l.units*=10;}
    for f in s.fills.values_mut() {f.units*=10;}
    let p=s.pending.as_mut().unwrap();p.requested_units*=10;p.first_filled*=10;p.first_value*=Decimal::from(10);
    for r in [&mut p.first,&mut p.hedge,&mut p.repair].into_iter().flatten(){r.units*=10;}
    let requested=p.requested_units;let part=requested/3;let old_lot=s.lots[0].clone();
    let (w,calls)=workers(vec![Some(part),Some(-1)]);let (mut db,_)=Store::offline_replay(&s.config).unwrap();
    for attempt in 1..=2 {
        advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();now+=3000;
        let mut a=evidence(&s,now);for x in &mut a{x.free_margin=Decimal::from(1000);x.equity=Decimal::from(1100);}
        advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,now),now,Some(&a)).await.unwrap();
        let r=calls.lock().unwrap().last().unwrap().1.clone();
        assert_eq!(r.units,if attempt==1 {requested}else{requested-part});
        assert_eq!(r.limit,b[0].bids[0].price*(Decimal::ONE-Decimal::from(attempt+1)/Decimal::from(10000)));
    }
    advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
    assert!(s.pending.is_none());assert_eq!(s.positions[0].units,-s.positions[1].units);
    assert_eq!(s.lots[1].units,requested);assert_eq!(s.lots[0].units,old_lot.units);
}

#[tokio::test]
async fn each_step_can_complete_entry_in_both_markets_and_directions() {
    for m in [MarketPair::Openai,MarketPair::Anth] {for d in [Direction::LighterLong,Direction::LighterShort] {
        for success in 1..=4 {
            let mut now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,m,d);
            let mut responses=vec![Some(0);success-1];responses.push(Some(-1));
            let (w,calls)=workers(responses);let (mut db,_)=Store::offline_replay(&s.config).unwrap();
            for attempt in 1..=success {
                advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
                let at=s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().after_ms.unwrap();
                assert_eq!(at,now+3000);
                let early=evidence(&s,at-1);
                advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,at-1),at-1,Some(&early)).await.unwrap();
                assert_eq!(calls.lock().unwrap().len(),attempt-1);
                now=at;let a=evidence(&s,now);
                advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,now),now,Some(&a)).await.unwrap();
                let r=calls.lock().unwrap().last().unwrap().1.clone();
                let best=if r.side==Side::Buy {b[0].asks[0].price}else{b[0].bids[0].price};
                let expected=best*(Decimal::ONE+Decimal::from(r.side.sign())*Decimal::from(attempt+1)/Decimal::from(10000));
                let expected=if m==MarketPair::Anth {m.protected_price(Venue::Lighter,expected,r.side==Side::Buy).unwrap()}else{expected};
                assert_eq!(r.limit,expected);assert_eq!(r.id,format!("retry-operation-v2-hedge-retry-{attempt}"));
                assert!(!r.reduce_only);assert_eq!(r.units,m.common_step()*18);
            }
            advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
            assert!(s.pending.is_none());assert_eq!(s.lots.len(),2);assert_eq!(s.opened_groups,2);
            assert_eq!(s.consecutive_rollbacks,0);assert_eq!(s.positions[0].units,-s.positions[1].units);
        }
    }}
}

#[tokio::test]
async fn partial_fills_only_retry_remainder_and_duplicate_receipts_do_not_overcount() {
    for m in [MarketPair::Openai,MarketPair::Anth] {for d in [Direction::LighterLong,Direction::LighterShort] {
        let mut now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,m,d);let step=m.common_step();
        let original=s.lots[0].clone();let (w,calls)=workers(vec![Some(1),Some(step*6-1),Some(-1)]);
        let dir=std::env::temp_dir().join(format!("cpa-hedge-test-{}",uuid::Uuid::new_v4()));let path=dir.join("ledger.sqlite");
        let (mut db,_)=Store::open(&path,&s.config).unwrap();
        for n in 0..3 {
            advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
            now+=3000;let a=evidence(&s,now);
            advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,now),now,Some(&a)).await.unwrap();
            let r=s.pending.as_ref().unwrap().hedge.clone().unwrap();
            let qty=if n==0 {1}else if n==1 {step*6-1}else{r.units};
            let before=s.pending.as_ref().unwrap().hedge_filled;
            apply(&mut s,1,&r,fill(&r,qty,true)).unwrap();assert_eq!(s.pending.as_ref().unwrap().hedge_filled,before);
            db.commit(&s,now,"synthetic_duplicate").unwrap();drop(db);
            let loaded=Store::open(&path,&s.config).unwrap();db=loaded.0;s=loaded.1;
        }
        let requests=calls.lock().unwrap().iter().map(|(_,r)|r.units).collect::<Vec<_>>();
        assert_eq!(requests,vec![step*18,step*18-1,step*12]);
        advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
        assert!(s.pending.is_none());assert_eq!(s.lots.len(),2);assert_eq!(s.lots[1].units,step*18);
        assert_eq!(serde_json::to_value(&s.lots[0]).unwrap(),serde_json::to_value(original).unwrap());
        assert_eq!(s.positions[0].units.abs(),step*28);assert_eq!(s.positions[0].units,-s.positions[1].units);
        drop(db);std::fs::remove_dir_all(dir).unwrap();
    }}
}

#[tokio::test]
async fn four_failures_then_only_unpaired_exposure_is_unwound_counting_one_rollback() {
    for m in [MarketPair::Openai,MarketPair::Anth] {
        let mut now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,m,Direction::LighterShort);
        let old=serde_json::to_value(&s.lots).unwrap();let (w,calls)=workers(vec![Some(0);4]);
        let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        for _ in 0..4 {
            advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();now+=3000;
            let a=evidence(&s,now);
            advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,now),now,Some(&a)).await.unwrap();
        }
        advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
        let sent=calls.lock().unwrap();assert_eq!(sent.len(),5);
        assert!(sent[..4].iter().all(|(_,r)|r.venue==Venue::Lighter && !r.reduce_only));
        assert!(sent[4].1.reduce_only && sent[4].1.venue==Venue::Entropy);drop(sent);
        advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();
        assert!(s.pending.is_none());assert_eq!(serde_json::to_value(&s.lots).unwrap(),old);
        assert_eq!(s.consecutive_rollbacks,1);assert_eq!(s.positions[0].units,-s.positions[1].units);
    }
}

#[tokio::test]
async fn unknown_retry_is_lookup_only_even_after_restart_and_deadline() {
    let now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
    let (w,calls)=workers(vec![None]);let (mut db,_)=Store::offline_replay(&s.config).unwrap();
    advance(&mut s,&mut db,&w,&b,now).await.unwrap();let at=now+3000;let a=evidence(&s,at);
    advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,at),at,Some(&a)).await.unwrap();
    let req=serde_json::to_value(&s.pending.as_ref().unwrap().hedge).unwrap();
    for offset in [6000,9000,700000,703000] {
        s=serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert!(!due(&s,now+offset));
        if s.status==Status::NeedsAttention {recheck_timed_out(&mut s,&mut db,&w,now+offset).await.unwrap();}
        else {advance(&mut s,&mut db,&w,&fresh(&b,now+offset),now+offset).await.unwrap();}
        assert_eq!(serde_json::to_value(&s.pending.as_ref().unwrap().hedge).unwrap(),req);
        assert!(s.pending.as_ref().unwrap().repair.is_none());
    }
    assert_eq!(calls.lock().unwrap().iter().filter(|(submit,_)|*submit).count(),1);
}

#[test]
fn fresh_owned_terminal_evidence_is_required_without_mutation_on_rejection() {
    for case in 0..10 {
        let now=100_000;let (mut s,mut a,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
        s.pending.as_mut().unwrap().entry_hedge_retry.as_mut().unwrap().after_ms=Some(now);
        match case {
            0=>a[0].authenticated=false,1=>a[1].observed_ms=now-3001,2=>a[1].observed_ms=now+1,
            3=>a[0].position_units+=1,4=>a[0].open_orders=1,5=>a[0].account="other".into(),
            6=>a.swap(0,1),7=>{s.positions[0].units-=1;a[0].position_units-=1;},
            8=>s.pending.as_mut().unwrap().first_filled+=1,_=>a[1].observed_ms=0,
        }
        let (mut db,_)=Store::offline_replay(&s.config).unwrap();let before=serde_json::to_value(&s).unwrap();
        assert!(prepare(&mut s,&mut db,&b,now,Some(&a)).is_err(),"case {case}");
        assert_eq!(serde_json::to_value(s).unwrap(),before);
    }
}

#[tokio::test]
async fn stop_pause_loss_and_low_margin_abandon_retries_and_only_reduce() {
    for case in 0..4 {
        let now=crate::domain::now_ms();let (mut s,mut a,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
        s.pending.as_mut().unwrap().entry_hedge_retry.as_mut().unwrap().after_ms=Some(now);
        match case {0=>s.stop_requested=true,1=>s.paused=true,
            2=>s.loss_stop=Some(LossStop{at_ms:now,net_pnl:Decimal::from(-30)}),_=>a[0].free_margin=Decimal::ZERO}
        let (w,calls)=workers(vec![]);let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        advance_with_hedge_evidence(&mut s,&mut db,&w,&b,now,Some(&a)).await.unwrap();
        assert!(s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().abandoned);
        assert_eq!(calls.lock().unwrap().len(),1);
        assert!(calls.lock().unwrap()[0].1.reduce_only);
    }
}

#[test]
fn depth_and_spread_budget_failures_are_bounded_without_submitting() {
    for budget in [true,false] {
        let mut now=100_000;let (mut s,_,mut b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
        if budget {s.pending.as_mut().unwrap().min_entry_spread=Some(Decimal::from(100));}
        else {b[0].bids[0].units=1;b[0].bids.push(Level{price:Decimal::from(2010),units:1000});}
        let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        for attempt in 1..=4 {
            if attempt==1 {assert!(prepare(&mut s,&mut db,&b,now,None).unwrap());}
            now+=3000;let a=evidence(&s,now);
            assert_eq!(prepare(&mut s,&mut db,&fresh(&b,now),now,Some(&a)).unwrap(),attempt<4);
            let p=s.pending.as_ref().unwrap();assert_eq!(p.entry_hedge_retry.as_ref().unwrap().attempt,attempt);
            assert_eq!(p.hedge.as_ref().unwrap().id,"retry-operation-v2-hedge");
        }
    }
}

#[test]
fn legacy_pending_repairs_and_closed_positions_do_not_opt_into_open_retries() {
    let now=100_000;let (s,a,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
    for case in 0..5 {
        let mut s=s.clone();let p=s.pending.as_mut().unwrap();
        match case {0=>p.entry_hedge_retry=None,1=>p.action=Action::Close,2=>p.hedge_terminal=false,
            3=>p.first_terminal=false,_=>{p.repair=p.first.clone();p.failed=true;}}
        let before=serde_json::to_value(&s).unwrap();let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        assert!(!due(&s,now));assert!(!prepare(&mut s,&mut db,&b,now,Some(&a)).unwrap());
        assert_eq!(serde_json::to_value(s).unwrap(),before);
    }
}

#[test]
fn unavailable_account_reads_delay_without_advancing_or_erasing_terminal_order() {
    let now=100_000;let (mut s,_,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
    let (mut db,_)=Store::offline_replay(&s.config).unwrap();prepare(&mut s,&mut db,&b,now,None).unwrap();
    let before=serde_json::to_value(&s.pending.as_ref().unwrap().hedge).unwrap();
    defer_account_read(&mut s,&mut db,now+3000).unwrap();
    let p=s.pending.as_ref().unwrap();assert_eq!(p.entry_hedge_retry.as_ref().unwrap().attempt,0);
    assert_eq!(p.entry_hedge_retry.as_ref().unwrap().after_ms,Some(now+6000));
    assert_eq!(serde_json::to_value(&p.hedge).unwrap(),before);
}

#[test]
fn stale_quotes_get_bounded_grace_then_abandon_without_advancing_price() {
    let start=100_000;let (mut s,_,b)=fixture(start,MarketPair::Openai,Direction::LighterShort);
    let (mut db,_)=Store::offline_replay(&s.config).unwrap();
    assert!(prepare(&mut s,&mut db,&b,start,None).unwrap());
    for offset in [3000,6000,9000] {
        let at=start+offset;let a=evidence(&s,at);
        assert_eq!(prepare(&mut s,&mut db,&b,at,Some(&a)).unwrap(),offset<9000);
        assert_eq!(s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().attempt,0);
    }
    assert!(s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().abandoned);
}

#[tokio::test]
async fn exhausted_partial_hedges_preserve_common_pairs_or_unwind_noncommon_fills() {
    for common in [true,false] {
        let mut now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
        let partial=if common {60}else{51};
        let (w,calls)=workers(vec![Some(partial),Some(0),Some(0),Some(0)]);
        let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        for _ in 0..4 {
            advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();now+=3000;let a=evidence(&s,now);
            advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,now),now,Some(&a)).await.unwrap();
        }
        for _ in 0..3 {advance(&mut s,&mut db,&w,&fresh(&b,now),now).await.unwrap();}
        assert!(s.pending.is_none());assert_eq!(s.positions[0].units,-s.positions[1].units);
        assert_eq!(s.positions[0].units.abs(),if common {160}else{100});
        assert_eq!(s.lots[0].units,100);assert_eq!(s.lots.len(),if common {2}else{1});
        let sent=calls.lock().unwrap();let reductions=sent.iter().filter(|(_,r)|r.reduce_only).map(|(_,r)|(r.venue,r.units)).collect::<Vec<_>>();
        assert_eq!(reductions,if common {vec![(Venue::Entropy,120)]}else{vec![(Venue::Lighter,51),(Venue::Entropy,180)]});
    }
}

#[tokio::test]
async fn failed_write_never_sends_and_lost_receipt_after_crash_never_resends() {
    for fail_kind in ["attempt_started","order_observed"] {
        let now=crate::domain::now_ms();let (mut s,_,b)=fixture(now,MarketPair::Openai,Direction::LighterShort);
        let dir=std::env::temp_dir().join(format!("cpa-hedge-disk-{}",uuid::Uuid::new_v4()));let path=dir.join("ledger.sqlite");
        let (mut db,_)=Store::open(&path,&s.config).unwrap();let (w,calls)=workers(vec![Some(-1)]);
        advance(&mut s,&mut db,&w,&b,now).await.unwrap();
        let sql=rusqlite::Connection::open(&path).unwrap();
        sql.execute_batch(&format!("CREATE TRIGGER fail_event BEFORE INSERT ON events WHEN NEW.kind='{fail_kind}' BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;")).unwrap();
        let at=now+3000;let a=evidence(&s,at);
        assert!(advance_with_hedge_evidence(&mut s,&mut db,&w,&fresh(&b,at),at,Some(&a)).await.is_err());
        assert_eq!(calls.lock().unwrap().len(),usize::from(fail_kind=="order_observed"));
        sql.execute_batch("DROP TRIGGER fail_event;").unwrap();drop(sql);drop(db);
        let (mut db,mut loaded)=Store::open(&path,&s.config).unwrap();
        if fail_kind=="order_observed" {
            assert!(!loaded.pending.as_ref().unwrap().hedge_terminal);
            assert_eq!(loaded.pending.as_ref().unwrap().hedge_filled,0);
            advance(&mut loaded,&mut db,&w,&fresh(&b,at),at).await.unwrap();
            assert_eq!(calls.lock().unwrap().iter().filter(|(submit,_)|*submit).count(),1);
        } else {
            assert!(loaded.pending.as_ref().unwrap().hedge_terminal);
            assert_eq!(loaded.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().attempt,0);
        }
        drop(db);std::fs::remove_dir_all(dir).unwrap();
    }
}
