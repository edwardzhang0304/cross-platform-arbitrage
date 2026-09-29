use super::*;

fn d(v: i64) -> Decimal { Decimal::from(v) }

// Sep 25 incident shape: ten old pairs (850), extra Entropy 90, terminal
// zero-fill Lighter hedge and terminal zero-fill Entropy reducing repair.
fn incident(now: u64, direction: Direction) -> (Snapshot, [AccountEvidence; 2], [Book; 2]) {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.config.direction_policy = DirectionPolicy::Both;
    s.config.execution_slippage_bps = Decimal::ONE;
    s.direction = direction;
    s.status = Status::NeedsAttention;
    s.reason = RESIDUAL_HALT.into();
    s.paused = false;
    s.stop_requested = false;
    for (i, units) in [80,60,90,90,80,90,90,90,90,90].into_iter().enumerate() {
        let id = format!("old-{i}");
        for venue in [Venue::Lighter, Venue::Entropy] {
            s.record_fill(&Fill { id: format!("{id}-{venue:?}"), order_id: format!("{id}-v2-entry"),
                venue, side: direction.open_side(venue), units, price: d(1633), fee: Decimal::ZERO,
                time_ms: now-20_000 }, None).unwrap();
        }
        s.lots.push(Lot { id, units, level:i, opened_ms:now-20_000,
            entry_spread:d(12), entry_net_spread:Some(d(12)) });
    }
    s.opened_groups=10;
    let mut p: Operation = serde_json::from_value(serde_json::json!({
        "id":"incident-13", "action":"open", "level":5, "requested_units":90,
        "created_ms":now-10_000,"first":null,"hedge":null,"repair":null,
        "first_terminal":true,"hedge_terminal":true,"repair_terminal":true,
        "first_filled":90,"hedge_filled":0,"repair_filled":0,"first_value":"146979",
        "hedge_value":"0","failed":true,"first_venue":"entropy"
    })).unwrap();
    let old_request = |suffix: &str, venue: Venue, reduce: bool| OrderRequest {
        id:format!("incident-13-v2-{suffix}"),venue,
        side:if reduce {direction.open_side(venue).opposite()} else {direction.open_side(venue)},
        units:90,limit:d(1633),arrival_mid:None,reduce_only:reduce,created_ms:now-9000,
        expires_ms:now-4000,signed_expires_ms:None,
    };
    p.first=Some(old_request("first",Venue::Entropy,false));
    p.hedge=Some(old_request("hedge",Venue::Lighter,false));
    p.repair=Some(old_request("repair",Venue::Entropy,true));
    p.repair_retry_after_ms=Some(now);
    let f=Fill {id:"extra-entry".into(),order_id:p.first.as_ref().unwrap().id.clone(),
        venue:Venue::Entropy,side:direction.open_side(Venue::Entropy),units:90,
        price:Decimal::new(16331,1),fee:Decimal::ZERO,time_ms:now-8500};
    s.record_fill(&f,None).unwrap();
    s.pending=Some(p);
    let accounts=[Venue::Lighter,Venue::Entropy].map(|venue| AccountEvidence {
        venue,account:"fixture".into(),observed_ms:now,position_units:s.positions[venue.index()].units,
        free_margin:d(50),equity:d(100),leverage:3,isolated:true,open_orders:0,
        authenticated:true,liquidation_price:None,
    });
    let books=[1652,1628].map(|p| Book {bids:vec![Level {price:d(p),units:2000}],
        asks:vec![Level {price:d(p+1),units:2000}],received_ms:now,connected:true});
    (s,accounts,books)
}

fn result(req:&OrderRequest, units:i64, terminal:bool)->OrderResult {
    OrderResult { exchange_created_ms: None, terminal,reason:"fixture result".into(),fills: if units==0 {vec![]} else {vec![Fill {
        id:format!("{}-fill",req.id),order_id:req.id.clone(),venue:req.venue,side:req.side,
        units,price:req.limit,fee:Decimal::ZERO,time_ms:req.created_ms,
    }]}}
}

