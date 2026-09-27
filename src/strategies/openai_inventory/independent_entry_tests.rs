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
