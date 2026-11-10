//! Synthetic legacy journals, never a copied account database or real order ID.
use super::*;
use std::sync::{Arc,Mutex};

fn legacy(now:u64, market:MarketPair, direction:Direction, filled:i64)
    -> (Snapshot,[AccountEvidence;2],[Book;2]) {
    let (mut s,mut a,mut b)=incident(now,direction);
    s.config.market=market;s.pending=None;s.positions=Default::default();
    s.fills.clear();s.fill_opening.clear();s.lots.clear();
    s.opened_groups=10;s.closed_groups=0;
    let scale=market.common_step()/10;
    for i in 0..10 {
        let units=if i<8 {80}else{110}*scale;
        let id=format!("synthetic-lot-{i}");
        for venue in [Venue::Lighter,Venue::Entropy] {
            s.record_fill(&Fill{id:format!("{id}-{venue:?}"),order_id:format!("{id}-open"),
                venue,side:direction.open_side(venue),units,price:d(1300),fee:Decimal::ZERO,
                time_ms:now-20_000},None).unwrap();
        }
        s.lots.push(Lot{id,units,level:i,opened_ms:now-20_000,
            entry_spread:d(20),entry_net_spread:Some(d(20))});
    }
    let mut p:Operation=serde_json::from_value(serde_json::json!({
        "id":"synthetic-close-minimum","action":"close","level":8,
        "requested_units":220*scale,"created_ms":now-10_000,
        "first":null,"hedge":null,"repair":null,"first_venue":"lighter",
        "first_terminal":true,"hedge_terminal":false,"repair_terminal":false,
        "first_filled":filled*scale,"hedge_filled":0,"repair_filled":0,
        "first_value":(d(1300)*d(filled*scale)).to_string(),"hedge_value":"0","failed":false,
        "repair_attempt":228,"recovery_slippage_bps":5,
        "close_allocations":[{"lot_id":"synthetic-lot-8","units":110*scale},
                             {"lot_id":"synthetic-lot-9","units":110*scale}]
    })).unwrap();
    let first=OrderRequest{id:format!("{}-v2-first",p.id),venue:Venue::Lighter,
        side:direction.open_side(Venue::Lighter).opposite(),units:220*scale,limit:d(1300),
        arrival_mid:None,reduce_only:true,created_ms:now-9000,expires_ms:now-4000,signed_expires_ms:None};
    s.record_fill(&result(&first,filled*scale,true).fills[0],None).unwrap();
    p.first=Some(first.clone());
    p.align_close=Some(OrderRequest{id:format!("{}-v2-align-close-228",p.id),
        units:scale,created_ms:now-3000,expires_ms:now+2000,..first});
    p.align_close_terminal=true;p.repair_retry_after_ms=Some(now);
    s.pending=Some(p);s.status=Status::NeedsAttention;s.reason=ALIGNMENT_HALT.into();
    s.paused=true;s.stop_requested=true;s.resume_after_recovery=false;
    for x in &mut a {x.position_units=s.positions[x.venue.index()].units;}
    for book in &mut b {
        book.bids=vec![Level{price:d(1300),units:10000*scale}];
        book.asks=book.bids.clone();
    }
    (s,a,b)
}

struct LegalCloseBackend(Arc<Mutex<Vec<OrderRequest>>>,MarketPair);
impl venue::VenueBackend for LegalCloseBackend {
    fn submit(&mut self,r:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        self.0.lock().unwrap().push(r.clone());
        let legal=r.reduce_only && r.units>=self.1.minimum_units(r.venue)
            && self.1.quantity(r.units)*r.limit>=d(10);
        Box::pin(async move {
            assert!(legal,"test venue rejects subminimum partial orders");
            Ok(result(&r,r.units,true))
        })
    }
    fn lookup(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        Box::pin(async {anyhow::bail!("a terminal order must not be reissued")})
    }
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence> {
        Box::pin(async {anyhow::bail!("synthetic reconciled evidence is supplied separately")})
    }
}

