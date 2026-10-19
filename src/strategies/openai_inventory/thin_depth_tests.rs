// Real failure shape: inventory exceeds the visible book, but one protected
// paired group fits. Fixtures contain no account credentials or venue calls.
fn thin_group_fixture(now:u64,market:MarketPair,scale:i64,reverse:bool)
    -> (Snapshot,[AccountEvidence;2],i64) {
    let (mut s,mut a)=group_fixture(now,reverse);
    s.config.market=market;
    if scale==10 {s.config.set_initial_capital(10).unwrap();}
    let factor=10_i64.pow(market.quantity_decimals()-4)*scale;
    for p in &mut s.positions {p.units*=factor;}
    for lot in &mut s.lots {lot.units*=factor;}
    for (i,x) in a.iter_mut().enumerate() {
        x.position_units=s.positions[i].units;x.free_margin=d(1000);x.equity=d(1000);
    }
    (s,a,1000*factor)
}

fn thin_books(now:u64,spread:i64,reverse:bool,units:i64)->[Book;2] {
    let mut b=bidir_books(now,spread,reverse);
    for book in &mut b {for level in book.bids.iter_mut().chain(&mut book.asks) {level.units=units;}}
    b
}

#[test]
fn thin_depth_does_not_block_confirmed_profitable_group_both_markets_and_capitals() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for scale in [1,10] {for reverse in [false,true] {
            let now=4_000_000;let (mut s,mut a,group)=thin_group_fixture(now,market,scale,reverse);
            let mut b=thin_books(now,12,reverse,group);
            assert!(s.total_pnl(&b).is_err(),"whole inventory is not executable");
            let value=s.risk_valuation(&b,now).unwrap();
            assert!(!value.depth_complete);assert_eq!(value.unseen_exit_units,[group;2]);
            assert!(strategy::evaluate(&mut s,&b,&a,now).unwrap().is_none());
            let t=now+15000;for x in &mut a {x.observed_ms=t;}for book in &mut b {book.received_ms=t;}
            let op=strategy::evaluate(&mut s,&b,&a,t).unwrap().unwrap();
            assert_eq!(op.action,Action::Close);assert_eq!(op.requested_units,group);
            assert_eq!(op.close_lot_id.as_deref(),Some("group-1"));
            assert_eq!(op.close_allocations.len(),1);
            assert_eq!(s.positions[0].units,-s.positions[1].units);
        }}
    }
}

#[test]
fn thin_depth_loss_latches_and_selects_only_executable_paired_slice() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for scale in [1,10] {for reverse in [false,true] {
            let now=4_000_000;let (mut s,a,group)=thin_group_fixture(now,market,scale,reverse);
            s.config.max_loss_usdc=d(scale);s.config.close_slice_notional=d(150*scale);
            s.funding_synced_ms=0; // Loss exits do not wait for profit/funding gates.
            let b=thin_books(now,40,reverse,group);
            let risk=s.risk_valuation(&b,now).unwrap();
            assert!(!risk.depth_complete);assert!(risk.estimated_total_pnl<=-s.config.max_loss_usdc);
            let op=strategy::evaluate(&mut s,&b,&a,now).unwrap().unwrap();
            assert_eq!(op.action,Action::Close);assert_eq!(op.requested_units,group);
            assert!(s.loss_stop.is_some() && s.paused && s.close_requested && s.stop_after_close);
            assert_eq!(s.loss_stop.as_ref().unwrap().net_pnl,risk.estimated_total_pnl);
            let restored:Snapshot=serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(restored.loss_stop).unwrap(),serde_json::to_value(&s.loss_stop).unwrap());
        }}
    }
}

#[test]
fn thin_depth_risk_mark_preserves_costs_but_is_not_an_executable_profit_estimate() {
    let now=4_000_000;let (mut s,_)=loss_fixture(now);
    s.positions[0].realized=d(2);s.positions[0].fees=Decimal::new(5,1);
    s.positions[1].funding=Decimal::new(-3,1);s.round_start_pnl=d(1);
    let mut b=books(now,26);
    assert_eq!(s.risk_valuation(&b,now).unwrap().estimated_total_pnl,s.total_pnl(&b).unwrap());
    b[0].bids=vec![Level{price:d(100),units:2000},Level{price:d(99),units:3000}];
    b[1].asks=vec![Level{price:d(126),units:1000},Level{price:d(127),units:2000}];
    let mark=s.risk_valuation(&b,now).unwrap();
    assert_eq!(mark.unseen_exit_units,[5000,7000]);
    // Known long exit mark 99.2, short exit mark 126.9: -11.7 plus carry 1.2.
    assert_eq!(mark.estimated_total_pnl,Decimal::new(-105,1));
    assert!(s.total_pnl(&b).is_err());assert!(s.remaining_net(&b).is_err());
    assert!(strategy::enforce_loss_limit(&mut s,&b,now).unwrap());
    assert_eq!(s.loss_stop.unwrap().net_pnl,Decimal::new(-105,1));
}

