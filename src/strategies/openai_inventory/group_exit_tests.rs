fn group_fixture(now:u64,reverse:bool)->(Snapshot,[AccountEvidence;2]) {
    let mut s=warmed(now);s.config.exit_policy=ExitPolicy::PerGroup;
    s.config.direction_policy=DirectionPolicy::Both;
    s.config.execution_slippage_bps=Decimal::ZERO;s.config.fee_entropy=Decimal::ZERO;
    s.paused=true;s.anchor=Some(d(10));
    for (_,m) in &mut s.samples{*m=d(15);}
    s.positions[0]=Position{units:2000,average:d(100),..Default::default()};
    s.positions[1]=Position{units:-2000,average:d(115),..Default::default()};
    s.lots=[10,20].into_iter().enumerate().map(|(i,p)|Lot{id:format!("group-{i}"),level:i,units:1000,opened_ms:now-60000,entry_spread:d(p),entry_net_spread:Some(d(p))}).collect();
    let mut a=accounts(now);a[0].position_units=2000;a[1].position_units=-2000;
    if reverse {mirror_fixture(&mut s,&mut a);}
    (s,a)
}

#[test]
fn per_group_selects_profitable_lot_and_round_exits_combined_inventory() {
    for reverse in [false,true] {
        let now=4_000_000;let(mut s,mut a)=group_fixture(now,reverse);
        let mut round=s.clone();round.config.exit_policy=ExitPolicy::Round;
        assert!(strategy::evaluate(&mut s,&bidir_books(now,12,reverse),&a,now).unwrap().is_none());
        assert!(strategy::evaluate(&mut round,&bidir_books(now,12,reverse),&a,now).unwrap().is_none());
        let next=now+15000;for x in &mut a{x.observed_ms=next;}
        let op=strategy::evaluate(&mut s,&bidir_books(next,12,reverse),&a,next).unwrap().unwrap();
        assert_eq!(op.close_lot_id.as_deref(),Some("group-1"));assert_eq!(op.requested_units,1000);
        let all=strategy::evaluate(&mut round,&bidir_books(next,12,reverse),&a,next).unwrap().unwrap();
        assert_eq!(all.close_lot_id,None);assert_eq!(all.requested_units,2000);
        s.pending=Some(op.clone());
        assert!(strategy::normal_exit_eligible(&s,&bidir_books(next,12,reverse),next).unwrap());
        assert!(!strategy::normal_exit_eligible(&s,&bidir_books(next,19,reverse),next).unwrap());
        // Simulate a paired partial completion: only the selected lot is reduced.
        let mut partial=op;partial.first_filled=500;partial.hedge_filled=500;
        partial.first_terminal=true;partial.hedge_terminal=true;s.pending=Some(partial);
        for p in &mut s.positions {p.units=p.units.signum()*1500;}
        s.finish_operation(next).unwrap();
        assert_eq!(s.lots[0].units,1000);assert_eq!(s.lots[1].units,500);
        assert_eq!(s.lots[1].entry_net_spread,Some(d(20)));
    }
}

#[test]
fn group_threshold_includes_costs_protected_prices_depth_and_two_signals() {
    let now=4_000_000;let(mut s,mut a)=group_fixture(now,false);let b=books(now,18);
    assert!(strategy::group_exit_eligible(&s,&s.lots[1],&b,1000,now).unwrap());
    s.config.execution_slippage_bps=d(1);
    assert!(!strategy::group_exit_eligible(&s,&s.lots[1],&b,1000,now).unwrap());
    s.config.execution_slippage_bps=Decimal::ZERO;
    s.lots[1].entry_net_spread=Some(d(17));
    assert!(!strategy::group_exit_eligible(&s,&s.lots[1],&b,1000,now).unwrap());
    s.lots[1].entry_net_spread=None;
    assert!(strategy::group_exit_eligible(&s,&s.lots[1],&b,1000,now).is_err());
    s.lots[1].entry_net_spread=Some(d(20));
    let mut thin=b.clone();thin[0].bids[0].units=100;
    assert!(strategy::group_exit_eligible(&s,&s.lots[1],&thin,1000,now).is_err());
    assert!(strategy::evaluate(&mut s,&b,&a,now).unwrap().is_none());
    let later=now+45000;for x in &mut a{x.observed_ms=later;}
    assert!(strategy::evaluate(&mut s,&books(later,18),&a,later).unwrap().is_none());
    let later=later+15000;for x in &mut a{x.observed_ms=later;}
    assert!(strategy::evaluate(&mut s,&books(later,19),&a,later).unwrap().is_none());
    let later=later+15000;for x in &mut a{x.observed_ms=later;}
    assert!(strategy::evaluate(&mut s,&books(later,18),&a,later).unwrap().is_none());
}