#[test]
fn rc10_repair_slices_only_protected_depth_in_both_markets_and_directions() {
    let now=100_000;
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for side in [Side::Buy,Side::Sell] {
            let (mut s,_,mut b)=incident(now,Direction::LighterShort);
            s.config.market=market;
            let step=s.config.common_step();
            let levels=vec![Level{price:d(2000),units:step*6+step/2},
                Level{price:if side==Side::Buy {d(2001)}else{d(1999)},units:step*100}];
            b[1].bids=vec![Level{price:d(1999),units:step*100}];
            b[1].asks=vec![Level{price:d(2001),units:step*100}];
            if side==Side::Buy {b[1].asks=levels;}else{b[1].bids=levels;}
            let r=request(&s,&b,Venue::Entropy,side,step*18,true,"repair",now).unwrap();
            assert_eq!(r.units,step*6);
            assert!(r.reduce_only);
            assert_eq!(r.side,side);
            assert!(if side==Side::Buy {r.limit<=Decimal::new(20002,1)}else{r.limit>=Decimal::new(19998,1)});
            // The ordinary hedge/entry still requires full protected depth.
            assert!(request(&s,&b,Venue::Entropy,side,step*18,true,"hedge",now).is_err());
        }
    }
}

#[test]
fn automatic_retry_preserves_ten_pairs_on_full_and_partial_fills_in_both_directions() {
    let now=100_000;
    for direction in [Direction::LighterShort,Direction::LighterLong] {
        for first_repair in [0,40] {
            let (mut s,mut a,b)=incident(now,direction);
            let lots=serde_json::to_value(&s.lots).unwrap();
            let old=s.pending.as_ref().unwrap().repair.clone().unwrap();
            apply(&mut s,2,&old,result(&old,first_repair,true)).unwrap();
            s.reason=RESIDUAL_HALT.into();
            a[1].position_units=s.positions[1].units;
            assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
            let qty=90-first_repair;
            let r=request(&s,&b,Venue::Entropy,direction.open_side(Venue::Entropy).opposite(),
                qty,true,"repair",now).unwrap();
            assert_eq!(r.id,"incident-13-v2-repair-1");
            assert!(r.reduce_only);
            s.pending.as_mut().unwrap().repair=Some(r.clone());
            apply(&mut s,2,&r,result(&r,qty,true)).unwrap();
            // Authenticated duplicate lookup must not count another fill.
            apply(&mut s,2,&r,result(&r,qty,true)).unwrap();
            assert_eq!(s.pending.as_ref().unwrap().repair_filled,90);
            s.finish_operation(now+1).unwrap();
            assert!(s.pending.is_none());
            assert_eq!(serde_json::to_value(&s.lots).unwrap(),lots);
            assert_eq!(s.opened_groups,10);
            assert_eq!(s.closed_groups,0);
            assert_eq!(s.positions[0].units,-s.positions[1].units);
            assert_eq!(s.positions[0].units.abs(),850);
        }
    }
}

#[test]
fn retry_requires_terminal_outcomes_fresh_accounts_owned_positions_and_protected_depth() {
    let now=100_000;
    for case in 0..12 {
        let (mut s,mut a,mut b)=incident(now,Direction::LighterShort);
        match case {
            0=>s.pending.as_mut().unwrap().first_terminal=false,
            1=>s.pending.as_mut().unwrap().hedge_terminal=false,
            2=>s.pending.as_mut().unwrap().repair_terminal=false,
            3=>a[1].authenticated=false,
            4=>a[1].open_orders=1,
            5=>a[1].observed_ms=now-4000,
            6=>a[1].observed_ms=now+1,
            7=>a[1].position_units+=10,
            8=>{a[1].position_units+=10;s.positions[1].units+=10;},
            9=>b[1].received_ms=now-2000,
            // Less than one venue step cannot be sliced safely.
            10=>b[1].bids=vec![Level{price:d(1628),units:9},Level{price:d(1627),units:1000}],
            _=>s.config.auto_neutralize=false,
        }
        let before=serde_json::to_value(&s).unwrap();
        assert!(!resume_protected_repair(&mut s,&a,&b,now).unwrap_or(false),"case {case}");
        assert_eq!(serde_json::to_value(&s).unwrap(),before,"case {case}");
    }
}

