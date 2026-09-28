#[test]
fn one_second_decisions_open_both_directions_without_resampling_mean() {
    for reverse in [false, true] {
        let now=4_005_000;
        let mut s=warmed(now);
        s.config.direction_policy=DirectionPolicy::Both;
        s.config.decision_ms=Some(1000);
        if reverse { for (_,mean) in &mut s.samples { *mean = -*mean; } }
        assert!(strategy::evaluate(&mut s,&bidir_books(now,16,reverse),&accounts(now),now).unwrap().is_none());
        let mut samples=s.samples.clone();let sampled=s.last_sample_ms;
        samples.retain(|(t,_)|*t>=now+1000-s.config.mean_window_ms);
        let t=now+999;
        assert!(strategy::evaluate(&mut s,&bidir_books(t,16,reverse),&accounts(t),t).unwrap().is_none());
        let t=now+1000;
        let op=strategy::evaluate(&mut s,&bidir_books(t,16,reverse),&accounts(t),t).unwrap().unwrap();
        assert_eq!(op.action,Action::Open);
        assert_eq!(s.direction,if reverse {Direction::LighterShort}else{Direction::LighterLong});
        assert_eq!(s.samples,samples);assert_eq!(s.last_sample_ms,sampled);
        assert_eq!(s.config.sample_ms,15000);
    }
}

#[test]
fn one_second_profit_exit_requires_two_new_books_for_both_policies_and_directions() {
    for reverse in [false,true] { for policy in [ExitPolicy::Round,ExitPolicy::PerGroup] {
        let now=4_005_000;let(mut s,mut a)=group_fixture(now,reverse);
        s.config.exit_policy=policy;s.config.shared_exit_conditions=true;s.config.decision_ms=Some(1000);
        let b=bidir_books(now,12,reverse);
        assert!(strategy::evaluate(&mut s,&b,&a,now).unwrap().is_none());
        let mut samples=s.samples.clone();
        samples.retain(|(t,_)|*t>=now+1000-s.config.mean_window_ms);
        let t=now+1000;for x in &mut a{x.observed_ms=t;}
        // Re-reading a fresh cached book is not a second confirmation.
        assert!(strategy::evaluate(&mut s,&b,&a,t).unwrap().is_none());
        let mut one_new=b.clone();one_new[0].received_ms=t;
        assert!(strategy::evaluate(&mut s,&one_new,&a,t).unwrap().is_none());
        one_new[1].received_ms=t;
        let op=strategy::evaluate(&mut s,&one_new,&a,t).unwrap().unwrap();
        assert_eq!(op.action,Action::Close);
        assert_eq!(op.close_lot_id.as_deref(),if policy==ExitPolicy::PerGroup{Some("group-1")}else{None});
        assert_eq!(s.samples,samples);
    }}
}

#[test]
fn one_second_open_does_not_confirm_repeated_stale_gapped_or_failed_signals() {
    let now=4_005_000;
    for failure in ["repeated","one_new","stale","gap","below_threshold"] {
        let mut s=warmed(now);s.config.decision_ms=Some(1000);
        assert!(strategy::evaluate(&mut s,&books(now,16),&accounts(now),now).unwrap().is_none());
        let t=now+1000;let mut b=books(t,16);
        match failure {
            "repeated"=>for x in &mut b{x.received_ms=now;},
            "one_new"=>b[1].received_ms=now,
            "stale"=>b[1].connected=false,
            "below_threshold"=>b=books(t,8),
            _=>{}
        }
        let t=if failure=="gap" {now+31000}else{t};
        if failure=="gap" {b=books(t,16);}
        let result=strategy::evaluate(&mut s,&b,&accounts(t),t);
        if failure=="stale" {assert!(result.is_err());}else{assert!(result.unwrap().is_none());}
        if failure=="stale" || failure=="below_threshold" {
            let t=t+1000;
            assert!(strategy::evaluate(&mut s,&books(t,16),&accounts(t),t).unwrap().is_none());
            assert!(strategy::evaluate(&mut s,&books(t+1000,16),&accounts(t+1000),t+1000).unwrap().is_some());
        }
    }
}

#[test]
fn slow_venue_updates_keep_original_confirmation_lifetime_without_trading_on_stale_books() {
    for reverse in [false,true] {
        let now=4_005_000;let mut s=warmed(now);s.config.decision_ms=Some(1000);
        s.config.direction_policy=DirectionPolicy::Both;
        if reverse {for (_,m) in &mut s.samples{*m = -*m;}}
        let b=bidir_books(now,16,reverse);
        assert!(strategy::evaluate(&mut s,&b,&accounts(now),now).unwrap().is_none());
        // No new Entropy update for five seconds: cached quotes must not trade.
        for offset in [1000,2000,4000] {
            let result=strategy::evaluate(&mut s,&b,&accounts(now+offset),now+offset);
            if offset<=1500 {assert!(result.unwrap().is_none());}else{assert!(result.is_err());}
        }
        let next=now+5400;
        let op=strategy::evaluate(&mut s,&bidir_books(next,16,reverse),&accounts(next),next).unwrap().unwrap();
        assert_eq!(op.action,Action::Open);assert_eq!(s.config.confirmation_window_ms(),30000);
    }
    for policy in [ExitPolicy::Round,ExitPolicy::PerGroup] {
        let now=4_005_000;let(mut s,mut a)=group_fixture(now,false);
        s.config.decision_ms=Some(1000);s.config.exit_policy=policy;s.config.shared_exit_conditions=true;
        let b=books(now,12);assert!(strategy::evaluate(&mut s,&b,&a,now).unwrap().is_none());
        for x in &mut a{x.observed_ms=now+2000;}
        assert!(strategy::evaluate(&mut s,&b,&a,now+2000).is_err());
        for x in &mut a{x.observed_ms=now+5400;}
        assert_eq!(strategy::evaluate(&mut s,&books(now+5400,12),&a,now+5400).unwrap().unwrap().action,Action::Close);
    }
}

