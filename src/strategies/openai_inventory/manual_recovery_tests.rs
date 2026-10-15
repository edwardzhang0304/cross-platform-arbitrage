use super::*;
use rust_decimal::Decimal;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

fn fixture(market: MarketPair, now: u64) -> (Snapshot, [AccountEvidence; 2]) {
    let mut c: InventoryConfig = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-strategy.json")).unwrap();
    c.market = market;
    c.lighter_address=Some("0x3333333333333333333333333333333333333333".into());
    let mut s = Snapshot::new(c).unwrap();
    s.status = Status::Stopped; s.paused = true; s.stop_requested = true;
    s.consecutive_rollbacks = 3; s.recovery_after_ms = Some(now - 1);
    s.direction = Direction::LighterShort;
    let units = if market == MarketPair::Anth {700} else {90};
    s.positions[0].units = -units; s.positions[1].units = units;
    s.lots.push(Lot{id:"synthetic-owned".into(),level:0,units,opened_ms:now-1000,
        entry_spread:Decimal::from(12),entry_net_spread:Some(Decimal::from(11))});
    s.anchor = Some(Decimal::from(7)); s.time_adds_used = 4;
    s.last_open_completed = Some((now - 1000, Decimal::from(12)));
    s.funding_synced_ms = now;
    s.armed[0] = false; s.entry_confirmations[1] = Some((now, 1));
    let a = [Venue::Lighter, Venue::Entropy].map(|venue| AccountEvidence {
        venue, account: if venue == Venue::Lighter {s.config.lighter_account.clone()} else {s.config.entropy_account.clone()},
        observed_ms:now,position_units:s.positions[venue.index()].units,
        free_margin:100.into(),equity:100.into(),leverage:3,isolated:true,
        open_orders:0,authenticated:true,liquidation_price:None,
    });
    (s, a)
}

#[test]
fn only_manual_latch_changes_and_start_remains_separate_for_both_markets() {
    for market in [MarketPair::Openai, MarketPair::Anth] {
        let now = 1_000_000; let (s, a) = fixture(market, now);
        let books = [1700,1688].map(|p| Book{bids:vec![Level{price:p.into(),units:10000}],
            asks:vec![Level{price:p.into(),units:10000}],received_ms:now,connected:true});
        assert!(service::start_live_strategy(&mut s.clone(),Some(&a),&books,now).is_err());
        let mut next = checked_state(&s, &a, now).unwrap();
        assert!(next.stop_requested && next.paused && !next.resume_after_recovery);
        assert_eq!(next.status, Status::Stopped); assert_eq!(next.consecutive_rollbacks, 0);
        let mut expected = s.clone(); expected.consecutive_rollbacks=0;expected.recovery_after_ms=None;
        expected.resume_after_recovery=false;expected.reason=next.reason.clone();
        strategy::clear_decision_confirmation(&mut expected);
        assert_eq!(serde_json::to_value(&next).unwrap(),serde_json::to_value(&expected).unwrap());
        // A separate start still performs its own freshness and funding checks.
        assert!(service::start_live_strategy(&mut next,Some(&a),&books,now+4000).is_err());
        service::start_live_strategy(&mut next,Some(&a),&books,now).unwrap();
        assert_eq!(next.status,Status::Running);
    }
}

#[test]
fn every_other_risk_condition_and_unresolved_order_remains_blocked() {
    let now = 1_000_000;
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for bad in ["running","not_paused","not_stopped","no_lock","paper","pending","loss","orphan","close","stop_after_close","emergency","lot","ledger","same_side","nonpositive_lot"] {
            let (mut s, mut a)=fixture(market,now);
            match bad {
                "running"=>s.status=Status::Running,"not_paused"=>s.paused=false,
                "not_stopped"=>s.stop_requested=false,"no_lock"=>s.consecutive_rollbacks=2,
                "paper"=>s.config.mode=Mode::Paper,
                "pending"=>s.pending=Some(serde_json::from_value(serde_json::json!({
                    "id":"unknown","action":"open","level":0,"requested_units":90,"created_ms":1,
                    "first":null,"hedge":null,"repair":null,"first_terminal":false,"hedge_terminal":false,
                    "repair_terminal":false,"first_filled":0,"hedge_filled":0,"repair_filled":0,
                    "first_value":"0","hedge_value":"0","failed":false
                })).unwrap()),
                "loss"=>s.loss_stop=Some(LossStop{at_ms:now,net_pnl:Decimal::from(-30)}),
                "orphan"=>s.live_orphan=Some(live_orphan::Incident{missing:Venue::Lighter,detected_ms:now,last_check_ms:now,attempts:vec![],completed_ms:None,warning:String::new()}),
                "close"=>s.close_requested=true,"stop_after_close"=>s.stop_after_close=true,
                "emergency"=>{emergency_exit::latch(&mut s,now).unwrap();s.status=Status::Stopped;},
                "lot"=>s.lots[0].units+=1,"ledger"=>s.positions[0].units+=1,
                "same_side"=>{s.positions[0].units=s.positions[1].units;a[0].position_units=s.positions[0].units;},
                _=>s.lots[0].units=0,
            }
            let before=serde_json::to_value(&s).unwrap();
            assert!(checked_state(&s,&a,now).is_err(),"{market:?}/{bad}");
            assert_eq!(serde_json::to_value(&s).unwrap(),before);
        }
    }
}