#[test]
fn retries_back_off_but_keep_following_residual_and_survive_restart() {
    let now=100_000;
    let (mut s,a,b)=incident(now,Direction::LighterShort);
    assert!(!automatic_repair_due(&s,now-1));
    // Count a terminal pre-dispatch failure even though no request was created.
    s.pending.as_mut().unwrap().repair=None;
    for attempt in 1..=8 {
        assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
        let p=s.pending.as_mut().unwrap();
        assert_eq!(p.repair_attempt,attempt);
        assert!(!p.repair_terminal && p.repair_retry_after_ms.is_none());
        p.repair_terminal=true;
        p.repair_retry_after_ms=Some(now);
        s.status=Status::NeedsAttention;s.reason=RESIDUAL_HALT.into();
        s=serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap();
    }
    assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
    assert_eq!([retry_delay(0),retry_delay(3),retry_delay(6),retry_delay(100)],
        [5000,15000,30000,30000]);
}

#[test]
fn pause_stop_and_loss_latch_survive_protective_repair() {
    let now=100_000;
    let (mut s,a,b)=incident(now,Direction::LighterShort);
    s.paused=true;s.stop_requested=true;
    s.loss_stop=Some(LossStop{at_ms:now,net_pnl:d(-30)});
    assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
    assert!(s.paused && s.stop_requested && s.loss_stop.is_some());
    assert_eq!(s.config.execution_slippage_bps,Decimal::ONE);
}

#[test]
fn smaller_second_request_still_rejects_a_venue_overfill() {
    let now=100_000;
    let (mut s,mut a,b)=incident(now,Direction::LighterShort);
    let old=s.pending.as_ref().unwrap().repair.clone().unwrap();
    apply(&mut s,2,&old,result(&old,40,true)).unwrap();
    a[1].position_units=s.positions[1].units;
    s.reason=RESIDUAL_HALT.into();
    assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
    let r=request(&s,&b,Venue::Entropy,Side::Sell,50,true,"repair",now).unwrap();
    assert!(apply(&mut s,2,&r,result(&r,60,true)).is_err());
}

struct RepairBackend(std::sync::Arc<std::sync::Mutex<Vec<OrderRequest>>>);
impl venue::VenueBackend for RepairBackend {
    fn submit(&mut self, r:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        Box::pin(async move {
            self.0.lock().unwrap().push(r.clone());
            Ok(result(&r,r.units,true))
        })
    }
    fn lookup(&mut self, _:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        Box::pin(async { anyhow::bail!("unexpected duplicate lookup") })
    }
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence> {
        Box::pin(async { anyhow::bail!("fixture does not provide live accounts") })
    }
}