#[test]
fn group_loss_exit_overrides_profit_target_and_persists_identity() {
    let now=4_000_000;let(mut s,a)=group_fixture(now,false);
    s.previous_group_exit.insert("group-1".into(),now-15000);
    let serialized=serde_json::to_string(&s).unwrap();
    let mut restored:Snapshot=serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored.previous_group_exit,s.previous_group_exit);
    restored.config.max_loss_usdc=d(1);
    let op=strategy::evaluate(&mut restored,&books(now,40),&a,now).unwrap().unwrap();
    assert_eq!(op.action,Action::Close);assert_eq!(op.close_lot_id,None);
    assert!(restored.loss_stop.is_some());assert!(restored.stop_after_close);
    s.config.mode=Mode::Live;assert!(s.config.validate().is_err());
}

#[tokio::test]
async fn comparison_same_policy_has_identical_economics_and_restores_with_group_target() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    let now=crate::domain::now_ms();let mut seed=warmed(now);
    seed.config.execution_slippage_bps=Decimal::ZERO;
    seed.config.direction_policy=DirectionPolicy::Both;
    let folder=std::env::temp_dir().join(format!("exit-comparison-test-{}",seed.instance_id));
    std::fs::create_dir_all(&folder).unwrap();
    let shared=Arc::new(RwLock::new(books(now,16)));let clock=Arc::new(AtomicU64::new(now));
    let cfg=seed.config.clone();
    let mut a=comparison::Variant::open(&folder.join("a.sqlite"),cfg.clone(),&seed,now,shared.clone(),clock.clone()).unwrap();
    let mut b=comparison::Variant::open(&folder.join("b.sqlite"),cfg,&seed,now,shared.clone(),clock.clone()).unwrap();
    assert_eq!(a.summary(&[Book::default(),Book::default()],now)["estimated_net_if_closed_now"],"0");
    for offset in [15000,30000,30250,30500,30750,31000,31250,31500] {
        let t=now+offset;let frame=books(t,16);*shared.write().unwrap()=frame.clone();clock.store(t,Ordering::SeqCst);
        let(x,y)=tokio::join!(a.tick(&frame,t),b.tick(&frame,t));x.unwrap();y.unwrap();
    }
    assert_eq!(a.state.lots.len(),1);assert_eq!(b.state.lots.len(),1);
    assert!(a.summary(&[Book::default(),Book::default()],now)["estimated_net_if_closed_now"].is_null());
    assert_eq!(a.state.positions[0].units,b.state.positions[0].units);
    assert_eq!(a.state.total_pnl(&books(now+31500,16)).unwrap(),b.state.total_pnl(&books(now+31500,16)).unwrap());
    let opening_net=a.state.lots[0].entry_net_spread.unwrap();
    let qty=quantity(a.state.paired_units());
    assert_eq!(opening_net*qty,qty*d(16)-a.state.cumulative_fees());
    let mut cfg=a.state.config.clone();cfg.exit_policy=ExitPolicy::PerGroup;
    a.state.config=cfg.clone();a.store.commit(&a.state,now+31500,"test_group_policy").unwrap();
    drop(a);
    let restored=comparison::Variant::open(&folder.join("a.sqlite"),cfg,&seed,now,shared,clock).unwrap();
    assert_eq!(restored.state.lots[0].entry_net_spread,Some(opening_net));
    assert_eq!(restored.state.opened_groups,1);
}

