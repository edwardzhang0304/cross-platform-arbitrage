#[test]
fn improving_spread_does_not_restart_ready_time_addition() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, mut a) = accumulation_fixture(now, reverse);
        s.config.entry_confirmation_ms = Some(5000);
        for offset in 0..=5 {
            let t = now + offset * 1000;
            for x in &mut a { x.observed_ms = t; }
            // Same shape as the reported spike: time-add first, then grid eligible.
            let spread = if offset < 4 { 11 } else { 12 };
            let op = strategy::evaluate(&mut s, &bidir_books(t, spread, reverse), &a, t).unwrap();
            if offset < 5 {
                assert!(op.is_none());
                assert_eq!(s.time_entry_confirmations[usize::from(reverse)], Some((now, 20)));
            } else {
                let op = op.unwrap();
                assert_eq!(op.level, 20);
                assert_eq!(op.min_entry_spread, Some(d(10)));
                assert_eq!(s.entry_confirmations, [None; 2]);
                assert_eq!(s.time_entry_confirmations, [None; 2]);
            }
        }
    }
}

#[test]
fn simultaneous_confirmations_prefer_one_grid_and_grid_dip_keeps_time_clock() {
    for reverse in [false, true] {
        for dip in [false, true] {
            let now = 4_000_000;
            let (mut s, mut a) = accumulation_fixture(now, reverse);
            s.config.entry_confirmation_ms = Some(5000);
            for offset in 0..=5 {
                let t = now + offset * 1000;
                for x in &mut a { x.observed_ms = t; }
                let spread = if dip && offset == 3 { 11 } else { 12 };
                let op = strategy::evaluate(&mut s, &bidir_books(t, spread, reverse), &a, t).unwrap();
                if offset < 5 { assert!(op.is_none()); }
                else {
                    assert_eq!(op.unwrap().level, if dip { 20 } else { 1 });
                    assert_eq!(s.sequence, 1);
                }
            }
        }
    }
}

#[test]
fn independent_confirmations_keep_freshness_depth_risk_and_spread_resets() {
    for reverse in [false, true] {
        for bad in ["stale", "disconnected", "depth", "funds", "spread"] {
            let now = 4_000_000;
            let (mut s, mut a) = accumulation_fixture(now, reverse);
            s.config.entry_confirmation_ms = Some(5000);
            assert!(strategy::evaluate(&mut s, &bidir_books(now, 12, reverse), &a, now).unwrap().is_none());
            let mut b = bidir_books(now + 4000, 12, reverse);
            for x in &mut a { x.observed_ms = now + 4000; }
            match bad {
                "stale" => b[1].received_ms = now,
                "disconnected" => b[1].connected = false,
                "depth" => { b[1].asks[0].units = 10; b[1].bids[0].units = 10; }
                "funds" => a[0].free_margin = Decimal::ZERO,
                _ => b = bidir_books(now + 4000, 9, reverse),
            }
            let result = strategy::evaluate(&mut s, &b, &a, now + 4000);
            assert!(result.is_err() || result.unwrap().is_none());
            assert_eq!(s.entry_confirmations, [None; 2], "{bad}");
            assert_eq!(s.time_entry_confirmations, [None; 2], "{bad}");
            for offset in [5000, 10000] {
                let t = now + offset;
                a = accounts(t);
                for i in 0..2 { a[i].position_units = s.positions[i].units; }
                let op = strategy::evaluate(&mut s, &bidir_books(t, 12, reverse), &a, t).unwrap();
                assert_eq!(op.is_some(), offset == 10000, "{bad}");
            }
        }
    }
}

#[test]
fn fifth_time_add_is_allowed_sixth_is_blocked_and_total_group_cap_remains() {
    for reverse in [false, true] {
        for (used, full, expected) in [(4, false, Some(24)), (5, false, None), (4, true, None)] {
            let now = 4_000_000;
            let (mut s, mut a) = accumulation_fixture(now, reverse);
            s.config.accumulation.as_mut().unwrap().max_time_adds = 5;
            s.time_adds_used = used;
            s.config.validate().unwrap();
            if full { s.lots.resize(20, s.lots[0].clone()); }
            assert!(strategy::evaluate(&mut s, &bidir_books(now, 11, reverse), &a, now).unwrap().is_none());
            for x in &mut a { x.observed_ms = now + 1000; }
            let op = strategy::evaluate(&mut s, &bidir_books(now + 1000, 11, reverse), &a, now + 1000).unwrap();
            assert_eq!(op.map(|o| o.level), expected);
        }
    }
}

#[test]
fn one_hour_time_add_gate_and_five_unit_grid_are_independent() {
    for reverse in [false, true] {
        // Five-second confirmation must not borrow time from before the one-hour gate.
        for (elapsed, spread, expected) in [
            (900_000, 11, None), (3_594_000, 11, None),
            (3_599_000, 11, None), (3_600_000, 11, Some(20)),
            (3_600_000, 9, None), (1000, 14, None), (1000, 15, Some(1)),
        ] {
            let now = 4_000_000;
            let (mut s, mut a) = accumulation_fixture(now, reverse);
            s.config.grid = d(5);
            s.config.entry_confirmation_ms = Some(5000);
            let r = s.config.accumulation.as_mut().unwrap();
            r.interval_ms = 3_600_000; r.max_time_adds = 5;
            r.quota_scope = TimeAddQuotaScope::GridStage;
            s.last_open_completed = Some((now - elapsed, d(10)));
            assert!(strategy::evaluate(&mut s, &bidir_books(now, spread, reverse), &a, now).unwrap().is_none());
            for x in &mut a { x.observed_ms = now + 5000; }
            let op = strategy::evaluate(&mut s, &bidir_books(now + 5000, spread, reverse), &a, now + 5000).unwrap();
            assert_eq!(op.map(|o| o.level), expected, "elapsed={elapsed}, spread={spread}");
        }
    }
}