struct SlicedRepairBackend {
    requests:std::sync::Arc<std::sync::Mutex<Vec<OrderRequest>>>,
    scale:i64,
}
impl venue::VenueBackend for SlicedRepairBackend {
    fn submit(&mut self,r:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        let mut sent=self.requests.lock().unwrap();
        let qty=if sent.is_empty(){40*self.scale}else{r.units};sent.push(r.clone());
        Box::pin(async move{Ok(result(&r,qty,true))})
    }
    fn lookup(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult> {
        Box::pin(async{anyhow::bail!("no unknown request in terminal fixture")})
    }
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence> {
        Box::pin(async{anyhow::bail!("synthetic accounts supplied by fixture")})
    }
}

#[tokio::test]
async fn rc10_close_990_to_810_slices_180_with_partial_fill_and_real_ledger_reopen() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for direction in [Direction::LighterShort,Direction::LighterLong] {
            let mut now=crate::domain::now_ms();
            let scale=market.common_step()/10;
            let (mut s,mut a,mut b)=close_incident(now,direction);
            s.config.market=market;s.positions=Default::default();s.fills.clear();s.lots.clear();
            for i in 0..11 {
                let id=format!("synthetic-selected-{i}");
                for venue in [Venue::Lighter,Venue::Entropy] {
                    s.record_fill(&Fill{id:format!("{id}-{venue:?}"),order_id:format!("{id}-entry"),
                        venue,side:direction.open_side(venue),units:90*scale,price:d(2000),fee:Decimal::ZERO,time_ms:now-20_000},None).unwrap();
                }
                s.lots.push(Lot{id,units:90*scale,level:i,opened_ms:now-20_000,entry_spread:d(20),entry_net_spread:Some(d(20))});
            }
            let untouched=serde_json::to_value(&s.lots[..9]).unwrap();
            let p=s.pending.as_mut().unwrap();
            p.requested_units=180*scale;p.first_filled=180*scale;p.first_value=d(2000)*d(180*scale);
            p.first.as_mut().unwrap().units=180*scale;
            p.first.as_mut().unwrap().limit=d(2000);
            p.close_allocations=s.lots[9..].iter().map(|l|CloseAllocation{lot_id:l.id.clone(),units:l.units}).collect();
            let first=p.first.clone().unwrap();
            s.record_fill(&Fill{id:"synthetic-first-close".into(),order_id:first.id.clone(),venue:first.venue,
                side:first.side,units:first.units,price:d(2000),fee:Decimal::ZERO,time_ms:now-8500},None).unwrap();
            s.paused=true;s.stop_requested=true;
            let side=direction.open_side(Venue::Entropy).opposite();
            b[0].bids=vec![Level{price:d(1999),units:5000*scale}];
            b[0].asks=vec![Level{price:d(2001),units:5000*scale}];
            b[1].bids=vec![Level{price:d(1999),units:5000*scale}];
            b[1].asks=vec![Level{price:d(2001),units:5000*scale}];
            let thin=vec![Level{price:d(2000),units:65*scale},
                Level{price:if side==Side::Sell{d(1999)}else{d(2001)},units:5000*scale}];
            if side==Side::Sell{b[1].bids=thin;}else{b[1].asks=thin;}
            let sent=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,
                Box::new(SlicedRepairBackend{requests:sent.clone(),scale})).unwrap());
            let dir=std::env::temp_dir().join(format!("cpa-rc10-slices-{}",uuid::Uuid::new_v4()));
            let path=dir.join("ledger.sqlite");
            let (mut db,_)=Store::open(&path,&s.config).unwrap();
            db.commit(&s,now,"synthetic_incident").unwrap();
            for round in 0..4 {
                for x in &mut a {x.position_units=s.positions[x.venue.index()].units;x.observed_ms=now;}
                for x in &mut b {x.received_ms=now;}
                assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap(),"round {round}, {:?}, {}, {:?}",s.status,s.reason,s.pending);
                advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
                let req=s.pending.as_ref().unwrap().repair.clone().unwrap();
                let fill=if round==0{40*scale}else{req.units};
                let total=s.pending.as_ref().unwrap().repair_filled;
                apply(&mut s,2,&req,result(&req,fill,true)).unwrap();
                assert_eq!(s.pending.as_ref().unwrap().repair_filled,total,"duplicate receipt counted twice");
                db.commit(&s,now,"synthetic_duplicate").unwrap();
                // Close the real SQLite connection and restore the exact durable state.
                drop(db);let (restored,state)=Store::open(&path,&s.config).unwrap();db=restored;s=state;
                advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
                if round<3 {
                    now=s.pending.as_ref().unwrap().repair_retry_after_ms.unwrap();
                }
            }
            let requests=sent.lock().unwrap();
            assert_eq!(requests.iter().map(|r|r.units/scale).collect::<Vec<_>>(),vec![60,60,60,20]);
            assert_eq!(requests.iter().map(|r|&r.id).collect::<std::collections::BTreeSet<_>>().len(),4);
            assert!(requests.iter().all(|r|r.reduce_only && r.venue==Venue::Entropy && r.side==side));
            assert!(s.pending.is_none() && s.paused && s.stop_requested);
            assert!(s.loss_stop.is_none());
            assert_eq!(serde_json::to_value(&s.lots).unwrap(),untouched);
            assert_eq!(s.paired_units(),810*scale);
            assert_eq!(s.positions[0].units,-s.positions[1].units);
            assert_eq!(s.positions[0].units.abs(),810*scale);
            assert_eq!(s.config.execution_slippage_bps,Decimal::ONE);
            drop(requests);drop(db);std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[tokio::test]
