fn capped_fixture(now: u64, reverse: bool) -> (Snapshot, [AccountEvidence; 2]) {
    let (mut s, a) = accumulation_fixture(now, reverse);
    s.config.grid = d(5);
    s.config.grid_spacing = GridSpacing::CappedDoubling;
    let r = s.config.accumulation.as_mut().unwrap();
    r.interval_ms = CURRENT_TIME_ADD_INTERVAL_MS;
    r.quota_scope = TimeAddQuotaScope::GridStage;
    r.max_time_adds = 5;
    s.last_open_completed = Some((now - CURRENT_TIME_ADD_INTERVAL_MS, d(10)));
    s.entry_high_water = Some(d(10));
    (s, a)
}

#[test]
fn capped_spacing_thresholds_and_four_hour_boundary() {
    for reverse in [false, true] {
        let now = 20_000_000;
        let (s, _) = capped_fixture(now, reverse);
        for (level, want) in [10,15,25,45,65,85,105,125].into_iter().enumerate() {
            assert_eq!(s.required_entry_spread(d(5),level),d(want));
        }
        for (elapsed, expected) in [(3_600_000,false),(14_398_000,false),(14_400_000,true)] {
            let (mut s, mut a) = capped_fixture(now, reverse);
            s.last_open_completed = Some((now-elapsed,d(10)));
            for offset in [0,1000] {
                let t=now+offset;for x in &mut a {x.observed_ms=t;}
                let op=strategy::evaluate(&mut s,&bidir_books(t,11,reverse),&a,t).unwrap();
                assert_eq!(op.as_ref().map(|o|o.level), (expected && offset>0).then_some(20));
            }
        }
    }
}

#[test]
fn falling_spread_and_vacant_old_slots_never_bypass_high_water_or_time_quota() {
    for reverse in [false,true] {for used in [4,5] {
        let now=20_000_000;
        let (mut s,mut a)=capped_fixture(now,reverse);
        // A higher-entry group has already closed; its floor survives with the lower group.
        s.entry_high_water=Some(d(30));s.last_open_completed=Some((now-14_400_000,d(30)));
        s.time_adds_used=used;s.armed.fill(true);
        let restored:Snapshot=serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(restored.held_entry_floor(),Some(d(30)));
        assert_eq!(restored.next_grid_level(),Some(3)); // next price is 45, not 15 or 25
        for spread in [20,29,30] {
            let mut s=restored.clone();
            for offset in [0,1000] {
                let t=now+offset;for x in &mut a{x.observed_ms=t;}
                let op=strategy::evaluate(&mut s,&bidir_books(t,spread,reverse),&a,t).unwrap();
                let expected=(spread==30 && used<5 && offset>0).then_some(24);
                assert_eq!(op.map(|o|o.level),expected,"{reverse}/{used}/{spread}");
            }
        }
    }}
}

#[test]
fn high_water_is_kept_on_partial_close_and_cleared_only_when_flat() {
    for reverse in [false,true] {
        let now=20_000_000;
        let (mut s,mut a)=capped_fixture(now,reverse);
        s.entry_high_water=Some(d(30));s.close_requested=true;
        let op=strategy::evaluate(&mut s,&bidir_books(now,10,reverse),&a,now).unwrap().unwrap();
        let mut partial=op.clone();partial.first_filled=500;partial.hedge_filled=500;
        partial.first_terminal=true;partial.hedge_terminal=true;
        for p in &mut s.positions {p.units=p.units.signum()*500;}
        s.pending=Some(partial);s.finish_operation(now).unwrap();
        assert_eq!(s.held_entry_floor(),Some(d(30)));
        s.close_requested=true;
        for i in 0..2 {a[i].observed_ms=now+1000;a[i].position_units=s.positions[i].units;}
        let mut end=strategy::evaluate(&mut s,&bidir_books(now+1000,10,reverse),&a,now+1000).unwrap().unwrap();
        end.first_filled=500;end.hedge_filled=500;end.first_terminal=true;end.hedge_terminal=true;
        for p in &mut s.positions {p.units=0;}
        s.pending=Some(end);s.finish_operation(now+1000).unwrap();
        assert!(s.lots.is_empty());assert_eq!(s.entry_high_water,None);
        assert_eq!(s.last_open_completed,None);assert_eq!(s.anchor,None);
        assert_eq!(s.required_entry_spread(d(5),0),d(6));
    }
}

#[test]
fn only_paired_open_completion_changes_floor_clock_and_stage_quota() {
    for reverse in [false,true] {for succeeds in [false,true] {
        let now=20_000_000;
        let (mut s,mut a)=capped_fixture(now,reverse);s.time_adds_used=4;
        let previous=s.last_open_completed;
        assert!(strategy::evaluate(&mut s,&bidir_books(now,15,reverse),&a,now).unwrap().is_none());
        let t=now+1000;for x in &mut a {x.observed_ms=t;}
        let b=bidir_books(t,15,reverse);
        let mut op=strategy::evaluate(&mut s,&b,&a,t).unwrap().unwrap();
        assert_eq!(op.level,1);
        assert_eq!(s.time_adds_used,4);assert_eq!(s.entry_high_water,Some(d(10)));
        op.first_terminal=true;op.hedge_terminal=true;
        if succeeds {
            op.first_filled=op.requested_units;op.hedge_filled=op.requested_units;
            op.first_value=b[1].mid().unwrap()*Decimal::from(op.requested_units);
            op.hedge_value=b[0].mid().unwrap()*Decimal::from(op.requested_units);
            for p in &mut s.positions {p.units+=p.units.signum()*op.requested_units;}
        } else {op.failed=true;}
        s.pending=Some(op);s.finish_operation(t).unwrap();
        assert_eq!(s.time_adds_used,if succeeds {0}else{4});
        assert_eq!(s.entry_high_water,Some(d(if succeeds {15}else{10})));
        assert_eq!(s.last_open_completed,if succeeds {Some((t,d(15)))}else{previous});
        let recovered:Snapshot=serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(recovered.entry_high_water,s.entry_high_water);
    }}
}
