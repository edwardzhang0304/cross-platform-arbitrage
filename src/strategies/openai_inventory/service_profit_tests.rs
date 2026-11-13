use super::*;

fn fixture(market: MarketPair, scale: i64, reverse: bool) -> (Snapshot, [Book; 2], u64) {
    let now = 4_000_000;
    let mut config = InventoryConfig::default();
    config.market = market;
    config.set_initial_capital(scale as u32).unwrap();
    config.fee_lighter = Decimal::new(1, 3);
    config.fee_entropy = Decimal::new(2, 3);
    config.execution_slippage_bps = Decimal::ONE;
    let mut s = Snapshot::new(config).unwrap();
    let units = market.units(Decimal::new(scale, 1)).unwrap();
    let sign = if reverse { -1 } else { 1 };
    for (i, p) in s.positions.iter_mut().enumerate() {
        p.units = units * if i == 0 { sign } else { -sign };
        p.average = Decimal::from(if i == 0 { 100 } else { 130 });
        p.realized = Decimal::from(if i == 0 { 2 * scale } else { scale });
        p.fees = Decimal::new(if i == 0 { 2 * scale } else { 3 * scale }, 1);
        p.funding = Decimal::new(if i == 0 { -10 * scale } else { 15 * scale }, 2);
    }
    // Account lifetime PnL must not reset at the latest round or double-charge
    // execution costs already embedded in actual fill prices.
    s.round_start_pnl = Decimal::from(123);
    s.execution_cost = Decimal::from(99);
    let books = [110, 120].map(|bid| Book {
        bids: vec![Level { price: bid.into(), units }],
        asks: vec![Level { price: (bid + 1).into(), units }],
        received_ms: now, connected: true,
    });
    (s, books, now)
}

#[test]
fn profit_display_preserves_costs_for_both_markets_directions_and_capital_tiers() {
    for market in [MarketPair::Openai, MarketPair::Anth] {
        for scale in [1, 10] { for reverse in [false, true] {
            let (s, mut books, now) = fixture(market, scale, reverse);
            let before = serde_json::to_value(&s).unwrap();
            let q = Decimal::new(scale, 1);
            let slip = Decimal::new(1, 4);
            let (left, right, gross) = if reverse {
                let left = Decimal::from(111) * (Decimal::ONE + slip);
                let right = Decimal::from(120) * (Decimal::ONE - slip);
                (left, right, q * (Decimal::from(100) - left + right - Decimal::from(130)))
            } else {
                let left = Decimal::from(110) * (Decimal::ONE - slip);
                let right = Decimal::from(121) * (Decimal::ONE + slip);
                (left, right, q * (left - Decimal::from(100) + Decimal::from(130) - right))
            };
            let expected = gross - q * left * Decimal::new(1, 3)
                - q * right * Decimal::new(2, 3) + Decimal::new(255 * scale, 2);
            let value = s.risk_valuation(&books, now).unwrap();
            assert_eq!(display_net_pnl(&s, &books, Some(&value)), (Some(expected), false));
            assert_eq!(s.total_pnl(&books).unwrap(), expected);
            // The same prices with half the visible liquidity still provide a
            // display estimate, but the full exit remains non-executable.
            for b in &mut books { for l in b.bids.iter_mut().chain(&mut b.asks) { l.units /= 2; } }
            let value = s.risk_valuation(&books, now).unwrap();
            assert_eq!(display_net_pnl(&s, &books, Some(&value)), (Some(expected), true));
            assert!(s.total_pnl(&books).is_err());
            assert!(s.remaining_net(&books).is_err());
            assert_eq!(serde_json::to_value(&s).unwrap(), before);
        }}
    }
}

#[test]
fn profit_display_handles_the_same_book_for_small_and_large_accounts() {
    let (small, books, now) = fixture(MarketPair::Openai, 1, true);
    let (large, _, _) = fixture(MarketPair::Openai, 10, true);
    let a = small.risk_valuation(&books, now).unwrap();
    let b = large.risk_valuation(&books, now).unwrap();
    let small_display = display_net_pnl(&small, &books, Some(&a));
    let large_display = display_net_pnl(&large, &books, Some(&b));
    assert!(!small_display.1);
    assert!(large_display.1);
    assert_eq!(large_display.0.unwrap(), small_display.0.unwrap() * Decimal::TEN);
    assert!(large.total_pnl(&books).is_err());
}

#[test]
fn profit_display_does_not_reuse_stale_or_invalid_quotes_but_flat_pnl_needs_no_quote() {
    let (mut s, books, now) = fixture(MarketPair::Openai, 10, false);
    for case in ["stale", "future", "disconnected", "empty", "crossed", "invalid", "unsorted"] {
        let mut bad = books.clone();
        match case {
            "stale" => bad[0].received_ms = now - s.config.book_max_age_ms - 1,
            "future" => bad[0].received_ms = now + 1,
            "disconnected" => bad[0].connected = false,
            "empty" => bad[0].bids.clear(),
            "crossed" => bad[0].bids[0].price = Decimal::from(999),
            "invalid" => bad[0].asks[0].units = 0,
            _ => bad[0].bids.push(Level { price: Decimal::from(111), units: 10 }),
        }
        let value = s.risk_valuation(&bad, now).ok();
        assert!(value.is_none(), "{case}");
        assert_eq!(display_net_pnl(&s, &bad, value.as_ref()), (None, false), "{case}");
    }
    for p in &mut s.positions { p.units = 0; }
    assert_eq!(display_net_pnl(&s, &[Book::default(), Book::default()], None),
        (Some(Decimal::new(255, 1)), false));
}

#[test]
fn profit_display_keeps_unreconciled_incidents_unknown_and_allows_completed_exit_history() {
    let (mut s, books, now) = fixture(MarketPair::Openai, 10, true);
    let value = s.risk_valuation(&books, now).unwrap();
    s.live_orphan = Some(live_orphan::Incident { missing: Venue::Lighter, detected_ms: now,
        last_check_ms: now, attempts: vec![], completed_ms: None, warning: String::new() });
    assert_eq!(display_net_pnl(&s, &books, Some(&value)), (None, false));
    s.live_orphan = None;
    emergency_exit::latch(&mut s, now).unwrap();
    assert_eq!(display_net_pnl(&s, &books, Some(&value)), (None, false));
    s.emergency_exit.as_mut().unwrap().completed_ms = Some(now);
    assert_eq!(display_net_pnl(&s, &books, Some(&value)), (Some(value.estimated_total_pnl), false));
}