async fn workflow_dispatches_only_remaining_quantity_then_finishes_without_touching_old_pairs() {
    let now=crate::domain::now_ms();
    let (mut s,mut a,b)=incident(now,Direction::LighterShort);
    let requests=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let workers=[Venue::Lighter,Venue::Entropy].map(|v| venue::AccountWorker::spawn(v,
        Mode::Paper,true,Box::new(RepairBackend(requests.clone()))).unwrap());
    let (mut db,_)=Store::offline_replay(&s.config).unwrap();
    let old=s.pending.as_ref().unwrap().repair.clone().unwrap();
    apply(&mut s,2,&old,result(&old,40,true)).unwrap();
    a[1].position_units=s.positions[1].units;
    s.pending.as_mut().unwrap().repair_retry_after_ms=None;
    advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
    assert_eq!(s.pending.as_ref().unwrap().repair_retry_after_ms,Some(now+5000));
    assert!(!resume_protected_repair(&mut s,&a,&b,now).unwrap());
    let due=now+5000;
    a.iter_mut().for_each(|a|a.observed_ms=due);
    let mut fresh=b.clone();fresh.iter_mut().for_each(|b|b.received_ms=due);
    assert!(resume_protected_repair(&mut s,&a,&fresh,due).unwrap());
    // These flags must survive repair completion, preventing new entries.
    s.paused=true;s.stop_requested=true;
    advance(&mut s,&mut db,&workers,&fresh,due).await.unwrap();
    advance(&mut s,&mut db,&workers,&fresh,due).await.unwrap();
    let sent=requests.lock().unwrap();
    assert_eq!(sent.len(),1);
    assert_eq!(sent[0].units,50);
    assert!(sent[0].reduce_only);
    assert_eq!(sent[0].side,Side::Sell);
    assert_eq!(sent[0].venue,Venue::Entropy);
    assert_eq!(sent[0].id,"incident-13-v2-repair-1");
    assert!(s.pending.is_none() && s.recovery_after_ms.is_none());
    assert_eq!(s.lots.len(),10);
    assert_eq!([s.positions[0].units,s.positions[1].units],[-850,850]);
}

// Sep 25 16:42: three selected groups closed on Lighter; Entropy could not
// construct either the hedge or repair inside the unchanged protection limit.
fn close_incident(now:u64, direction:Direction) -> (Snapshot,[AccountEvidence;2],[Book;2]) {
    let (mut s,mut a,b)=incident(now,direction);
    s.pending=None;
    s.fills.clear();s.fill_opening.clear();s.lots.clear();
    s.positions=Default::default();
    for (i,units) in [80,60,90,90,80,90,90,90,90,90,90,90,90].into_iter().enumerate() {
        let id=format!("group-{i}");
        for venue in [Venue::Lighter,Venue::Entropy] {
            s.record_fill(&Fill {id:format!("{id}-{venue:?}"),order_id:format!("{id}-v2-first"),
                venue,side:direction.open_side(venue),units,price:d(1650),fee:Decimal::ZERO,
                time_ms:now-20_000},None).unwrap();
        }
        s.lots.push(Lot{id,units,level:i,opened_ms:now-20_000,entry_spread:d(24),entry_net_spread:Some(d(24))});
    }
    s.opened_groups=14;s.closed_groups=1;
    let mut p:Operation=serde_json::from_value(serde_json::json!({
        "id":"close-19","action":"close","level":5,"requested_units":270,
        "created_ms":now-10_000,"first":null,"hedge":null,"repair":null,
        "first_terminal":true,"hedge_terminal":true,"repair_terminal":true,
        "first_filled":270,"hedge_filled":0,"repair_filled":0,"first_value":"445770",
        "hedge_value":"0","failed":true,"first_venue":"lighter",
        "close_allocations":[{"lot_id":"group-10","units":90},{"lot_id":"group-11","units":90},
                             {"lot_id":"group-12","units":90}]
    })).unwrap();
    let first=OrderRequest{id:"close-19-v2-first".into(),venue:Venue::Lighter,
        side:direction.open_side(Venue::Lighter).opposite(),units:270,limit:d(1651),
        arrival_mid:None,reduce_only:true,created_ms:now-9000,expires_ms:now-4000,signed_expires_ms:None};
    s.record_fill(&Fill{id:"close-first".into(),order_id:first.id.clone(),venue:first.venue,
        side:first.side,units:270,price:d(1651),fee:Decimal::ZERO,time_ms:now-8500},None).unwrap();
    p.first=Some(first);p.repair_retry_after_ms=Some(now);s.pending=Some(p);
    for x in &mut a {x.position_units=s.positions[x.venue.index()].units;}
    (s,a,b)
}