#[test]
fn grid_stage_quota_resets_only_after_a_paired_fill_and_survives_restart() {
    for reverse in [false, true] {
        for scope in [TimeAddQuotaScope::Round, TimeAddQuotaScope::GridStage] {
            for outcome in ["filled", "partial_paired", "unresolved", "residual", "zero"] {
                let now = 4_000_000;
                let (mut s, mut a) = accumulation_fixture(now, reverse);
                s.config.grid = d(5);
                let r = s.config.accumulation.as_mut().unwrap();
                r.interval_ms = 3_600_000; r.max_time_adds = 5; r.quota_scope = scope;
                s.time_adds_used = 5;
                let previous_open = s.last_open_completed;
                assert!(strategy::evaluate(&mut s, &bidir_books(now, 15, reverse), &a, now).unwrap().is_none());
                for x in &mut a { x.observed_ms = now + 1000; }
                let mut op = strategy::evaluate(&mut s, &bidir_books(now + 1000, 15, reverse), &a, now + 1000).unwrap().unwrap();
                assert_eq!(op.level, 1);
                assert_eq!(s.time_adds_used, 5, "reservation must not reset quota");
                let qty = match outcome {
                    "zero" => 0,
                    "partial_paired" => s.config.common_step(),
                    _ => op.requested_units,
                };
                op.first_filled = qty; op.hedge_filled = qty;
                op.first_terminal = outcome != "unresolved"; op.hedge_terminal = true;
                let b = bidir_books(now, 15, reverse);
                op.first_value = b[op.first_venue.index()].mid().unwrap() * Decimal::from(qty);
                op.hedge_value = b[op.hedge_venue().index()].mid().unwrap() * Decimal::from(qty);
                for p in &mut s.positions { p.units += p.units.signum() * qty; }
                if outcome == "residual" { s.positions[0].units += s.config.common_step(); }
                s.pending = Some(op);
                let finished = s.finish_operation(now + 1000);
                if matches!(outcome, "unresolved" | "residual") {
                    assert!(finished.is_err());
                    assert_eq!(s.time_adds_used, 5);
                    assert_eq!(s.last_open_completed, previous_open);
                    continue;
                }
                finished.unwrap();
                let reset = scope == TimeAddQuotaScope::GridStage && qty > 0;
                assert_eq!(s.time_adds_used, if reset { 0 } else { 5 });
                assert_eq!(s.anchor, Some(d(10)));
                assert_eq!(s.last_open_completed, if qty > 0 {Some((now + 1000, d(15)))} else {previous_open});
                let restored: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
                assert_eq!(restored.time_adds_used, s.time_adds_used);
                assert_eq!(restored.last_open_completed, s.last_open_completed);
                assert_eq!(restored.config, s.config);
            }
        }
    }
}

#[test]
fn each_grid_stage_allows_five_time_adds_without_exceeding_total_capacity() {
    for reverse in [false, true] {
        for stage in 0..=2 {
            for (used, full, expected) in [(4, false, Some(24)), (5, false, None), (4, true, None)] {
                let now = 4_000_000;
                let (mut s, mut a) = accumulation_fixture(now, reverse);
                s.config.grid = d(5);
                s.config.max_loss_usdc = d(1000);
                let r = s.config.accumulation.as_mut().unwrap();
                r.interval_ms = 3_600_000; r.max_time_adds = 5; r.quota_scope = TimeAddQuotaScope::GridStage;
                let spread = 10 + 5 * stage as i64;
                s.time_adds_used = used;
                s.last_open_completed = Some((now - 3_600_000, d(spread)));
                s.armed[..=stage].fill(false);
                s.lots[0].level = stage;
                // Earlier-stage time-add lots can share a numeric level; their IDs
                // remain unique and they must not consume the new stage's quota.
                let mut old = s.lots[0].clone(); old.id = "previous-stage-time-add".into(); old.level = 24;
                s.lots.push(old);
                if full { s.lots.resize(20, s.lots[0].clone()); }
                let held = s.paired_units();
                for i in 0..2 {s.positions[i].units = s.positions[i].units.signum() * held; a[i].position_units = s.positions[i].units;}
                assert!(strategy::evaluate(&mut s, &bidir_books(now, spread + 1, reverse), &a, now).unwrap().is_none());
                for x in &mut a { x.observed_ms = now + 1000; }
                let op = strategy::evaluate(&mut s, &bidir_books(now + 1000, spread + 1, reverse), &a, now + 1000).unwrap();
                assert_eq!(op.map(|o| o.level), expected, "stage={stage}, used={used}, full={full}");
            }
        }
    }
}