#[tokio::test]
async fn selected_group_executes_both_reducing_legs_without_touching_old_lot() {
    use std::sync::{Arc,RwLock,atomic::AtomicU64};
    for reverse in [false,true] {
        let now=crate::domain::now_ms();let(mut s,mut a)=group_fixture(now,reverse);
        assert!(strategy::evaluate(&mut s,&bidir_books(now,12,reverse),&a,now).unwrap().is_none());
        let next=now+15000;for x in &mut a{x.observed_ms=next;}
        let b=bidir_books(next,12,reverse);
        s.pending=strategy::evaluate(&mut s,&b,&a,next).unwrap();
        assert_eq!(s.pending.as_ref().unwrap().close_lot_id.as_deref(),Some("group-1"));
        let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(next));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,
            Box::new(venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone()).with_logical_clock(clock.clone()))).unwrap());
        let path=std::env::temp_dir().join(format!("selected-group-{}.sqlite",s.instance_id));
        let(mut db,_)=store::Store::open(&path,&s.config).unwrap();
        for _ in 0..8 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&b,next).await.unwrap();}
        assert!(s.pending.is_none());assert_eq!(s.lots.len(),1);assert_eq!(s.lots[0].id,"group-0");
        assert_eq!(s.lots[0].units,1000);assert_eq!(s.closed_groups,1);
        assert_eq!(s.positions[0].units,1000*s.direction.sign());
        assert_eq!(s.positions[1].units,-1000*s.direction.sign());
        assert!(s.fill_opening.values().all(|x|!*x));
        let (l,e)=tokio::join!(workers[0].account(),workers[1].account());
        s.assert_reconciled(&[l.unwrap(),e.unwrap()]).unwrap();
    }
}

#[tokio::test]
async fn paired_comparison_different_exit_rules_diverge_only_after_common_entry() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    let now=crate::domain::now_ms();let mut seed=warmed(now);
    seed.config.execution_slippage_bps=Decimal::ZERO;seed.config.direction_policy=DirectionPolicy::Both;
    let folder=std::env::temp_dir().join(format!("exit-ab-divergence-{}",seed.instance_id));
    std::fs::create_dir_all(&folder).unwrap();
    let shared=Arc::new(RwLock::new(books(now,16)));let clock=Arc::new(AtomicU64::new(now));
    let mut cfg=seed.config.clone();
    let mut a=comparison::Variant::open(&folder.join("a.sqlite"),cfg.clone(),&seed,now,shared.clone(),clock.clone()).unwrap();
    cfg.exit_policy=ExitPolicy::PerGroup;
    let mut b=comparison::Variant::open(&folder.join("b.sqlite"),cfg,&seed,now,shared.clone(),clock.clone()).unwrap();
    for offset in [15000,30000,30250,30500,30750,31000,31250,31500,45000,60000,60250,60500,60750,61000,61250,61500] {
        let t=now+offset;let frame=books(t,if offset<45000{16}else{13});
        *shared.write().unwrap()=frame.clone();clock.store(t,Ordering::SeqCst);
        let(x,y)=tokio::join!(a.tick(&frame,t),b.tick(&frame,t));x.unwrap();y.unwrap();
    }
    assert_eq!(a.state.opened_groups,1);assert_eq!(b.state.opened_groups,1);
    assert_eq!(a.state.lots.len(),1);assert_eq!(b.state.lots.len(),0);
    assert_eq!(a.state.closed_groups,0);assert_eq!(b.state.closed_groups,1);
    assert!(b.state.total_pnl(&books(now+61500,13)).unwrap()>Decimal::ZERO);
}

#[test]
fn shared_rules_change_only_aggregation_and_apply_mean_to_both() {
    for reverse in [false,true] {
        let now=4_000_000;let (mut group,mut a)=group_fixture(now,reverse);
        group.config.shared_exit_conditions=true;
        for (_,m) in &mut group.samples {*m=d(17)*Decimal::from(group.direction.sign());}
        let mut round=group.clone();round.config.exit_policy=ExitPolicy::Round;
        // Both must wait above the common mean, even though the best group is profitable.
        let above=bidir_books(now,18,reverse);
        assert!(strategy::evaluate(&mut group,&above,&a,now).unwrap().is_none());
        assert!(strategy::evaluate(&mut round,&above,&a,now).unwrap().is_none());
        for t in [now+15000,now+30000] {
            for x in &mut a{x.observed_ms=t;}
            let b=bidir_books(t,16,reverse);
            let g=strategy::evaluate(&mut group,&b,&a,t).unwrap();
            let r=strategy::evaluate(&mut round,&b,&a,t).unwrap();
            assert!(r.is_none()); // Weighted entry 15: combined inventory still loses at 16.
            if t==now+30000 {assert_eq!(g.unwrap().close_lot_id.as_deref(),Some("group-1"));}
            else {assert!(g.is_none());}
        }
    }
}