#[tokio::test]
async fn close_retry_finishes_only_selected_three_groups_in_both_directions() {
    let now=crate::domain::now_ms();
    for direction in [Direction::LighterShort,Direction::LighterLong] {
        for hedge_fill in [0,90] {
            for repaired in [0,40] {
                let (mut s,mut a,b)=close_incident(now,direction);
                let untouched=serde_json::to_value(&s.lots[..10]).unwrap();
                for (which,n,suffix) in [(1,hedge_fill,"hedge"),(2,repaired,"repair")] {
                    if n==0 {continue;}
                    let r=request(&s,&b,Venue::Entropy,direction.open_side(Venue::Entropy).opposite(),
                        270-hedge_fill*(which==2) as i64,true,suffix,now).unwrap();
                    if which==1 {s.pending.as_mut().unwrap().hedge=Some(r.clone());}
                    else {s.pending.as_mut().unwrap().repair=Some(r.clone());}
                    apply(&mut s,which,&r,result(&r,n,true)).unwrap();
                }
                s.reason=RESIDUAL_HALT.into();
                a[1].position_units=s.positions[1].units;
                let requests=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,
                    Mode::Paper,true,Box::new(RepairBackend(requests.clone()))).unwrap());
                let (mut db,_)=Store::offline_replay(&s.config).unwrap();
                s.pending.as_mut().unwrap().repair_retry_after_ms=None;
                advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
                assert_eq!(s.pending.as_ref().unwrap().repair_retry_after_ms,Some(now+5000));
                assert!(!automatic_repair_due(&s,now+4999));
                let at=now+5000;
                a.iter_mut().for_each(|x|x.observed_ms=at);
                let mut b=b;b.iter_mut().for_each(|x|x.received_ms=at);
                assert!(resume_protected_repair(&mut s,&a,&b,at).unwrap());
                s.paused=true;s.stop_requested=true;
                advance(&mut s,&mut db,&workers,&b,at).await.unwrap();
                let req=s.pending.as_ref().unwrap().repair.clone().unwrap();
                apply(&mut s,2,&req,result(&req,270-hedge_fill-repaired,true)).unwrap();
                advance(&mut s,&mut db,&workers,&b,at).await.unwrap();
                let sent=requests.lock().unwrap();
                assert_eq!(sent.len(),1);assert_eq!(sent[0].venue,Venue::Entropy);
                assert_eq!(sent[0].side,direction.open_side(Venue::Entropy).opposite());
                assert!(sent[0].reduce_only);assert_eq!(sent[0].units,270-hedge_fill-repaired);
                assert_eq!(sent[0].id,"close-19-v2-repair-1");
                assert!(s.pending.is_none());assert!(s.paused && s.stop_requested);
                assert_eq!(serde_json::to_value(&s.lots).unwrap(),untouched);
                assert_eq!(s.opened_groups,14);assert_eq!(s.closed_groups,4);
                assert_eq!(s.positions[0].units.abs(),850);
                assert_eq!(s.positions[0].units,-s.positions[1].units);
                assert_eq!(s.closed_lot_allocations["close-19"].len(),3);
            }
        }
    }
}

