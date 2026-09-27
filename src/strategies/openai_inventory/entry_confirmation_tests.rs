#[test]
fn entry_delays_use_elapsed_time_not_check_count_in_both_directions() {
    for delay in [1000,5000,15000] { for reverse in [false,true] {
        let now=4_005_000;let mut s=warmed(now);
        s.config.decision_ms=Some(1000);s.config.entry_confirmation_ms=Some(delay);
        s.config.direction_policy=DirectionPolicy::Both;
        if reverse {for (_,m) in &mut s.samples{*m = -*m;}}
        for offset in (0..delay).step_by(1000) {
            let t=now+offset;
            assert!(strategy::evaluate(&mut s,&bidir_books(t,16,reverse),&accounts(t),t).unwrap().is_none());
            assert_eq!(s.entry_confirmations[usize::from(reverse)].unwrap().0,now);
        }
        let t=now+delay;
        let op=strategy::evaluate(&mut s,&bidir_books(t,16,reverse),&accounts(t),t).unwrap().unwrap();
        assert_eq!(op.action,Action::Open);
        assert_eq!(s.direction,if reverse{Direction::LighterShort}else{Direction::LighterLong});
        assert_eq!(s.entry_confirmations,[None;2]);
    }}
}

#[test]
fn timed_entries_restart_after_failure_staleness_depth_or_direction_change() {
    for failure in ["spread","stale","depth","account","disconnect","direction","timeout"] {
        let now=4_005_000;let mut s=warmed(now);s.config.decision_ms=Some(1000);
        s.config.entry_confirmation_ms=Some(5000);s.config.direction_policy=DirectionPolicy::Both;
        assert!(strategy::evaluate(&mut s,&books(now,16),&accounts(now),now).unwrap().is_none());
        let mut b=books(now+2000,16);let mut a=accounts(now+2000);
        match failure {
            "spread"=>b=books(now+2000,8),
            "direction"=>b=books(now+2000,-16),
            "stale"=>b[1].received_ms=now,
            "disconnect"=>b[1].connected=false,
            "depth"=>for side in [&mut b[1].asks,&mut b[1].bids]{side[0].units=10;},
            "account"=>a[0].free_margin=Decimal::ZERO,
            _=>{}
        }
        if failure!="timeout" {let result=strategy::evaluate(&mut s,&b,&a,now+2000);assert!(result.is_err()||result.unwrap().is_none());}
        let t=now+if failure=="timeout"{31000}else{5000};
        assert!(strategy::evaluate(&mut s,&books(t,16),&accounts(t),t).unwrap().is_none(),"{failure}");
        assert!(strategy::evaluate(&mut s,&books(t+5000,16),&accounts(t+5000),t+5000).unwrap().is_some(),"{failure}");
    }
}

#[test]
fn timed_entry_does_not_slow_normal_per_group_exit_or_emergency_exit() {
    for delay in [1000,5000,15000] {
        let now=4_005_000;let(mut s,mut a)=group_fixture(now,false);
        s.config.shared_exit_conditions=true;s.config.decision_ms=Some(1000);s.config.entry_confirmation_ms=Some(delay);
        assert!(strategy::evaluate(&mut s,&books(now,12),&a,now).unwrap().is_none());
        for x in &mut a{x.observed_ms=now+1000;}
        assert_eq!(strategy::evaluate(&mut s,&books(now+1000,12),&a,now+1000).unwrap().unwrap().action,Action::Close);
        s.close_requested=true;s.decision_observation=Some((now+1000,[now+1000;2]));
        assert_eq!(strategy::evaluate(&mut s,&books(now+1000,25),&a,now+1000).unwrap().unwrap().action,Action::Close);
    }
}

#[test]
fn timed_entry_config_and_restart_fail_closed() {
    let mut c=InventoryConfig::default();c.decision_ms=Some(1000);c.entry_confirmation_ms=Some(5000);
    c.validate().unwrap();
    for delay in [0,999,15001,30000] {c.entry_confirmation_ms=Some(delay);assert!(c.validate().is_err());}
    c.entry_confirmation_ms=Some(5000);c.mode=Mode::Live;assert!(c.validate().is_err());c.mode=Mode::Paper;
    let now=4_005_000;let mut seed=warmed(now);seed.config=c.clone();
    assert!(strategy::evaluate(&mut seed,&books(now,16),&accounts(now),now).unwrap().is_none());
    let folder=std::env::temp_dir().join(format!("confirmation-restart-{}",seed.instance_id));
    let path=folder.join("state.sqlite");
    let(mut db,_)=store::Store::open(&path,&c).unwrap();db.commit(&seed,now,"test").unwrap();drop(db);
    let(db,mut s)=store::Store::open(&path,&c).unwrap();s.status=Status::Running;
    assert_eq!(s.entry_confirmations,[None;2]);
    assert!(strategy::evaluate(&mut s,&books(now+5000,16),&accounts(now+5000),now+5000).unwrap().is_none());
    assert!(strategy::evaluate(&mut s,&books(now+10000,16),&accounts(now+10000),now+10000).unwrap().is_some());
    drop(db);let _=std::fs::remove_dir_all(folder);
}