#[test]
fn both_venues_must_have_fresh_authenticated_matching_positions_and_zero_orders() {
    let now=1_000_000;
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for venue in 0..2 {
            for bad in ["identity","auth","venue","stale","future","zero_time","orders","position","cross","leverage"] {
                let (s, mut a)=fixture(market,now);
                match bad {
                    "identity"=>a[venue].account="other-account".into(),"auth"=>a[venue].authenticated=false,
                    "venue"=>a[venue].venue=a[1-venue].venue,"stale"=>a[venue].observed_ms=now-3001,
                    "future"=>a[venue].observed_ms=now+1,"zero_time"=>a[venue].observed_ms=0,
                    "orders"=>a[venue].open_orders=1,"position"=>a[venue].position_units+=1,
                    "cross"=>a[venue].isolated=false,_=>a[venue].leverage=4,
                }
                assert!(checked_state(&s,&a,now).is_err(),"{market:?}/{venue}/{bad}");
            }
        }
    }
}

#[test]
fn flat_and_opposite_direction_inventory_can_be_acknowledged_without_rewriting_lots() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for flat in [true,false] {
            let (mut s,mut a)=fixture(market,1_000_000);
            if flat {s.lots.clear();s.positions[0].units=0;s.positions[1].units=0;}
            else {s.direction=Direction::LighterLong;s.positions[0].units*=-1;s.positions[1].units*=-1;}
            for i in 0..2 {a[i].position_units=s.positions[i].units;}
            let next=checked_state(&s,&a,1_000_000).unwrap();
            assert_eq!(next.direction,s.direction);
            assert_eq!(serde_json::to_value(&next.positions).unwrap(),serde_json::to_value(&s.positions).unwrap());
            assert_eq!(serde_json::to_value(&next.lots).unwrap(),serde_json::to_value(&s.lots).unwrap());
        }
    }
}