#[test]
fn close_retry_rejects_unknown_unowned_stale_and_unprotected_residuals_without_mutation() {
    let now=100_000;
    for case in 0..15 {
        let (mut s,mut a,mut b)=close_incident(now,Direction::LighterShort);
        match case {
            0=>s.pending.as_mut().unwrap().first_terminal=false,
            1=>s.pending.as_mut().unwrap().hedge_terminal=false,
            2=>s.pending.as_mut().unwrap().repair_terminal=false,
            3=>s.pending.as_mut().unwrap().first_filled=269,
            4=>s.pending.as_mut().unwrap().close_allocations[0].units=100,
            5=>s.pending.as_mut().unwrap().close_allocations[0].lot_id="not-owned".into(),
            6=>s.pending.as_mut().unwrap().close_allocations[0].lot_id="group-11".into(),
            7=>a[1].position_units+=10,
            8=>{a[1].position_units+=10;s.positions[1].units+=10;},
            9=>a[1].observed_ms=now-4000,
            10=>a[1].open_orders=1,
            11=>b[1].received_ms=now-2000,
            12=>b[1].bids=vec![Level{price:d(1628),units:9},Level{price:d(1627),units:1000}],
            13=>s.pending.as_mut().unwrap().repair_retry_after_ms=Some(now+1),
            _=>s.config.auto_neutralize=false,
        }
        let before=serde_json::to_value(&s).unwrap();
        assert!(!resume_protected_repair(&mut s,&a,&b,now).unwrap_or(false),"case {case}");
        assert_eq!(serde_json::to_value(&s).unwrap(),before,"case {case}");
    }
}

#[test]
fn close_retry_counter_and_stop_loss_survive_restart() {
    let now=100_000;
    let (mut s,a,b)=close_incident(now,Direction::LighterShort);
    s.paused=true;s.stop_requested=true;s.loss_stop=Some(LossStop{at_ms:now,net_pnl:d(-30)});
    for attempt in 1..=8 {
        assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
        assert!(s.paused && s.stop_requested && s.loss_stop.is_some());
        let p=s.pending.as_mut().unwrap();assert_eq!(p.repair_attempt,attempt);
        p.repair_terminal=true;p.repair_retry_after_ms=Some(now);
        s.status=Status::NeedsAttention;s.reason=RESIDUAL_HALT.into();
        s=serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap();
    }
    assert!(automatic_repair_due(&s,now));
    assert_eq!(s.config.execution_slippage_bps,Decimal::ONE);
}

#[test]
fn apparent_completion_cannot_erase_unbalanced_positions_or_pending_record() {
    let now=100_000;
    for direction in [Direction::LighterShort,Direction::LighterLong] {
        for close in [false,true] {
            let (mut s,_,_)=if close {close_incident(now,direction)} else {incident(now,direction)};
            // Counter says repaired, but no authenticated reducing fill exists.
            let p=s.pending.as_mut().unwrap();p.repair_filled=p.first_filled;
            let before=serde_json::to_value(&s).unwrap();
            let error=s.finish_operation(now).unwrap_err().to_string();
            assert!(error.contains("both venue positions"));
            assert_eq!(serde_json::to_value(&s).unwrap(),before);
        }
    }
}

#[test]
fn every_pending_phase_blocks_new_entry_and_unknown_recovery_requests_are_not_resent() {
    let now=100_000;
    for close in [false,true] {
        let (s,a,b)=if close {close_incident(now,Direction::LighterShort)}
            else {incident(now,Direction::LighterShort)};
        for status in [Status::Running,Status::NeedsAttention,Status::RecoveringExposure,Status::Recovering] {
            let mut pending=s.clone();pending.status=status;
            let id=pending.pending.as_ref().unwrap().id.clone();
            assert!(strategy::evaluate(&mut pending,&b,&a,now).unwrap().is_none());
            assert_eq!(pending.pending.as_ref().unwrap().id,id);
        }
        let mut pending=s.clone();
        let p=pending.pending.as_mut().unwrap();
        if close {
            p.first_filled=223;p.hedge=None;p.hedge_terminal=false;
            p.repair=None;p.repair_terminal=false;p.align_close_terminal=false;
            pending.reason=ALIGNMENT_HALT.into();
        } else {
            p.hedge_filled=51;p.unwind_hedge_filled=40;p.unwind_hedge_terminal=false;
            p.repair=None;p.repair_terminal=false;pending.reason=UNWIND_HALT.into();
        }
        let before=serde_json::to_value(&pending).unwrap();
        assert!(!resume_protected_repair(&mut pending,&a,&b,now).unwrap());
        assert_eq!(serde_json::to_value(&pending).unwrap(),before);
    }
}