#[test]
fn shared_group_requires_two_dollars_even_when_mean_and_profit_pass() {
    let now=4_000_000;let mut group=warmed(now);
    group.config.shared_exit_conditions=true;group.config.exit_policy=ExitPolicy::PerGroup;
    group.config.direction_policy=DirectionPolicy::Both;group.direction=Direction::LighterShort;group.paused=true;
    let decimal=|x:&str|x.parse::<Decimal>().unwrap();
    for (_,m) in &mut group.samples{*m=decimal("-1.5");}
    group.positions[0]=Position{units:-90,average:decimal("1628.287155"),..Default::default()};
    group.positions[1]=Position{units:90,average:decimal("1626.1626"),fees:decimal("0.001317191706"),..Default::default()};
    group.lots=vec![Lot{id:"historical-single".into(),level:0,units:90,opened_ms:now-60000,
        entry_spread:decimal("2.124555"),entry_net_spread:Some(decimal("1.978200366"))}];
    let mut round=group.clone();round.config.exit_policy=ExitPolicy::Round;
    for t in [now,now+15000] {
        let b=["1628.19","1627.2"].map(|p|Book{bids:vec![Level{price:decimal(p),units:10000}],
            asks:vec![Level{price:decimal(p),units:10000}],received_ms:t,connected:true});
        let mut a=accounts(t);a[0].position_units=-90;a[1].position_units=90;
        let g=strategy::evaluate(&mut group,&b,&a,t).unwrap();
        let r=strategy::evaluate(&mut round,&b,&a,t).unwrap();
        assert!(g.is_none(), "positive profit below the 2-dollar target must not close a group");
        if t==now+15000 {assert_eq!(r.unwrap().action,Action::Close);}
    }
    for t in [now+30000,now+45000] {
        let b=["1627.0","1627.5"].map(|p|Book{bids:vec![Level{price:decimal(p),units:10000}],
            asks:vec![Level{price:decimal(p),units:10000}],received_ms:t,connected:true});
        let mut a=accounts(t);a[0].position_units=-90;a[1].position_units=90;
        let g=strategy::evaluate(&mut group,&b,&a,t).unwrap();
        if t==now+45000 {assert_eq!(g.unwrap().close_lot_id.as_deref(),Some("historical-single"));}
        else {assert!(g.is_none());}
    }
}

#[tokio::test]
async fn shared_single_group_has_identical_full_execution_in_both_directions() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    for reverse in [false,true] {
        let now=crate::domain::now_ms();let mut seed=warmed(now);
        seed.config.direction_policy=DirectionPolicy::Both;seed.config.shared_exit_conditions=true;
        seed.config.entry_offset=Decimal::ONE;
        for (_,m) in &mut seed.samples {*m=d(if reverse{-10}else{10});}
        let folder=std::env::temp_dir().join(format!("shared-exit-{}",seed.instance_id));
        std::fs::create_dir_all(&folder).unwrap();
        let shared=Arc::new(RwLock::new(bidir_books(now,16,reverse)));
        let clock=Arc::new(AtomicU64::new(now));let cfg=seed.config.clone();
        let mut round=comparison::Variant::open(&folder.join("round.sqlite"),cfg.clone(),&seed,now,shared.clone(),clock.clone()).unwrap();
        let mut cfg=cfg;cfg.exit_policy=ExitPolicy::PerGroup;
        let mut group=comparison::Variant::open(&folder.join("group.sqlite"),cfg,&seed,now,shared.clone(),clock.clone()).unwrap();
        for offset in [15000,30000,30250,30500,30750,31000,31250,31500,45000,60000,60250,60500,60750,61000,61250,61500] {
            let t=now+offset;let b=bidir_books(t,if offset<45000{16}else{9},reverse);
            *shared.write().unwrap()=b.clone();clock.store(t,Ordering::SeqCst);
            let(a,b)=tokio::join!(round.tick(&b,t),group.tick(&b,t));a.unwrap();b.unwrap();
            assert_eq!(round.state.opened_groups,group.state.opened_groups);
            assert_eq!(round.state.closed_groups,group.state.closed_groups);
            assert_eq!(round.state.positions.iter().map(|p|p.units).collect::<Vec<_>>(),group.state.positions.iter().map(|p|p.units).collect::<Vec<_>>());
            assert_eq!(round.state.cumulative_fees(),group.state.cumulative_fees());
        }
        assert_eq!(round.state.closed_groups,1);assert_eq!(round.state.lots.len(),0);
        let economics=|s:&Snapshot|{
            let mut rows=s.fills.values().map(|f|(f.time_ms,format!("{:?}",f.venue),format!("{:?}",f.side),f.units,f.price,f.fee)).collect::<Vec<_>>();
            rows.sort();rows
        };
        assert_eq!(economics(&round.state),economics(&group.state));
        assert_eq!(round.state.total_pnl(&bidir_books(now+61500,9,reverse)).unwrap(),group.state.total_pnl(&bidir_books(now+61500,9,reverse)).unwrap());
    }
}