struct ReadOnlyEvidence { account: AccountEvidence, calls: Arc<AtomicUsize>, failure: &'static str }
impl venue::VenueBackend for ReadOnlyEvidence {
    fn submit(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult>{panic!("manual recovery must not trade")}
    fn lookup(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult>{panic!("must reject unresolved orders before recovery")}
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence>{panic!("cached account path must not be used")}
    fn reconcile_account(&mut self)->venue::BoxFuture<'_,AccountEvidence>{
        self.calls.fetch_add(1,Ordering::SeqCst);
        let mut a=self.account.clone();let failure=self.failure;
        Box::pin(async move {
            if failure=="network" {anyhow::bail!("synthetic connection failure");}
            if failure=="timeout" {std::future::pending::<()>().await;}
            a.observed_ms=crate::domain::now_ms();
            if failure=="orders" {a.open_orders=1;}
            if failure=="position" {a.position_units+=1;}
            Ok(a)
        })
    }
}
fn workers(a:[AccountEvidence;2], failure:&'static str)->([AccountWorker;2],[Arc<AtomicUsize>;2]) {
    let calls=[Arc::new(AtomicUsize::new(0)),Arc::new(AtomicUsize::new(0))];
    let workers=a.map(|account|{let i=account.venue.index();AccountWorker::spawn(account.venue,Mode::Paper,true,
        Box::new(ReadOnlyEvidence{account,calls:calls[i].clone(),failure:if i==1{failure}else{""}})).unwrap()});
    (workers,calls)
}
struct Temp(std::path::PathBuf);
impl Temp {
    fn new()->Self {Self(std::env::temp_dir().join(format!("manual-recovery-{}",uuid::Uuid::new_v4())))}
    fn db(&self)->std::path::PathBuf{self.0.join("state.sqlite")}
}
impl Drop for Temp {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}

#[tokio::test]
async fn running_or_unlocked_strategy_is_rejected_before_network_reads() {
    for unlocked in [true,false] {
        let now=crate::domain::now_ms();let (mut s,a)=fixture(MarketPair::Openai,now);
        if unlocked {s.consecutive_rollbacks=0;}else{s.status=Status::Running;s.stop_requested=false;}
        let temp=Temp::new();let (mut db,_)=Store::open(&temp.db(),&s.config).unwrap();
        db.commit(&s,now,"seed").unwrap();let (workers,calls)=workers(a,"");
        assert!(confirm(&mut s,&mut db,&workers,"not-eligible").await.is_err());
        assert_eq!(calls.each_ref().map(|n|n.load(Ordering::SeqCst)),[0,0]);
    }
}

#[tokio::test]
async fn fresh_queries_durable_ack_and_duplicate_cannot_release_a_future_incident() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        let now=crate::domain::now_ms();let (mut s,a)=fixture(market,now);let temp=Temp::new();
        let (mut db,_)=Store::open(&temp.db(),&s.config).unwrap();db.commit(&s,now,"seed").unwrap();
        let (workers,calls)=workers(a,"");
        confirm(&mut s,&mut db,&workers,"operator-ack").await.unwrap();
        assert_eq!(s.consecutive_rollbacks,0);assert_eq!(calls.each_ref().map(|n|n.load(Ordering::SeqCst)),[1,1]);
        let sql=rusqlite::Connection::open(temp.db()).unwrap();
        let event:String=sql.query_row("SELECT body FROM events WHERE kind='command' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).unwrap();
        let event:serde_json::Value=serde_json::from_str(&event).unwrap();
        assert_eq!(event["evidence"]["previous_rollbacks"],3);
        assert_eq!(event["evidence"]["accounts"].as_array().unwrap().len(),2);
        assert!(event["evidence"]["accounts"].as_array().unwrap().iter().all(|a|a["open_orders"]==0 && a["authenticated"]==true));
        drop(sql);drop(db);
        let (mut db,restored)=Store::open(&temp.db(),&s.config).unwrap();
        assert_eq!(restored.consecutive_rollbacks,0);assert!(restored.stop_requested && restored.paused);
        assert_eq!(restored.paired_units(),s.paired_units());
        s.consecutive_rollbacks=3;db.commit(&s,now+1,"later-incident").unwrap();
        assert!(confirm(&mut s,&mut db,&workers,"operator-ack").await.unwrap().is_none());
        assert_eq!(s.consecutive_rollbacks,3);assert_eq!(calls.each_ref().map(|n|n.load(Ordering::SeqCst)),[1,1]);
        confirm(&mut s,&mut db,&workers,"new-explicit-ack").await.unwrap();assert_eq!(s.consecutive_rollbacks,0);
    }
}

#[tokio::test(start_paused=true)]
async fn failed_checks_timeouts_and_failed_commit_leave_latch_unchanged() {
    for failure in ["network","timeout","orders","position","disk"] {
        let now=crate::domain::now_ms();let (mut s,a)=fixture(MarketPair::Openai,now);let temp=Temp::new();
        let (mut db,_)=Store::open(&temp.db(),&s.config).unwrap();db.commit(&s,now,"seed").unwrap();
        let (workers,_)=workers(a,failure);let sql=rusqlite::Connection::open(temp.db()).unwrap();
        if failure=="disk" {sql.execute_batch("CREATE TRIGGER fail_ack BEFORE INSERT ON commands BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;").unwrap();}
        let before=serde_json::to_value(&s).unwrap();
        assert!(confirm(&mut s,&mut db,&workers,"rejected-ack").await.is_err(),"{failure}");
        assert_eq!(serde_json::to_value(&s).unwrap(),before);
        let durable:String=sql.query_row("SELECT body FROM state",[],|r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&durable).unwrap(),before);
        assert!(!db.command_seen("rejected-ack","\"confirm_rollback_recovery\"").unwrap());
    }
}