#[tokio::test]
async fn partially_filled_close_alignment_keeps_only_remaining_quantity_then_pairs_the_other_leg() {
    let now=crate::domain::now_ms();
    for direction in [Direction::LighterShort,Direction::LighterLong] {
        let (mut s,mut a,b)=close_incident(now,direction);
        s.fills.get_mut("Lighter:close-first").unwrap().units=223;
        s.positions[0].units=direction.open_side(Venue::Lighter).sign()*(1120-223);
        let p=s.pending.as_mut().unwrap();p.first_filled=223;p.first_value=d(1651)*d(223);
        p.hedge_terminal=false;p.repair_terminal=false;
        let r=request(&s,&b,Venue::Lighter,direction.open_side(Venue::Lighter).opposite(),
            7,true,"align-close",now).unwrap();
        s.pending.as_mut().unwrap().align_close=Some(r.clone());
        apply(&mut s,4,&r,result(&r,3,true)).unwrap();
        s.status=Status::NeedsAttention;s.reason=ALIGNMENT_HALT.into();
        for x in &mut a {x.position_units=s.positions[x.venue.index()].units;}
        let requests=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,
            Mode::Paper,true,Box::new(RepairBackend(requests.clone()))).unwrap());
        let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
        s.paused=true;s.stop_requested=true;
        for _ in 0..3 {advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
        let sent=requests.lock().unwrap();
        assert_eq!(sent.len(),2);
        assert_eq!((sent[0].venue,sent[0].units),(Venue::Lighter,4));
        assert_eq!(sent[0].id,"close-19-v2-align-close-1");
        assert_eq!((sent[1].venue,sent[1].units),(Venue::Entropy,230));
        assert!(sent.iter().all(|r|r.reduce_only));
        assert!(s.pending.is_none());assert_eq!(s.paired_units(),890);
        assert_eq!(s.positions[0].units,-s.positions[1].units);
        assert_eq!(s.positions[0].units.abs(),890);
        assert_eq!(s.lots.last().unwrap().units,40);
    }
}

#[tokio::test]
async fn non_common_hedge_partial_unwind_is_retried_before_original_first_leg_repair() {
    let now=crate::domain::now_ms();
    for direction in [Direction::LighterShort,Direction::LighterLong] {
        let (mut s,mut a,b)=incident(now,direction);
        let p=s.pending.as_mut().unwrap();p.repair=None;p.repair_terminal=false;
        let h=p.hedge.clone().unwrap();
        apply(&mut s,1,&h,result(&h,51,true)).unwrap();
        let r=request(&s,&b,Venue::Lighter,direction.open_side(Venue::Lighter).opposite(),
            51,true,"unwind-hedge",now).unwrap();
        s.pending.as_mut().unwrap().unwind_hedge=Some(r.clone());
        apply(&mut s,3,&r,result(&r,40,true)).unwrap();
        s.status=Status::NeedsAttention;s.reason=UNWIND_HALT.into();
        for x in &mut a {x.position_units=s.positions[x.venue.index()].units;}
        let requests=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,
            Mode::Paper,true,Box::new(RepairBackend(requests.clone()))).unwrap());
        let (mut db,_)=Store::offline_replay(&s.config).unwrap();
        assert!(resume_protected_repair(&mut s,&a,&b,now).unwrap());
        s.paused=true;s.stop_requested=true;
        for _ in 0..3 {advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
        let sent=requests.lock().unwrap();assert_eq!(sent.len(),2);
        assert_eq!((sent[0].venue,sent[0].units),(Venue::Lighter,11));
        assert_eq!(sent[0].id,"incident-13-v2-unwind-hedge-1");
        assert_eq!((sent[1].venue,sent[1].units),(Venue::Entropy,90));
        assert!(sent.iter().all(|r|r.reduce_only));
        assert!(s.pending.is_none());assert_eq!(s.lots.len(),10);
        assert_eq!(s.positions[0].units,-s.positions[1].units);
        assert_eq!(s.positions[0].units.abs(),850);
    }
}