#[test]
fn one_second_checks_keep_fifteen_second_mean_history_and_emergency_bypasses_clock() {
    let now=4_005_000;let mut s=warmed(now);s.config.decision_ms=Some(1000);s.paused=true;
    for offset in (0..=30000).step_by(250) {
        let t=now+offset;
        assert!(strategy::evaluate(&mut s,&books(t,10),&accounts(t),t).unwrap().is_none());
    }
    let added:Vec<_>=s.samples.iter().filter(|(t,_)|*t>=now).map(|(t,_)|*t).collect();
    assert_eq!(added,vec![now,now+15000,now+30000]);
    let(mut s,a)=group_fixture(now,false);s.config.decision_ms=Some(1000);
    s.decision_observation=Some((now,[now,now]));s.close_requested=true;
    let op=strategy::evaluate(&mut s,&books(now,25),&a,now).unwrap().unwrap();
    assert_eq!(op.action,Action::Close);
}

#[test]
fn cadence_is_paper_only_backward_compatible_and_restart_requires_new_confirmations() {
    let legacy=InventoryConfig::default();assert_eq!(legacy.decision_interval_ms(),15000);
    let value=serde_json::to_value(&legacy).unwrap();assert!(value.get("decision_ms").is_none());
    assert_eq!(serde_json::from_value::<InventoryConfig>(value).unwrap(),legacy);
    for interval in [0,250,999,60001] {let mut c=legacy.clone();c.decision_ms=Some(interval);assert!(c.validate().is_err());}
    let mut config=legacy;config.decision_ms=Some(1000);config.validate().unwrap();
    config.mode=Mode::Live;assert!(config.validate().is_err());config.mode=Mode::Paper;
    let now=4_005_000;let mut s=warmed(now);s.config=config.clone();
    assert!(strategy::evaluate(&mut s,&books(now,16),&accounts(now),now).unwrap().is_none());
    let path=std::env::temp_dir().join(format!("decision-restart-{}.sqlite",s.instance_id));
    let(mut store,_)=store::Store::open(&path,&config).unwrap();store.commit(&s,now,"test").unwrap();drop(store);
    let(store,mut restored)=store::Store::open(&path,&config).unwrap();
    assert_eq!(restored.samples,s.samples);
    assert_eq!(serde_json::to_value(&restored.positions).unwrap(),serde_json::to_value(&s.positions).unwrap());
    assert!(restored.decision_observation.is_none() && restored.previous_signal.is_none());
    restored.status=Status::Running;
    let t=now+1000;assert!(strategy::evaluate(&mut restored,&books(t,16),&accounts(t),t).unwrap().is_none());
    assert!(strategy::evaluate(&mut restored,&books(t+1000,16),&accounts(t+1000),t+1000).unwrap().is_some());
    drop(store);let _=std::fs::remove_file(&path);let _=std::fs::remove_file(path.with_extension("lock"));
}

#[tokio::test]
async fn one_second_comparison_executes_paired_entry_and_profit_exit_with_original_costs() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    for reverse in [false,true] {for policy in [ExitPolicy::Round,ExitPolicy::PerGroup] {
        // Historical time exposes any accidental wall-clock expiry check in
        // the offline fixture, independently of machine speed or test load.
        let now=4_005_000;let mut seed=warmed(now);
        seed.config.decision_ms=Some(1000);seed.config.direction_policy=DirectionPolicy::Both;
        seed.config.exit_policy=policy;seed.config.shared_exit_conditions=true;
        if reverse {for (_,m) in &mut seed.samples{*m = -*m;}}
        let folder=std::env::temp_dir().join(format!("decision-e2e-{}",seed.instance_id));
        std::fs::create_dir_all(&folder).unwrap();
        let b=bidir_books(now,16,reverse);
        let shared=Arc::new(RwLock::new(b));let clock=Arc::new(AtomicU64::new(now));
        let mut variant=comparison::Variant::open(&folder.join("state.sqlite"),seed.config.clone(),&seed,now,shared.clone(),clock.clone()).unwrap();
        for offset in (0..=10000).step_by(250) {
            let t=now+offset;let b=bidir_books(t,if offset<5000{16}else{8},reverse);
            *shared.write().unwrap()=b.clone();clock.store(t,Ordering::SeqCst);
            variant.tick(&b,t).await.unwrap();
            assert!(variant.warning.is_empty(),"frame {offset}: {}",variant.warning);
            if offset==4000 {
                assert_eq!(variant.state.opened_groups,1);assert_eq!(variant.state.lots.len(),1);
                assert!(variant.state.pending.is_none());
                assert_eq!(variant.state.positions[0].units,-variant.state.positions[1].units);
            }
        }
        assert_eq!(variant.state.opened_groups,1);assert_eq!(variant.state.closed_groups,1);
        assert!(variant.state.pending.is_none() && variant.state.lots.is_empty());
        assert!(variant.state.positions.iter().all(|p|p.units==0));
        assert!(variant.state.cumulative_fees()>Decimal::ZERO);
        assert!(variant.state.total_pnl(&bidir_books(now+10000,8,reverse)).unwrap()>Decimal::ZERO);
        assert!(variant.state.fills.values().filter(|f|variant.state.fill_opening[&format!("{:?}:{}",f.venue,f.id)])
            .all(|f| f.time_ms<now+5000));
        drop(variant);let _=std::fs::remove_dir_all(folder);
    }}
}