#[tokio::test]
async fn legacy_minimum_loop_finishes_only_two_reserved_groups_after_sqlite_reload() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for direction in [Direction::LighterShort,Direction::LighterLong] {
            for first_fill in [9,10] {
                let now=crate::domain::now_ms();let scale=market.common_step()/10;
                let (s,a,b)=legacy(now,market,direction,first_fill);
                let untouched=serde_json::to_value(&s.lots[..8]).unwrap();
                let path=std::env::temp_dir().join(format!("minimum-close-{}.sqlite",uuid::Uuid::new_v4()));
                let (mut db,_)=Store::open(&path,&s.config).unwrap();
                db.commit(&s,now,"synthetic_legacy_minimum").unwrap();drop(db);
                let (mut db,mut s)=Store::open(&path,&s.config).unwrap();
                let sent=Arc::new(Mutex::new(vec![]));
                let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(
                    v,Mode::Paper,true,Box::new(LegalCloseBackend(sent.clone(),market))).unwrap());
                // Loading marks a pending journal Recovering; the service first
                // observes its known terminal alignment before the REST-verified retry.
                advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
                assert!(sent.lock().unwrap().is_empty());
                assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
                advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
                let request=s.pending.as_ref().unwrap().align_close.clone().unwrap();
                let filled=s.pending.as_ref().unwrap().first_filled;
                apply(&mut s,4,&request,result(&request,request.units,true)).unwrap();
                assert_eq!(s.pending.as_ref().unwrap().first_filled,filled,"duplicate fill must be ignored");
                db.commit(&s,now,"synthetic_duplicate").unwrap();drop(db);
                let (mut db,mut s)=Store::open(&path,&s.config).unwrap();
                for _ in 0..3 {if s.pending.is_none(){break;}advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
                let sent=sent.lock().unwrap();assert_eq!(sent.len(),2);
                assert_eq!((sent[0].venue,sent[0].units),(Venue::Lighter,(220-first_fill)*scale));
                assert_eq!((sent[1].venue,sent[1].units),(Venue::Entropy,220*scale));
                assert!(sent[0].id.ends_with("align-close-229"));
                assert!(s.pending.is_none());assert_eq!(s.paired_units(),640*scale);
                assert_eq!(s.positions[0].units.abs(),640*scale);
                assert_eq!(s.positions[0].units,-s.positions[1].units);
                assert_eq!(serde_json::to_value(&s.lots).unwrap(),untouched);
                assert_eq!(s.closed_groups,2);assert_eq!(s.status,Status::Stopped);
                assert!(s.paused && s.stop_requested);
                assert!(s.emergency_exit.is_none() && !s.close_requested);
                drop(db);std::fs::remove_file(path).unwrap();
            }
        }
    }
}

#[test]
fn original_reservation_too_small_holds_without_claiming_flat_or_spending_more_queries() {
    let now=100_000;let (mut s,_,b)=legacy(now,MarketPair::Openai,Direction::LighterShort,219);
    let owned=serde_json::to_value((&s.positions,&s.lots,&s.fills)).unwrap();
    assert!(defer_unfillable_recovery(&mut s,&b,now).unwrap());
    assert_eq!(s.reason,MINIMUM_CLOSE_HALT);assert!(s.pending.is_some());
    for at in [now+3000,now+60000,now+3600000] {assert!(!automatic_repair_due(&s,at));}
    assert_eq!(serde_json::to_value((&s.positions,&s.lots,&s.fills)).unwrap(),owned);
    assert_eq!(s.pending.as_ref().unwrap().repair_attempt,228);
}