fn accumulation_fixture(now:u64,reverse:bool)->(Snapshot,[AccountEvidence;2]) {
    let(mut s,mut a)=group_fixture(now,reverse);
    s.config.shared_exit_conditions=true;s.config.entry_offset=Decimal::ONE;
    s.config.decision_ms=Some(1000);s.config.entry_confirmation_ms=Some(1000);
    s.config.accumulation=Some(super::config::AccumulationRules{entry_floor:d(5),interval_ms:900000,max_time_adds:2,contraction_ratio:Decimal::new(5,1)});
    for (_,m) in &mut s.samples {*m=d(5)*Decimal::from(s.direction.sign());}
    s.lots.truncate(1);s.positions[0].units/=2;s.positions[1].units/=2;
    for i in 0..2 {a[i].position_units=s.positions[i].units;}
    s.armed[0]=false;s.paused=false;
    s.last_open_completed=Some((now-900000,d(10)));
    seed_entry_quotes(&mut s, now, 5, reverse);
    (s,a)
}
#[test]
fn accumulation_floor_ratio_and_fee_protection_both_directions() {
    for reverse in [false,true] {
        let now=4_000_000;let(mut s,_)=accumulation_fixture(now,reverse);
        assert_eq!(s.config.entry_threshold(d(0)),d(5));
        assert_eq!(s.config.entry_threshold(d(10)),d(11));
        s.config.validate().unwrap();
        assert!(!strategy::group_exit_eligible(&s,&s.lots[0],&bidir_books(now,8,reverse),1000,now).unwrap());
        // A low mean must not prevent the fixed 50% contraction target.
        for (_,m) in &mut s.samples {*m=Decimal::ZERO;}
        assert!(strategy::group_exit_eligible(&s,&s.lots[0],&bidir_books(now,5,reverse),1000,now).unwrap());
        s.config.execution_slippage_bps=Decimal::ONE;
        assert!(!strategy::group_exit_eligible(&s,&s.lots[0],&bidir_books(now,5,reverse),1000,now).unwrap());
    }
}
#[test]
fn accumulation_time_delay_grid_priority_quota_and_restart() {
    for reverse in [false,true] {
        let now=4_000_000;
        for (spread,elapsed,used,expected) in [(10,899000,0,None),(10,900000,0,Some(20)),(10,900000,2,None),(12,1000,0,Some(1)),(9,900000,0,None)] {
            let(mut s,mut a)=accumulation_fixture(now,reverse);
            s.last_open_completed=Some((now-elapsed,d(10)));s.time_adds_used=used;
            s=serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert!(strategy::evaluate(&mut s,&bidir_books(now,spread,reverse),&a,now).unwrap().is_none());
            let t=now+1000;for x in &mut a{x.observed_ms=t;}
            let op=strategy::evaluate(&mut s,&bidir_books(t,spread,reverse),&a,t).unwrap();
            assert_eq!(op.as_ref().map(|o|o.level),expected);
            if let Some(mut o)=op {
                assert_eq!(o.min_entry_spread,Some(d(if spread==12{12}else{10})));
                let qty=o.requested_units;o.first_filled=qty;o.hedge_filled=qty;
                o.first_terminal=true;o.hedge_terminal=true;
                o.first_value=d(if reverse{100}else{110})*Decimal::from(qty);
                o.hedge_value=d(if reverse{110}else{100})*Decimal::from(qty);
                for p in &mut s.positions {p.units+=p.units.signum()*qty;}
                s.pending=Some(o);s.finish_operation(t).unwrap();
                assert_eq!(s.anchor,Some(d(10)));
                assert_eq!(s.time_adds_used,if spread==12{0}else{1});
                assert_eq!(s.last_open_completed,Some((t,d(10))));
            }
        }
    }
}

