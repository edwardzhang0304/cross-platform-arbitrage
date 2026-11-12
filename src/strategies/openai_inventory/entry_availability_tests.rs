fn availability_tick(s: &mut Snapshot, a: &mut [AccountEvidence; 2], now: u64, thin: bool) {
    for account in a.iter_mut() { account.observed_ms = now; }
    let mut b = bidir_books(now, 12, s.direction == Direction::LighterShort);
    if thin { for book in &mut b { for l in book.bids.iter_mut().chain(&mut book.asks) { l.units = 10; } } }
    entry_availability::update(s, &b, a, now);
}

#[test]
fn automatic_funding_pause_precedes_spread_signal_and_recovers_after_stable_checks() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, mut a) = accumulation_fixture(now, reverse);
        a[1].free_margin = Decimal::ZERO;
        // No eligible spread signal is required to expose insufficient funding.
        assert!(strategy::evaluate(&mut s, &bidir_books(now, 9, reverse), &a, now).unwrap().is_none());
        assert_eq!(s.status, Status::PausedEntries);
        assert!(!s.paused, "automatic state must not replace manual intent");
        assert_eq!(s.automatic_entry_pause.as_ref().unwrap().cause, entry_availability::Obstacle::Funding);
        let lots = serde_json::to_value(&s.lots).unwrap();
        a[1].free_margin = d(100);
        for i in 1..=10 { availability_tick(&mut s, &mut a, now+i*1000, false); assert!(entry_availability::active(&s)); }
        availability_tick(&mut s, &mut a, now+11_000, false);
        assert!(s.automatic_entry_pause.is_none());
        assert_eq!(s.status, Status::Running);
        assert_eq!(serde_json::to_value(&s.lots).unwrap(), lots);
        assert!(s.pending.is_none());
    }
}

#[test]
fn depth_pause_needs_continuous_failure_and_never_resumes_across_stale_evidence_or_restart() {
    let now = 4_000_000;
    let (mut s, mut a) = accumulation_fixture(now, false);
    for i in 0..5 { availability_tick(&mut s, &mut a, now+i*1000, true); assert!(!entry_availability::active(&s)); }
    availability_tick(&mut s, &mut a, now+5000, true);
    assert_eq!(s.status, Status::PausedEntries);
    assert_eq!(s.automatic_entry_pause.as_ref().unwrap().cause, entry_availability::Obstacle::Depth);
    for i in 6..=14 { availability_tick(&mut s, &mut a, now+i*1000, false); }
    let mut b = books(now+15_000, 12); b[1].connected = false;
    assert!(strategy::evaluate(&mut s, &b, &a, now+15_000).is_err());
    availability_tick(&mut s, &mut a, now+16_000, false);
    assert!(entry_availability::active(&s));
    s = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    availability_tick(&mut s, &mut a, now+30_000, false);
    assert!(entry_availability::active(&s));
    for i in 31..=40 { availability_tick(&mut s, &mut a, now+i*1000, false); }
    assert!(s.automatic_entry_pause.is_none());
}

#[test]
fn temporary_depth_failure_does_not_latch_and_manual_pause_stop_and_protection_never_auto_resume() {
    let now = 4_000_000;
    let (mut s, mut a) = accumulation_fixture(now, false);
    availability_tick(&mut s, &mut a, now, true);
    availability_tick(&mut s, &mut a, now+1000, false);
    assert!(s.automatic_entry_pause.is_none());
    for case in ["manual", "stop", "loss", "attention", "margin"] {
        let mut state = s.clone();
        a[1].free_margin = Decimal::ZERO;
        availability_tick(&mut state, &mut a, now+2000, false);
        assert!(entry_availability::active(&state));
        match case {
            "manual" => { service::pause_entries(&mut state); },
            "stop" => { state.stop_requested = true; state.status = Status::Stopped; },
            "loss" => { state.loss_stop = Some(LossStop {at_ms:now,net_pnl:d(-30)}); },
            "attention" => { state.status = Status::NeedsAttention; },
            _ => { state.entry_margin_block = Some(entry_margin::EntryMarginBlock {venue:Venue::Lighter,at_ms:now,order_id:"synthetic".into(),rejection:entry_margin::EntryRejection::InvalidBalance}); },
        }
        let status = state.status;
        a[1].free_margin = d(100);
        for i in 3..=20 { availability_tick(&mut state, &mut a, now+i*1000, false); }
        assert_eq!(state.status, status, "{case}");
        assert!(entry_availability::active(&state), "{case}");
    }
}

