use super::*;
use std::sync::{Arc, Mutex};

fn incident(market: MarketPair, now: u64) -> (Snapshot, OrderRequest, OrderResult) {
    let mut config = InventoryConfig::default();
    config.market = market;
    config.direction_policy = DirectionPolicy::Both;
    let mut s = Snapshot::new(config).unwrap();
    s.direction = Direction::LighterShort;
    s.paused = true;
    s.stop_requested = true;
    s.status = Status::NeedsAttention;
    s.reason = "order unresolved past execution deadline; reconciliation required".into();
    let exchange = now - 8 * 60 * 60 * 1000;
    let close = market == MarketPair::Openai;
    if close {
        for (i, units) in [80,60,90,90,80,90,90,90,90,90,90,90].into_iter().enumerate() {
            let id = format!("old-{i}");
            for venue in [Venue::Lighter, Venue::Entropy] {
                s.record_fill(&Fill { id: format!("{id}-{venue:?}"), order_id: format!("{id}-v2-first"),
                    venue, side: s.direction.open_side(venue), units, price: Decimal::from(2100),
                    fee: Decimal::ZERO, time_ms: exchange - 100_000 }, None).unwrap();
            }
            s.lots.push(Lot { id, units, level:i, opened_ms:exchange-100_000,
                entry_spread:Decimal::from(20), entry_net_spread:Some(Decimal::from(20)) });
        }
        s.opened_groups = 26;
        s.closed_groups = 14;
    }
    let req = OrderRequest { id:"clock-incident-v2-first".into(),
        venue:if close {Venue::Lighter} else {Venue::Entropy}, side:Side::Buy,
        units:if close {490} else {700}, limit:Decimal::from(2100), arrival_mid:None,
        reduce_only:close, created_ms:exchange+95_000, expires_ms:exchange+100_000,
        signed_expires_ms:close.then_some(exchange+694_000) };
    s.pending = Some(serde_json::from_value(serde_json::json!({
        "id":"clock-incident", "action":if close {"close"} else {"open"},
        "level":0,"requested_units":req.units,"created_ms":req.created_ms,
        "first":req,"hedge":null,"repair":null,"first_terminal":false,"hedge_terminal":false,
        "repair_terminal":false,"first_filled":0,"hedge_filled":0,"repair_filled":0,
        "first_value":"0","hedge_value":"0","failed":false,"first_venue":req.venue,
        "close_allocations":if close { s.lots[..6].iter().map(|l| CloseAllocation {
            lot_id:l.id.clone(),units:l.units }).collect::<Vec<_>>() } else {vec![]}
    })).unwrap());
    let fills = vec![Fill { id:"exchange-first-fill".into(), order_id:req.id.clone(),
        venue:req.venue,side:req.side,units:req.units,price:req.limit,fee:Decimal::new(1,3),
        time_ms:exchange+500 }];
    (s, req, OrderResult { exchange_created_ms:Some(exchange),terminal:true,fills,reason:"filled".into()})
}

struct RecoveryBackend {
    first: OrderResult,
    sent: Arc<Mutex<Vec<OrderRequest>>>,
}
impl venue::VenueBackend for RecoveryBackend {
    fn submit(&mut self, r: OrderRequest) -> venue::BoxFuture<'_,OrderResult> {
        Box::pin(async move {
            assert!(!r.id.ends_with("-first"), "original request must never be resubmitted");
            self.sent.lock().unwrap().push(r.clone());
            Ok(OrderResult { exchange_created_ms:Some(r.created_ms),terminal:true,reason:"filled".into(),
                fills:vec![Fill {id:format!("{}-fill",r.id),order_id:r.id,venue:r.venue,side:r.side,
                    units:r.units,price:r.limit,fee:Decimal::ZERO,time_ms:r.created_ms}]})
        })
    }
    fn lookup(&mut self, r: OrderRequest) -> venue::BoxFuture<'_,OrderResult> {
        Box::pin(async move {
            assert_eq!(r.id,"clock-incident-v2-first");
            Ok(self.first.clone())
        })
    }
    fn account(&mut self) -> venue::BoxFuture<'_,AccountEvidence> {
        Box::pin(async {anyhow::bail!("offline fixture has no real account")})
    }
}