#[test]
fn accumulation_full_capacity_and_round_reset() {
    let now=4_000_000;let(mut s,mut a)=accumulation_fixture(now,false);
    s.config.max_groups=1;s.armed=vec![false];
    for t in [now,now+1000] {
        for x in &mut a{x.observed_ms=t;}
        assert!(strategy::evaluate(&mut s,&books(t,12),&a,t).unwrap().is_none());
    }
    s.config.max_groups=20;s.armed=vec![true;20];s.lots[0].level=20;
    s.time_adds_used=2;s.paused=true;
    let mut close=None;
    for t in [now+2000,now+3000] {
        for x in &mut a{x.observed_ms=t;}
        close=strategy::evaluate(&mut s,&books(t,4),&a,t).unwrap();
    }
    let mut op=close.unwrap();op.first_filled=1000;op.hedge_filled=1000;
    op.first_terminal=true;op.hedge_terminal=true;s.pending=Some(op);
    for p in &mut s.positions {p.units=0;}
    s.finish_operation(now+3000).unwrap();
    assert!(s.lots.is_empty());assert_eq!(s.time_adds_used,0);assert!(s.last_open_completed.is_none());
    assert!(!s.first_armed);assert!(s.anchor.is_none());
}

#[test]
fn time_addition_ignores_rising_mean_but_keeps_slippage_and_prior_fill_floor() {
    for reverse in [false,true] {
        for (spread,slippage,expected) in [(10,0,Some(20)),(10,1,None),(9,0,None),(11,1,Some(20))] {
            let now=4_000_000;let(mut s,mut a)=accumulation_fixture(now,reverse);
            for (_,m) in &mut s.samples {*m=d(20)*Decimal::from(s.direction.sign());}
            seed_entry_quotes(&mut s, now, 20, reverse);
            s.config.execution_slippage_bps=d(slippage);
            // First/grid entries still require MA+1. Time additions only reuse the last fill.
            assert_eq!(s.required_entry_spread(d(20),1),d(21));
            assert_eq!(s.required_entry_spread(d(20),20),d(10));
            assert!(strategy::evaluate(&mut s,&bidir_books(now,spread,reverse),&a,now).unwrap().is_none());
            let t=now+1000;for x in &mut a{x.observed_ms=t;}
            let op=strategy::evaluate(&mut s,&bidir_books(t,spread,reverse),&a,t).unwrap();
            assert_eq!(op.as_ref().map(|o|o.level),expected);
            if let Some(op)=op {assert_eq!(op.min_entry_spread,Some(d(10)));}
        }
    }
}