#[test]
fn thin_depth_estimate_never_weakens_entry_or_invalid_quote_gates() {
    let now=4_000_000;let (s,a,group)=thin_group_fixture(now,MarketPair::Openai,10,false);
    let b=thin_books(now,12,false,group);
    assert!(s.risk_valuation(&b,now).unwrap().estimated_total_pnl>Decimal::ZERO);
    assert!(strategy::risk_check(&s,&b,&a,now,group,Action::Open).is_err());
    for bad in ["stale","future","disconnected","empty","crossed","invalid","unsorted"] {
        let mut b=b.clone();match bad {
            "stale"=>b[0].received_ms=now-s.config.book_max_age_ms-1,
            "future"=>b[0].received_ms=now+1,
            "disconnected"=>b[0].connected=false,
            "empty"=>b[0].bids.clear(),
            "crossed"=>b[0].bids[0].price=d(1000),
            "invalid"=>b[0].asks[0].units=0,
            _=>b[0].bids.push(Level{price:d(101),units:group}),
        }
        let mut state=s.clone();
        assert!(state.risk_valuation(&b,now).is_err(),"{bad}");
        assert!(strategy::evaluate(&mut state,&b,&a,now).is_err(),"{bad}");
        assert!(state.pending.is_none());assert!(state.loss_stop.is_none());
    }
}

#[test]
fn thin_depth_margin_protection_and_unfillable_loss_latch_remain_active() {
    let now=4_000_000;let (mut s,mut a,group)=thin_group_fixture(now,MarketPair::Openai,10,false);
    s.config.max_loss_usdc=d(300);s.config.close_slice_notional=d(150);
    a[0].equity=Decimal::ZERO;
    let b=thin_books(now,12,false,group);
    let op=strategy::evaluate(&mut s,&b,&a,now).unwrap().unwrap();
    assert_eq!(op.action,Action::Close);assert_eq!(op.requested_units,group);
    assert!(s.close_requested && s.loss_stop.is_none());
    // Not enough for a legal reducing slice: retain the stop, never fabricate depth.
    let (mut s,a,_)=thin_group_fixture(now,MarketPair::Openai,10,false);
    s.config.max_loss_usdc=d(1);
    let tiny=thin_books(now,40,false,1);
    assert!(strategy::evaluate(&mut s,&tiny,&a,now).unwrap().is_none());
    assert!(s.loss_stop.is_some() && s.close_requested);
    assert!(s.reason.contains("waiting for executable slice depth"));
    assert!(strategy::evaluate(&mut s,&b,&a,now).unwrap().is_some());
}

#[test]
fn thin_depth_slice_keeps_common_precision_and_slippage_limit() {
    let now=4_000_000;let (mut s,_,group)=thin_group_fixture(now,MarketPair::Openai,10,false);
    s.config.execution_slippage_bps=Decimal::ONE;
    let mut b=thin_books(now,12,false,group+7);
    b[0].bids.push(Level{price:d(90),units:group*10});
    b[1].asks.push(Level{price:d(120),units:group*10});
    assert_eq!(execution::protected_close_units(&s,&b,group*2,now).unwrap(),group);
    assert!(execution::protected_limit(&s,&b,Venue::Lighter,Side::Sell,group*2,now).is_err());
}

#[test]
fn thin_depth_smaller_than_one_group_still_confirms_a_profitable_legal_slice() {
    for market in [MarketPair::Openai,MarketPair::Anth] {for reverse in [false,true] {
        let now=4_000_000;let (mut s,mut a,group)=thin_group_fixture(now,market,10,reverse);
        let part=group*3/5;
        let b=thin_books(now,12,reverse,part);
        assert!(strategy::evaluate(&mut s,&b,&a,now).unwrap().is_none());
        let t=now+15000;for account in &mut a {account.observed_ms=t;}
        let b=thin_books(t,12,reverse,part);
        let mut op=strategy::evaluate(&mut s,&b,&a,t).unwrap().unwrap();
        assert_eq!(op.action,Action::Close);assert_eq!(op.requested_units,part);
        assert_eq!(op.close_lot_id.as_deref(),Some("group-1"));
        op.first_filled=part;op.hedge_filled=part;op.first_terminal=true;op.hedge_terminal=true;
        s.pending=Some(op);for p in &mut s.positions {p.units-=p.units.signum()*part;}
        s.finish_operation(t).unwrap();
        assert_eq!(s.lots[0].units,group);assert_eq!(s.lots[1].units,group-part);
        assert_eq!(s.closed_groups,0);assert_eq!(s.positions[0].units,-s.positions[1].units);
    }}
}