#[tokio::test]
async fn skewed_pending_open_and_close_survive_restart_and_finish_once_with_equal_legs() {
    for (market,already_recorded) in [(MarketPair::Openai,false),(MarketPair::Anth,false),
        (MarketPair::Openai,true),(MarketPair::Anth,true)] {
        let now = crate::domain::now_ms();
        let (mut s, req, result) = incident(market,now);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let workers = [Venue::Lighter,Venue::Entropy].map(|v| venue::AccountWorker::spawn(v,
            Mode::Paper,true,Box::new(RecoveryBackend {first:result.clone(),sent:sent.clone()})).unwrap());
        let dir = std::env::temp_dir().join(format!("clock-recovery-{}",s.instance_id));
        let path = dir.join("ledger.sqlite");
        let (mut db,_) = Store::open(&path,&s.config).unwrap();
        // Simulate an incomplete history page. Durable recovery must deduplicate
        // this fill when the full page is returned after a real ledger reopen.
        let mut partial = result.clone(); partial.terminal = false;
        if already_recorded { apply(&mut s,0,&req,partial).unwrap(); }
        db.commit(&s,now,"partial_history").unwrap(); drop(db);
        let (mut db,mut s) = Store::open(&path,&s.config).unwrap();
        assert_eq!(s.pending.as_ref().unwrap().first.as_ref().unwrap().created_ms,req.created_ms);
        s.status = Status::NeedsAttention;
        s.reason = "order unresolved past execution deadline; reconciliation required".into();
        assert_eq!(recheck_timed_out(&mut s,&mut db,&workers,now).await.unwrap(),"filled");
        assert_eq!(s.status,Status::Recovering);
        assert_eq!(s.pending.as_ref().unwrap().first_filled,req.units);
        assert!(sent.lock().unwrap().is_empty());
        let fill = s.fills.values().find(|f|f.id=="exchange-first-fill").unwrap();
        assert_eq!(fill.time_ms,result.fills[0].time_ms);
        let b = [2120,2100].map(|p|Book {bids:vec![Level {price:Decimal::from(p),units:10_000}],
            asks:vec![Level {price:Decimal::from(p),units:10_000}],received_ms:now,connected:true});
        advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
        advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
        assert!(s.pending.is_none(),"{:?}: {}",market,s.reason);
        assert!(s.paused && s.stop_requested);
        let remaining = if market==MarketPair::Openai {540} else {700};
        assert_eq!([s.positions[0].units,s.positions[1].units],[-remaining,remaining]);
        assert_eq!(s.paired_units(),remaining);
        assert_eq!(s.lots.len(),if market==MarketPair::Openai {6} else {1});
        assert_eq!(s.closed_groups,if market==MarketPair::Openai {20} else {0});
        let requests = sent.lock().unwrap();
        assert_eq!(requests.len(),1);
        assert_eq!(requests[0].units,req.units);
        assert_eq!(requests[0].venue,if market==MarketPair::Openai {Venue::Entropy} else {Venue::Lighter});
        assert_eq!(requests[0].reduce_only,market==MarketPair::Openai);
        drop(requests);drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn exchange_time_anchor_does_not_allow_foreign_old_future_or_overfilled_evidence() {
    let (s,req,result) = incident(MarketPair::Openai,crate::domain::now_ms());
    for case in 0..7 {
        let mut bad = result.clone();
        match case {
            0=>bad.exchange_created_ms=None,
            1=>bad.exchange_created_ms=Some(req.created_ms-300_001),
            2=>bad.fills[0].order_id="foreign-order".into(),
            3=>bad.fills[0].venue=Venue::Entropy,
            4=>bad.fills[0].time_ms=bad.exchange_created_ms.unwrap()-1001,
            5=>bad.fills[0].time_ms=req.signed_expiry()+300_001,
            _=>bad.fills[0].units+=1,
        }
        assert!(apply(&mut s.clone(),0,&req,bad).is_err(),"case {case}");
    }
}