#[test]
fn automatic_entry_pause_keeps_profitable_group_exit_and_loss_protection_alive() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, mut a) = group_fixture(now, reverse);
        s.paused = false; a[1].free_margin = Decimal::ZERO;
        assert!(strategy::evaluate(&mut s, &bidir_books(now, 12, reverse), &a, now).unwrap().is_none());
        assert_eq!(s.status, Status::PausedEntries);
        let next = now+15000; for account in &mut a { account.observed_ms = next; }
        let op = strategy::evaluate(&mut s, &bidir_books(next, 12, reverse), &a, next).unwrap().unwrap();
        assert_eq!(op.action, Action::Close);
        assert_eq!(op.requested_units, 1000);
        assert!(entry_availability::active(&s));
        s.config.max_loss_usdc = Decimal::new(1, 2);
        let next = next+15000; for account in &mut a { account.observed_ms = next; }
        let op = strategy::evaluate(&mut s, &bidir_books(next, 50, reverse), &a, next).unwrap().unwrap();
        assert_eq!(op.action, Action::Close);
        assert!(s.loss_stop.is_some());
    }
}

#[test]
fn automatic_pause_requires_valid_funding_ownership_and_single_group_depth_to_recover() {
    let now = 4_000_000;
    let (mut s, mut a) = accumulation_fixture(now, false);
    a[1].free_margin = Decimal::ZERO;
    availability_tick(&mut s, &mut a, now, false);
    a[1].free_margin = d(100);
    for fault in ["identity", "orders", "positions", "auth", "leverage"] {
        let mut bad = a.clone();
        match fault {
            "identity" => bad[1].account = "different-account".into(),
            "orders" => bad[1].open_orders = 1,
            "positions" => bad[1].position_units += 10,
            "auth" => bad[1].authenticated = false,
            _ => bad[1].leverage = 2,
        }
        for i in 1..=15 { availability_tick(&mut s, &mut bad, now+i*1000, false); }
        assert!(entry_availability::active(&s), "{fault}");
    }
}

#[test]
fn hourly_time_add_waits_for_full_hour_and_confirmation_but_grid_is_independent() {
    for reverse in [false,true] {
        let now=8_000_000;
        let (mut s,mut a)=accumulation_fixture(now,reverse);
        s.config.grid=d(5);
        s.config.accumulation.as_mut().unwrap().interval_ms=CURRENT_TIME_ADD_INTERVAL_MS;
        s.config.accumulation.as_mut().unwrap().quota_scope=TimeAddQuotaScope::GridStage;
        s.last_open_completed=Some((now-3_599_000,d(10)));
        s.config.entry_confirmation_ms=Some(5000);
        for offset in 0..=6 {
            let t=now+offset*1000;for account in &mut a {account.observed_ms=t;}
            let result=strategy::evaluate(&mut s,&bidir_books(t,10,reverse),&a,t).unwrap();
            if offset<6 {assert!(result.is_none(),"hour and five-second confirmation must both pass");}
            else {assert_eq!(result.unwrap().level,s.config.max_groups);}
        }
        let (mut s,mut a)=accumulation_fixture(now,reverse);
        s.config.grid=d(5);s.config.accumulation.as_mut().unwrap().interval_ms=CURRENT_TIME_ADD_INTERVAL_MS;
        s.last_open_completed=Some((now-10_000,d(10)));
        for offset in 0..=1 {
            let t=now+offset*1000;for account in &mut a {account.observed_ms=t;}
            let result=strategy::evaluate(&mut s,&bidir_books(t,15,reverse),&a,t).unwrap();
            if offset==0 {assert!(result.is_none());} else {assert_eq!(result.unwrap().level,1);}
        }
    }
}