#[tokio::test]
async fn thin_depth_group_close_executes_and_reconciles_without_touching_other_lot() {
    use std::sync::{Arc,RwLock,atomic::AtomicU64};
    for reverse in [false,true] {
        let now=4_000_000;let (mut s,mut a,group)=thin_group_fixture(now,MarketPair::Openai,10,reverse);
        let b=thin_books(now,12,reverse,group);
        strategy::evaluate(&mut s,&b,&a,now).unwrap();
        let t=now+15000;for x in &mut a {x.observed_ms=t;}
        let b=thin_books(t,12,reverse,group);
        s.pending=strategy::evaluate(&mut s,&b,&a,t).unwrap();
        assert!(s.pending.is_some());
        let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(t));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn_replay(v,
            venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone()),clock.clone()).unwrap());
        let (mut store,_)=store::Store::offline_replay(&s.config).unwrap();
        for _ in 0..12 {
            if s.pending.is_none() {break;}
            execution::advance(&mut s,&mut store,&workers,&b,t).await.unwrap();
        }
        assert!(s.pending.is_none());assert_eq!(s.closed_groups,1);
        assert_eq!(s.lots.len(),1);assert_eq!(s.lots[0].id,"group-0");
        assert_eq!(s.positions[0].units.abs(),group);assert_eq!(s.positions[0].units,-s.positions[1].units);
    }
}

#[tokio::test]
async fn thin_depth_loss_exit_finishes_multiple_slices_then_stops() {
    use std::sync::{Arc,RwLock,atomic::AtomicU64};
    for market in [MarketPair::Openai,MarketPair::Anth] {for reverse in [false,true] {
        let now=4_000_000;let (mut s,mut a,group)=thin_group_fixture(now,market,10,reverse);
        s.config.max_loss_usdc=d(1);s.config.close_slice_notional=d(150);
        let b=thin_books(now,40,reverse,group);
        let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(now));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn_replay(v,
            venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone()),clock.clone()).unwrap());
        let (mut store,_)=store::Store::offline_replay(&s.config).unwrap();
        for _ in 0..4 {
            s.pending=strategy::evaluate(&mut s,&b,&a,now).unwrap();
            if let Some(op)=&s.pending {assert_eq!(op.action,Action::Close);assert!(op.requested_units<=group);}
            for _ in 0..12 {
                if s.pending.is_none() {break;}
                execution::advance(&mut s,&mut store,&workers,&b,now).await.unwrap();
                if let Some(op)=&s.pending {
                    for request in [&op.first,&op.hedge,&op.repair].into_iter().flatten() {assert!(request.reduce_only);}
                }
            }
            assert!(s.pending.is_none());
            let (l,e)=tokio::join!(workers[0].account(),workers[1].account());a=[l.unwrap(),e.unwrap()];
            s.assert_reconciled(&a).unwrap();
            if s.lots.is_empty() {break;}
        }
        assert!(s.lots.is_empty());assert_eq!(s.closed_groups,2);
        assert!(s.positions.iter().all(|p|p.units==0));
        strategy::enforce_loss_limit(&mut s,&b,now).unwrap();
        assert_eq!(s.status,Status::Stopped);assert!(s.loss_stop.is_some() && s.stop_requested);
    }}
}

#[tokio::test]
async fn thin_depth_after_entry_reservation_cancels_before_any_new_exposure() {
    use std::sync::{Arc,RwLock,atomic::AtomicU64};
    let now=4_000_000;let (mut s,mut a)=accumulation_fixture(now,false);
    for p in &mut s.positions {p.units*=10;}
    for lot in &mut s.lots {lot.units*=10;}
    for (i,account) in a.iter_mut().enumerate() {account.position_units=s.positions[i].units;}
    assert!(strategy::evaluate(&mut s,&books(now,12),&a,now).unwrap().is_none());
    let t=now+1000;for account in &mut a {account.observed_ms=t;}
    s.pending=strategy::evaluate(&mut s,&books(t,12),&a,t).unwrap();
    let op=s.pending.as_ref().unwrap();assert_eq!(op.action,Action::Open);
    let b=thin_books(t,12,false,s.paired_units()-s.config.common_step());
    assert!(b[0].vwap(s.direction.open_side(Venue::Lighter),op.requested_units).is_ok());
    assert!(s.total_pnl(&b).is_err());
    let before=s.positions.iter().map(|p|p.units).collect::<Vec<_>>();
    let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(t));
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn_replay(v,
        venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone()),clock.clone()).unwrap());
    let (mut store,_)=store::Store::offline_replay(&s.config).unwrap();
    execution::advance(&mut s,&mut store,&workers,&b,t).await.unwrap();
    assert!(s.pending.is_none());assert!(s.fills.is_empty());
    let (l,e)=tokio::join!(workers[0].account(),workers[1].account());
    assert_eq!(vec![l.unwrap().position_units,e.unwrap().position_units],before);
}