#[tokio::test]
async fn confirmation_experiment_shares_prices_with_independent_protected_equal_capital_ledgers() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    let now=crate::domain::now_ms();let seed=warmed(now);
    let folder=std::env::temp_dir().join(format!("three-confirmations-{}",seed.instance_id));
    let shared=Arc::new(RwLock::new(books(now,16)));let clock=Arc::new(AtomicU64::new(now));
    let marks=Arc::new(RwLock::new([liquidation::Mark::default(),liquidation::Mark::default()]));
    let spec=liquidation::IsolationSpec{maintenance_rates:[Decimal::new(12,2),Decimal::new(8,2)],
        liquidation_fee_rates:[Decimal::new(1,2),Decimal::new(9,5)],mark_max_age_ms:5000};
    let mut variants=Vec::new();
    for delay in [1000,5000,15000] {
        let mut c=seed.config.clone();c.decision_ms=Some(1000);c.entry_confirmation_ms=Some(delay);
        c.exit_policy=ExitPolicy::PerGroup;c.shared_exit_conditions=true;
        variants.push(comparison::Variant::open_protected(&folder.join(format!("{delay}.sqlite")),c,&seed,now,
            shared.clone(),clock.clone(),Some((spec.clone(),marks.clone()))).unwrap());
    }
    for offset in (0..=25000).step_by(250) {
        let t=now+offset;let b=books(t,if offset<18000{16}else{8});
        *shared.write().unwrap()=b.clone();clock.store(t,Ordering::SeqCst);
        *marks.write().unwrap()=b.each_ref().map(|book|liquidation::Mark{price:book.mid(),received_ms:t,connected:true});
        for v in &mut variants {v.tick(&b,t).await.unwrap();}
    }
    for v in &variants {
        assert_eq!(v.state.opened_groups,1,"{}",v.warning);assert_eq!(v.state.closed_groups,1,"{}",v.warning);
        assert!(v.state.pending.is_none() && v.state.positions.iter().all(|p|p.units==0));
        let first=v.state.fills.values().filter(|f|v.state.fill_opening[&format!("{:?}:{}",f.venue,f.id)])
            .map(|f|f.time_ms).min().unwrap();
        assert!(first>=now+v.state.config.entry_confirmation_ms.unwrap());
        assert!(first<now+v.state.config.entry_confirmation_ms.unwrap()+2000);
        assert_eq!(v.state.config.paper_capital_per_venue,Decimal::from(100));
        assert!(v.summary(&books(now+25000,8),now+25000)["liquidation_protection_enabled"]==true);
    }
    assert_eq!(variants[0].state.samples,variants[1].state.samples);
    assert_eq!(variants[1].state.samples,variants[2].state.samples);
    drop(variants);let _=std::fs::remove_dir_all(folder);
}

#[tokio::test]
async fn missing_isolated_marks_clear_entry_confirmation_without_waiting_for_an_order() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    let now=crate::domain::now_ms();let mut seed=warmed(now);
    seed.config.decision_ms=Some(1000);seed.config.entry_confirmation_ms=Some(5000);
    seed.config.exit_policy=ExitPolicy::PerGroup;seed.config.shared_exit_conditions=true;
    let folder=std::env::temp_dir().join(format!("confirmation-marks-{}",seed.instance_id));
    let shared=Arc::new(RwLock::new(books(now,16)));let clock=Arc::new(AtomicU64::new(now));
    let marks=Arc::new(RwLock::new([liquidation::Mark::default(),liquidation::Mark::default()]));
    let spec=liquidation::IsolationSpec{maintenance_rates:[Decimal::new(1,1);2],
        liquidation_fee_rates:[Decimal::new(1,2);2],mark_max_age_ms:5000};
    let mut v=comparison::Variant::open_protected(&folder.join("state.sqlite"),seed.config.clone(),&seed,now,
        shared.clone(),clock.clone(),Some((spec,marks.clone()))).unwrap();
    for offset in [0,1000,5000] {
        let t=now+offset;let b=books(t,16);*shared.write().unwrap()=b.clone();clock.store(t,Ordering::SeqCst);
        *marks.write().unwrap()=if offset==1000{Default::default()}else{
            b.each_ref().map(|book|liquidation::Mark{price:book.mid(),received_ms:t,connected:true})};
        v.tick(&b,t).await.unwrap();assert!(v.state.pending.is_none());assert!(!v.state.paused);
        if offset==1000 {assert_eq!(v.state.entry_confirmations,[None;2]);}
        if offset==5000 {assert_eq!(v.state.entry_confirmations[0].unwrap().0,t);}
    }
    drop(v);let _=std::fs::remove_dir_all(folder);
}