#[tokio::test]
async fn time_addition_below_ma_executes_equal_legs_and_survives_reload() {
    use std::sync::{Arc,RwLock,atomic::AtomicU64};
    for reverse in [false,true] {
        for wait_ms in [1000,5000,15000] {
            let now=crate::domain::now_ms();let(mut s,mut a)=accumulation_fixture(now,reverse);
            s.config.entry_confirmation_ms=Some(wait_ms);
            for (_,m) in &mut s.samples {*m=d(20)*Decimal::from(s.direction.sign());}
            seed_entry_quotes(&mut s, now, 20, reverse);
            assert!(strategy::evaluate(&mut s,&bidir_books(now,10,reverse),&a,now).unwrap().is_none());
            for offset in (1000..wait_ms).step_by(1000) {
                let t=now+offset;for x in &mut a{x.observed_ms=t;}
                assert!(strategy::evaluate(&mut s,&bidir_books(t,10,reverse),&a,t).unwrap().is_none());
            }
            let t=now+wait_ms;for x in &mut a{x.observed_ms=t;}
            let b=bidir_books(t,10,reverse);
            s.pending=strategy::evaluate(&mut s,&b,&a,t).unwrap();
            let op=s.pending.as_ref().unwrap();assert_eq!(op.level,20);
            assert_eq!(op.min_entry_spread,Some(d(10)));let qty=op.requested_units;
            // A higher mean between confirmation and dispatch must not reject the time add.
            for (_,m) in &mut s.samples {*m=d(30)*Decimal::from(s.direction.sign());}
            seed_entry_quotes(&mut s, t, 30, reverse);
            let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(t));
            let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,
                Box::new(venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone())
                    .with_logical_clock(clock.clone()))).unwrap());
            let path=std::env::temp_dir().join(format!("time-add-ma-{}.sqlite",s.instance_id));
            let(mut db,_)=store::Store::open(&path,&s.config).unwrap();
            for _ in 0..8 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&b,t).await.unwrap();}
            assert!(s.pending.is_none());assert_eq!(s.lots.len(),2);assert_eq!(s.time_adds_used,1);
            assert_eq!(s.lots[1].level,20);assert_eq!(s.lots[1].entry_spread,d(10));
            assert_eq!(s.last_open_completed,Some((t,d(10))));assert_eq!(s.anchor,Some(d(10)));
            assert_eq!(s.positions[0].units,(1000+qty)*s.direction.sign());
            assert_eq!(s.positions[0].units,-s.positions[1].units);
            let (l,e)=tokio::join!(workers[0].account(),workers[1].account());
            s.assert_reconciled(&[l.unwrap(),e.unwrap()]).unwrap();
            drop(db);let(_,restored)=store::Store::open(&path,&s.config).unwrap();
            assert_eq!(restored.time_adds_used,1);assert_eq!(restored.last_open_completed,s.last_open_completed);
            assert_eq!(restored.required_entry_spread(d(30),21),d(10));
        }
    }
}

#[test]
fn accumulation_round_keeps_ma_gate_while_groups_keep_half_spread_target() {
    for reverse in [false,true] {
        let now=4_000_000;let (mut group,mut a)=group_fixture(now,reverse);
        group.config.shared_exit_conditions=true;group.config.decision_ms=Some(1000);
        group.config.entry_confirmation_ms=Some(5000);group.config.mean_window_ms=300000;
        group.config.accumulation=Some(AccumulationRules{entry_floor:d(5),interval_ms:900000,
            max_time_adds:2,contraction_ratio:Decimal::new(5,1)});
        for (_,m) in &mut group.samples {*m=d(5)*Decimal::from(group.direction.sign());}
        group.config.validate().unwrap();
        let mut round=group.clone();round.config.exit_policy=ExitPolicy::Round;
        round.config.validate().unwrap();
        // Aggregate profit is positive at 7, but above MA5m=5: round must wait.
        let b=bidir_books(now,7,reverse);
        assert!(!strategy::normal_exit_eligible(&round,&b,now).unwrap());
        assert!(strategy::normal_exit_eligible(&round,&bidir_books(now,4,reverse),now).unwrap());
        assert!(!strategy::group_exit_eligible(&group,&group.lots[0],&b,1000,now).unwrap());
        assert!(strategy::group_exit_eligible(&group,&group.lots[1],&b,1000,now).unwrap());
        for t in [now,now+1000] {
            for x in &mut a{x.observed_ms=t;}
            let op=strategy::evaluate(&mut round,&bidir_books(t,4,reverse),&a,t).unwrap();
            if t==now {assert!(op.is_none());}
            else {let op=op.unwrap();assert_eq!(op.action,Action::Close);assert!(op.close_lot_id.is_none());}
        }
        // Even an already triggered round must recheck eligibility after a bounce.
        let t=now+2000;for x in &mut a{x.observed_ms=t;}
        assert!(round.exit_batch_active);
        assert!(strategy::evaluate(&mut round,&bidir_books(t,7,reverse),&a,t).unwrap().is_none());
    }
}