#[test]
fn legacy_alignment_requires_matching_fresh_accounts_without_orders() {
    let now=100_000;let (s,a,b)=legacy(now,MarketPair::Openai,Direction::LighterShort,9);
    for case in 0..4 {
        let mut state=s.clone();let mut accounts=a.clone();
        match case {
            0=>accounts[0].open_orders=1,
            1=>accounts[1].authenticated=false,
            2=>accounts[0].observed_ms=now-s.config.account_max_age_ms-1,
            _=>accounts[1].position_units+=10,
        }
        let before=serde_json::to_value(&state).unwrap();
        assert!(!resume_protected_repair(&mut state,&accounts,&b,now).unwrap_or(false));
        assert_eq!(serde_json::to_value(&state).unwrap(),before);
    }
}

#[test]
fn common_partial_fill_checks_closing_side_price_not_book_midpoint() {
    let now=100_000;let (s,_,mut b)=legacy(now,MarketPair::Openai,Direction::LighterShort,80);
    b[1].bids[0].price=d(1249);b[1].asks[0].price=d(1251);
    assert_eq!(s.config.quantity(80)*b[1].mid().unwrap(),d(10));
    assert!(close_alignment_needed(&s,s.pending.as_ref().unwrap(),&b));
    b[1].bids[0].price=d(1300);b[1].asks[0].price=d(1301);
    assert!(!close_alignment_needed(&s,s.pending.as_ref().unwrap(),&b));
}

#[test]
fn minimum_rejected_alignment_only_retries_when_original_budget_allows_a_larger_order() {
    let now=100_000;let (mut s,_,_)=legacy(now,MarketPair::Openai,Direction::LighterShort,9);
    let p=s.pending.as_mut().unwrap();
    p.recovery_wait_reason="requested base size is below Lighter minimum".into();
    assert!(automatic_repair_due(&s,now));
    s.pending.as_mut().unwrap().align_close.as_mut().unwrap().units=211;
    assert!(!automatic_repair_due(&s,now));
}

#[tokio::test]
async fn unknown_legacy_alignment_is_looked_up_without_resizing_or_resubmitting() {
    let now=crate::domain::now_ms();let (mut s,_,b)=legacy(now,MarketPair::Openai,Direction::LighterShort,9);
    s.pending.as_mut().unwrap().align_close_terminal=false;s.status=Status::RecoveringExposure;
    let old=s.pending.as_ref().unwrap().align_close.clone().unwrap();
    let calls=Arc::new(Mutex::new(vec![]));
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(
        v,Mode::Paper,true,Box::new(UnknownRepairBackend(calls.clone()))).unwrap());
    let (mut db,_)=Store::offline_replay(&s.config).unwrap();
    for _ in 0..3 {advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
    let calls=calls.lock().unwrap();assert_eq!(calls.len(),3);
    assert!(calls.iter().all(|(submit,r)|!submit && r.id==old.id && r.units==old.units && r.limit==old.limit));
    assert_eq!(s.pending.as_ref().unwrap().first_filled,9);
}

#[test]
fn terminal_minimum_rejection_does_not_loop_but_price_failure_still_retries() {
    for reason in ["Order must have minimum value of $10. asset=200001",
        "requested base size 0.000100000000 is below Lighter minimum 0.005000000000"] {
        let now=100_000;let (mut s,_,_)=close_incident(now,Direction::LighterShort);
        let r=OrderRequest{id:"synthetic-minimum-repair".into(),venue:Venue::Entropy,
            side:Side::Sell,units:270,limit:d(1300),arrival_mid:None,reduce_only:true,
            created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
        s.pending.as_mut().unwrap().repair=Some(r.clone());
        let mut rejected=result(&r,0,true);rejected.reason=reason.into();
        apply(&mut s,2,&r,rejected).unwrap();s.status=Status::NeedsAttention;s.reason=RESIDUAL_HALT.into();
        assert!(!automatic_repair_due(&s,now+3000));
        s.pending.as_mut().unwrap().recovery_wait_reason="depth exceeds protected execution price".into();
        assert!(automatic_repair_due(&s,now+3000));
    }
}
