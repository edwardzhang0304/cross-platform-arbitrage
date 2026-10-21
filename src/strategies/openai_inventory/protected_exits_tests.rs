use super::*;
use rust_decimal::Decimal;
use std::sync::{Arc, Mutex};

fn fixture(market: MarketPair, scale: u32, now: u64) -> (Snapshot, [AccountEvidence; 2]) {
    let mut c: InventoryConfig = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-strategy.json")).unwrap();
    c.market = market;
    c.lighter_address=Some("0x3333333333333333333333333333333333333333".into());
    c.capital_multiplier = scale;
    for v in [&mut c.group_notional, &mut c.close_slice_notional, &mut c.max_notional_per_venue,
        &mut c.max_loss_usdc, &mut c.min_free_margin, &mut c.paper_capital_per_venue] { *v *= Decimal::from(scale); }
    let mut s = Snapshot::new(c).unwrap();
    s.status = Status::Stopped; s.paused = true; s.stop_requested = true;
    s.direction = Direction::LighterShort;
    s.consecutive_rollbacks = 2; s.time_adds_used = 4; s.anchor = Some(7.into());
    s.last_open_completed = Some((now - 10000, 12.into()));
    s.armed[0] = false; s.funding_synced_ms = now;
    let units = market.common_step() * 7 * i64::from(scale);
    s.positions[0] = Position {units:-units*2,average:2000.into(),..Default::default()};
    s.positions[1] = Position {units:units*2,average:1984.into(),..Default::default()};
    for (n, spread) in [20,12].into_iter().enumerate() {
        s.lots.push(Lot {id:format!("synthetic-{n}"),level:n,units,opened_ms:now-10000,
            entry_spread:spread.into(),entry_net_spread:Some(spread.into())});
    }
    s.entry_margin_block = Some(entry_margin::EntryMarginBlock {venue:Venue::Lighter,at_ms:now-5000,
        order_id:"synthetic-block".into(),rejection:entry_margin::EntryRejection::PreflightFunding});
    let a = evidence(&s, now);
    (s,a)
}
fn evidence(s: &Snapshot, now: u64) -> [AccountEvidence; 2] {
    [Venue::Lighter,Venue::Entropy].map(|v| AccountEvidence {
        venue:v,account:if v==Venue::Lighter{s.config.lighter_account.clone()}else{s.config.entropy_account.clone()},
        observed_ms:now,position_units:s.positions[v.index()].units,open_orders:0,authenticated:true,
        free_margin:Decimal::ZERO,equity:1000.into(),isolated:true,leverage:3,liquidation_price:None,
        entry_margin:Some(entry_margin::EntryMarginEvidence {reported_available:Some(28.into()),collateral:Some(Decimal::ZERO),
            allocated_margin:Some(20.into()),unrealized_pnl:Some(5.into()),position_value:Some(28.into()),
            initial_margin_fraction:Some(Decimal::new(3333,4)),mark_price:Some(2000.into()),isolated_only:true}),
    })
}
fn books(now: u64, spread: i64) -> [Book; 2] {
    [2000,2000-spread].map(|p| Book {bids:vec![Level {price:p.into(),units:1000000}],
        asks:vec![Level {price:p.into(),units:1000000}],received_ms:now,connected:true})
}
struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self { Self(std::env::temp_dir().join(format!("protected-exits-{}",uuid::Uuid::new_v4()))) }
    fn db(&self)->std::path::PathBuf {self.0.join("state.sqlite")}
}
impl Drop for Temp {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
#[derive(Default)]
struct Calls {reads:usize,submitted:Vec<OrderRequest>}
struct Synthetic {account:AccountEvidence,calls:Arc<Mutex<Calls>>,failure:&'static str}
impl venue::VenueBackend for Synthetic {
    fn reconcile_account(&mut self)->venue::BoxFuture<'_,AccountEvidence> {
        self.calls.lock().unwrap().reads+=1;
        let mut a=self.account.clone();let failure=self.failure;
        Box::pin(async move {
            if failure=="network" {anyhow::bail!("synthetic read failure");}
            if failure=="timeout" {std::future::pending::<()>().await;}
            a.observed_ms=crate::domain::now_ms();
            if failure=="orders" {a.open_orders=1;}
            if failure=="position" {a.position_units+=1;}
            Ok(a)
        })
    }
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence>{panic!("must use explicit reconciliation")}
    fn lookup(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult>{panic!("synthetic submissions are terminal")}
    fn submit(&mut self,r:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        assert!(r.reduce_only,"protected exits must never increase exposure");
        assert_ne!(self.account.position_units.signum(),r.side.sign());
        assert!(self.account.position_units.abs()>=r.units);
        self.account.position_units+=r.units*r.side.sign();
        self.calls.lock().unwrap().submitted.push(r.clone());
        Box::pin(async move {Ok(OrderResult {terminal:true,fills:vec![Fill {id:format!("fill-{}",r.id),
            order_id:r.id,venue:r.venue,side:r.side,units:r.units,price:r.limit,fee:Decimal::ZERO,
            time_ms:crate::domain::now_ms()}],exchange_created_ms:None,reason:"synthetic terminal fill".into(),rejection:None})})
    }
}
fn workers(a:[AccountEvidence;2], failure:&'static str)->([AccountWorker;2],Arc<Mutex<Calls>>) {
    let calls=Arc::new(Mutex::new(Calls::default()));
    let w=a.map(|account|AccountWorker::spawn(account.venue,Mode::Paper,true,
        Box::new(Synthetic {account,calls:calls.clone(),failure})).unwrap());
    (w,calls)
}

#[test]
fn transition_preserves_protection_inventory_rules_and_history_without_entry_cash() {
    for market in [MarketPair::Openai,MarketPair::Anth] {for scale in [1,10] {
        let now=1_000_000;let (s,a)=fixture(market,scale,now);
        let mut next=checked_protected_exits(&s,&a,now).unwrap();
        let mut expected=s.clone();expected.stop_requested=false;expected.resume_after_recovery=true;
        expected.recovery_after_ms=None;expected.status=Status::PausedEntries;expected.reason=next.reason.clone();
        strategy::clear_decision_confirmation(&mut expected);
        assert_eq!(serde_json::to_value(&next).unwrap(),serde_json::to_value(expected).unwrap());
        assert!(service::start_live_strategy(&mut next,Some(&a),&books(now,10),now).is_err());
        assert!(service::start_one_entry(&mut next,Some(&a),&books(now,10),now).is_err());
        assert!(strategy::risk_check(&next,&books(now,10),&a,now,s.lots[0].units,Action::Open).is_err());
        strategy::risk_check(&next,&books(now,10),&a,now,s.lots[0].units,Action::Close).unwrap();
        let mut without_details=a;for a in &mut without_details {a.entry_margin=None;}
        assert!(checked_protected_exits(&s,&without_details,now).is_ok());
    }}
}

#[test]
fn no_existing_fault_or_unfinished_order_can_be_overridden() {
    let now=1_000_000;
    for bad in ["no_lock","not_paused","not_stopped","attention","recovering","pending","loss","orphan","close","stop_after","emergency","lot","ledger"] {
        let (mut s,a)=fixture(MarketPair::Openai,1,now);
        match bad {
            "no_lock"=>s.entry_margin_block=None,"not_paused"=>s.paused=false,"not_stopped"=>s.stop_requested=false,
            "attention"=>s.status=Status::NeedsAttention,"recovering"=>s.status=Status::Recovering,
            "pending"=>{s=checked_protected_exits(&s,&a,now).unwrap();strategy::evaluate(&mut s,&books(now,9),&a,now).unwrap();
                let a=evidence(&s,now+1000);s.pending=strategy::evaluate(&mut s,&books(now+1000,9),&a,now+1000).unwrap();assert!(s.pending.is_some());},
            "loss"=>s.loss_stop=Some(LossStop {at_ms:now,net_pnl:(-30).into()}),
            "orphan"=>s.live_orphan=Some(live_orphan::Incident {missing:Venue::Lighter,detected_ms:now,last_check_ms:now,attempts:vec![],completed_ms:None,warning:String::new()}),
            "close"=>s.close_requested=true,"stop_after"=>s.stop_after_close=true,
            "emergency"=>{emergency_exit::latch(&mut s,now).unwrap();},
            "lot"=>s.lots[0].units=0,"ledger"=>s.positions[0].units+=10,_=>unreachable!(),
        }
        assert!(checked_protected_exits(&s,&a,now).is_err(),"{bad}");
    }
    for venue in 0..2 {for bad in ["stale","future","auth","account","orders","position","leverage","cross"] {
        let (s,mut a)=fixture(MarketPair::Anth,10,now);
        match bad {
            "stale"=>a[venue].observed_ms=now-4000,"future"=>a[venue].observed_ms=now+1,
            "auth"=>a[venue].authenticated=false,"account"=>a[venue].account="another-account".into(),
            "orders"=>a[venue].open_orders=1,"position"=>a[venue].position_units+=100,
            "leverage"=>a[venue].leverage=5,"cross"=>a[venue].isolated=false,_=>unreachable!(),
        }
        assert!(checked_protected_exits(&s,&a,now).is_err(),"{venue}/{bad}");
    }}
}

#[tokio::test(start_paused=true)]
async fn failed_reads_and_disk_failure_leave_stopped_state_and_latch_unchanged() {
    for failure in ["network","timeout","orders","position","disk"] {
        let now=crate::domain::now_ms();let (mut s,a)=fixture(MarketPair::Openai,1,now);let temp=Temp::new();
        let (mut db,_)=Store::open(&temp.db(),&s.config).unwrap();db.commit(&s,now,"seed").unwrap();
        let sql=rusqlite::Connection::open(temp.db()).unwrap();
        if failure=="disk" {sql.execute_batch("CREATE TRIGGER fail_ack BEFORE INSERT ON commands BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;").unwrap();}
        let before=serde_json::to_value(&s).unwrap();let (w,calls)=workers(a,failure);
        assert!(resume_protected_exits(&mut s,&mut db,&w,"bad").await.is_err(),"{failure}");
        assert_eq!(serde_json::to_value(&s).unwrap(),before);
        let body:String=sql.query_row("SELECT body FROM state",[],|r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(),before);
        assert!(calls.lock().unwrap().submitted.is_empty());
    }
}

#[tokio::test]
async fn multiple_profit_exits_flat_no_reentry_restart_and_later_stop() {
    for market in [MarketPair::Openai,MarketPair::Anth] {for scale in [1,10] {
        let now=crate::domain::now_ms();let (mut s,a)=fixture(market,scale,now);let temp=Temp::new();
        let lock=serde_json::to_value(&s.entry_margin_block).unwrap();
        let (mut db,_)=Store::open(&temp.db(),&s.config).unwrap();db.commit(&s,now,"seed").unwrap();drop(db);
        // The upgrade copies a STOPPED ledger. Loading cannot resume normal exits.
        let (mut db,mut restored)=Store::open(&temp.db(),&s.config).unwrap();
        service::quarantine_live_start(&mut restored);
        service::complete_reconciled_recovery(&mut restored,&a).unwrap();
        assert_eq!(restored.status,Status::Stopped);
        assert!(strategy::evaluate(&mut restored,&books(now,9),&a,now).unwrap().is_none());s=restored;
        let (w,calls)=workers(evidence(&s,now),"");
        resume_protected_exits(&mut s,&mut db,&w,"resume").await.unwrap();
        assert_eq!(calls.lock().unwrap().reads,2);assert!(calls.lock().unwrap().submitted.is_empty());
        for (round,spread) in [9,5].into_iter().enumerate() {
            // A restart with active exit-only mode retains that mode for held lots.
            drop(db);let opened=Store::open(&temp.db(),&s.config).unwrap();db=opened.0;s=opened.1;
            service::quarantine_live_start(&mut s);
            let a=evidence(&s,now);service::complete_reconciled_recovery(&mut s,&a).unwrap();
            assert_eq!(s.status,Status::PausedEntries);assert!(!s.stop_requested && s.paused);
            let at=crate::domain::now_ms();let first=at-1000;s.funding_synced_ms=first;
            let a=evidence(&s,first);assert!(strategy::evaluate(&mut s,&books(first,spread),&a,first).unwrap().is_none());
            let a=evidence(&s,at);let op=strategy::evaluate(&mut s,&books(at,spread),&a,at).unwrap().unwrap();
            assert_eq!(op.action,Action::Close);assert_eq!(op.requested_units,s.lots[0].units);
            s.pending=Some(op);
            for _ in 0..8 {let at=crate::domain::now_ms();execution::advance(&mut s,&mut db,&w,&books(at,spread),at).await.unwrap();if s.pending.is_none(){break;}}
            assert!(s.pending.is_none());assert_eq!(s.lots.len(),1-round);
            assert_eq!(s.status,Status::PausedEntries);assert!(!s.stop_requested && s.paused);
            assert_eq!(serde_json::to_value(&s.entry_margin_block).unwrap(),lock);
        }
        assert_eq!(s.positions[0].units,0);assert_eq!(s.positions[1].units,0);
        // A long run of wide entry spreads may warm the mean but must not add risk.
        for t in (1000..=360000).step_by(1000) {let at=now+t;s.funding_synced_ms=at;
            let a=evidence(&s,at);assert!(strategy::evaluate(&mut s,&books(at,50),&a,at).unwrap().is_none());}
        assert_eq!(calls.lock().unwrap().submitted.len(),4);
        assert!(calls.lock().unwrap().submitted.iter().all(|r|r.reduce_only));
        // A reused acknowledgement must not undo a later explicit stop.
        s.stop_requested=true;s.paused=true;s.resume_after_recovery=false;s.status=Status::Stopped;
        db.command(&s,"later-stop","\"stop\"",now).unwrap();
        assert!(resume_protected_exits(&mut s,&mut db,&w,"resume").await.unwrap().is_none());
        assert_eq!(s.status,Status::Stopped);assert_eq!(calls.lock().unwrap().reads,2);
    }}
}

#[test]
fn loss_stop_remains_active_and_pause_cannot_restart_or_erase_faults() {
    let now=1_000_000;let (s,a)=fixture(MarketPair::Openai,1,now);
    let mut s=checked_protected_exits(&s,&a,now).unwrap();
    s.positions[0].realized=-(s.config.max_loss_usdc+Decimal::from(1));
    let a=evidence(&s,now);
    let op=strategy::evaluate(&mut s,&books(now,16),&a,now).unwrap().unwrap();
    assert_eq!(op.action,Action::Close);assert!(s.loss_stop.is_some() && s.close_requested);
    assert!(s.entry_margin_block.is_some());
    for status in [Status::Stopped,Status::NeedsAttention,Status::Recovering,Status::Closing] {
        let (mut s,_)=fixture(MarketPair::Openai,1,now);s.status=status;
        let old=s.status;service::pause_entries(&mut s);assert_eq!(s.status,old);
    }
}

#[test]
fn original_profit_quote_and_funding_gates_still_apply_and_flat_restart_stays_stopped() {
    let now=1_000_000;let (s,a)=fixture(MarketPair::Openai,1,now);
    let base=checked_protected_exits(&s,&a,now).unwrap();
    for kind in ["no_profit","stale_book","missing_funding"] {
        let mut s=base.clone();let mut b=books(now,if kind=="no_profit"{50}else{5});
        if kind=="stale_book" {b[0].received_ms=now-4000;}
        if kind=="missing_funding" {s.funding_synced_ms=0;}
        let result=strategy::evaluate(&mut s,&b,&a,now);
        assert!(result.is_err() || result.unwrap().is_none(),"{kind}");
        assert!(s.pending.is_none() && s.paused && s.entry_margin_block.is_some());
    }
    let temp=Temp::new();let mut s=base;s.positions[0].units=0;s.positions[1].units=0;s.lots.clear();
    let (mut db,_)=Store::open(&temp.db(),&s.config).unwrap();db.commit(&s,now,"flat").unwrap();drop(db);
    let (_db,mut restored)=Store::open(&temp.db(),&s.config).unwrap();
    service::quarantine_live_start(&mut restored);
    let a=evidence(&restored,now);service::complete_reconciled_recovery(&mut restored,&a).unwrap();
    assert_eq!(restored.status,Status::Stopped);
    assert!(restored.stop_requested && restored.paused && restored.entry_margin_block.is_some());
}