#[tokio::test]
async fn offline_memory_replay_matches_durable_paper_execution() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    let now=crate::domain::now_ms();
    let mut config=InventoryConfig::default();config.exit_policy=ExitPolicy::PerGroup;
    config.direction_policy=DirectionPolicy::Both;config.shared_exit_conditions=true;
    config.decision_ms=Some(1000);config.entry_confirmation_ms=Some(5000);config.mean_window_ms=60000;
    config.entry_offset=Decimal::ONE;config.max_loss_usdc=d(30);
    let seed=Snapshot::new(config.clone()).unwrap();
    let folder=std::env::temp_dir().join(format!("offline-parity-{}",seed.instance_id));std::fs::create_dir(&folder).unwrap();
    let shared=Arc::new(RwLock::new(books(now,10)));let clock=Arc::new(AtomicU64::new(now));
    let marks=Arc::new(RwLock::new([liquidation::Mark::default(),liquidation::Mark::default()]));
    let spec=liquidation::IsolationSpec{maintenance_rates:[Decimal::new(12,2),Decimal::ONE/d(12)],
        liquidation_fee_rates:[Decimal::new(1,2),Decimal::new(9,5)],mark_max_age_ms:5000};
    let mut memory=comparison::Variant::offline_replay(config.clone(),now,shared.clone(),clock.clone(),(spec.clone(),marks.clone())).unwrap();
    let mut disk=comparison::Variant::open_protected(&folder.join("disk.sqlite"),config,&seed,now,shared.clone(),clock.clone(),Some((spec,marks.clone()))).unwrap();
    for offset in (0..100000).step_by(250) {
        let t=now+offset;let b=books(t,if offset<65000 {10}else if offset<80000 {16}else{9});
        *marks.write().unwrap()=b.clone().map(|b|liquidation::Mark{price:b.mid(),received_ms:t,connected:true});
        *shared.write().unwrap()=b.clone();clock.store(t,Ordering::SeqCst);
        let (x,y)=tokio::join!(memory.tick(&b,t),disk.tick(&b,t));x.unwrap();y.unwrap();
        assert_eq!(serde_json::to_value(&memory.state.positions).unwrap(),serde_json::to_value(&disk.state.positions).unwrap());
        assert_eq!(memory.state.opened_groups,disk.state.opened_groups);
        assert_eq!(memory.state.closed_groups,disk.state.closed_groups);
        assert_eq!(memory.warning,disk.warning);
    }
    assert!(memory.state.opened_groups>0 && memory.state.closed_groups>0);
    let export=folder.join("export.sqlite");memory.store.export_replay(&export).unwrap();
    assert!(memory.store.export_replay(&export).is_err());
    let c=rusqlite::Connection::open(export).unwrap();
    let body:String=c.query_row("SELECT body FROM state WHERE id=1",[],|r|r.get(0)).unwrap();
    let restored:Snapshot=serde_json::from_str(&body).unwrap();assert_eq!(restored.fills.len(),memory.state.fills.len());
    drop(c);drop(memory);drop(disk);std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn replay_worker_checks_historical_deadlines_without_using_wall_time() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    let now=4_000_000;let clock=Arc::new(AtomicU64::new(now));
    let shared=Arc::new(RwLock::new(books(now,10)));
    let config=InventoryConfig::default();
    let backend=venue::PaperBackend::new(Venue::Lighter,config.clone(),Position::default(),shared.clone());
    let worker=venue::AccountWorker::spawn_replay(Venue::Lighter,backend,clock.clone()).unwrap();
    let request=OrderRequest{id:"historical-valid".into(),venue:Venue::Lighter,side:Side::Buy,units:1000,
        limit:d(101),arrival_mid:Some(d(100)),reduce_only:false,created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
    let fill=worker.submit(request.clone()).await.unwrap();assert_eq!(fill.fills.len(),1);
    assert_eq!(fill.fills[0].time_ms,now);
    clock.store(now+5001,Ordering::SeqCst);
    let mut expired=request;expired.id="historical-expired".into();
    assert!(worker.submit(expired).await.unwrap_err().to_string().contains("final request check rejected"));
    let mut live=config;live.mode=Mode::Live;
    let backend=venue::PaperBackend::new(Venue::Lighter,live,Position::default(),shared);
    assert!(venue::AccountWorker::spawn_replay(Venue::Lighter,backend,clock).is_err());
}
