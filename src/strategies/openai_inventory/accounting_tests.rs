use super::{accounting::*, *};
use rust_decimal::Decimal;
use std::str::FromStr;

fn d(x: &str) -> Decimal { Decimal::from_str(x).unwrap() }
fn snapshot() -> Snapshot {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.direction = Direction::LighterShort;
    s.config.execution_slippage_bps = Decimal::ZERO;
    s.config.fee_entropy = Decimal::ZERO;
    s.config.fee_lighter = Decimal::ZERO;
    s
}
fn books(now: u64) -> [Book; 2] {
    [105, 100].map(|p| Book { bids: vec![Level { price: Decimal::from(p), units: 100000 }],
        asks: vec![Level { price: Decimal::from(p), units: 100000 }], received_ms: now, connected: true })
}
fn fill(s: &mut Snapshot, op: &str, venue: Venue, side: Side, n: i64, t: u64, price: &str) {
    let f = Fill { id: format!("{op}-{venue:?}-{t}"), order_id: format!("{op}-v2-fill"), venue, side,
        units: n, price: d(price), fee: Decimal::ZERO, time_ms: t };
    s.record_fill(&f, None).unwrap();
}
fn open(s: &mut Snapshot, id: &str, n: i64, t: u64) {
    fill(s, id, Venue::Lighter, Side::Sell, n, t, "112");
    fill(s, id, Venue::Entropy, Side::Buy, n, t + 1, "100");
    s.lots.push(Lot { id: id.into(), level: s.lots.len(), units: n, opened_ms: t + 2,
        entry_spread: d("12"), entry_net_spread: Some(d("12")) });
}
fn close(s: &mut Snapshot, op: &str, allocations: &[(&str, i64)], t: u64) {
    let total = allocations.iter().map(|(_, n)| n).sum();
    fill(s, op, Venue::Lighter, Side::Buy, total, t, "105");
    fill(s, op, Venue::Entropy, Side::Sell, total, t + 1, "100");
    s.closed_lot_allocations.insert(op.into(), allocations.iter().map(|(id, n)|
        CloseAllocation { lot_id: (*id).into(), units: *n }).collect());
    for (id, n) in allocations {
        s.lots.iter_mut().find(|l| l.id == *id).unwrap().units -= n;
    }
    s.lots.retain(|l| l.units > 0);
}
fn funding(id: &str, venue: Venue, t: u64, amount: &str) -> Funding {
    Funding { id: id.into(), venue, time_ms: t, amount: d(amount) }
}
fn report(s: &Snapshot) -> NetProfitAccounting {
    AccountingCache::default().report(s, &books(10000), 10000)
}

#[test]
fn settlement_uses_historical_size_and_offsets_income_against_expense() {
    let mut s = snapshot();
    open(&mut s, "a", 100, 1000);
    open(&mut s, "b", 200, 2000);
    record_funding(&mut s, funding("early", Venue::Entropy, 1500, "-0.03")).unwrap();
    record_funding(&mut s, funding("later", Venue::Entropy, 2500, "-0.09")).unwrap();
    record_funding(&mut s, funding("income", Venue::Lighter, 2500, "0.012")).unwrap();
    let r = report(&s);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].settled_funding, Some(d("-0.056")));
    assert_eq!(r.lots[1].settled_funding, Some(d("-0.052")));
    assert_eq!(r.settled_funding, d("-0.108"));
    let total: Decimal = r.lots.iter().map(|l| l.estimated_exit_net.unwrap()).sum();
    assert_eq!(total, s.total_pnl(&books(10000)).unwrap());
    assert_eq!(r.unallocated_funding, Decimal::ZERO);
}

#[test]
fn late_payment_respects_partial_close_and_does_not_charge_closed_units_again() {
    let mut s = snapshot();
    open(&mut s, "a", 300, 1000);
    close(&mut s, "close-a", &[("a", 100)], 2000);
    record_funding(&mut s, funding("new", Venue::Entropy, 2500, "-0.02")).unwrap();
    record_funding(&mut s, funding("late", Venue::Entropy, 1500, "-0.09")).unwrap();
    let r = report(&s);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].settled_funding, Some(d("-0.11")));
    assert_eq!(r.lots[0].remaining_funding, Some(d("-0.08")));
    assert_eq!(r.lots[0].estimated_exit_net, Some(d("0.06")));
}

#[test]
fn late_funding_of_a_closed_lot_is_not_assigned_to_its_replacement() {
    let mut s = snapshot();
    open(&mut s, "old", 100, 1000);
    close(&mut s, "close-old", &[("old", 100)], 2000);
    open(&mut s, "new", 100, 3000);
    record_funding(&mut s, funding("late", Venue::Entropy, 1500, "-0.03")).unwrap();
    let r = report(&s);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].lot_id, "new");
    assert_eq!(r.lots[0].remaining_funding, Some(Decimal::ZERO));
    assert_eq!(r.settled_funding, d("-0.03"));
}

#[test]
fn legacy_backfill_restart_and_duplicates_never_double_book_funding() {
    let mut s = snapshot(); open(&mut s, "a", 100, 1000);
    s.funding_ids.insert("Entropy:old".into());
    s.positions[1].funding = d("-0.03");
    let mut old = serde_json::to_value(&s).unwrap();
    old.as_object_mut().unwrap().remove("funding_records");
    let mut s: Snapshot = serde_json::from_value(old).unwrap();
    let before = s.total_pnl(&books(10000)).unwrap();
    assert!(!report(&s).funding_complete);
    assert_eq!(report(&s).lots[0].estimated_exit_net, None);
    let f = funding("old", Venue::Entropy, 1500, "-0.03");
    record_funding(&mut s, f.clone()).unwrap();
    let mut s: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    record_funding(&mut s, f).unwrap();
    assert!(report(&s).funding_complete);
    assert_eq!(s.total_pnl(&books(10000)).unwrap(), before);
    assert_eq!(s.positions[1].funding, d("-0.03"));
    assert!(record_funding(&mut s, funding("old", Venue::Entropy, 1500, "-0.04")).is_err());
    assert_eq!(s.positions[1].funding, d("-0.03"));
}

#[test]
fn funding_rounding_conserves_cash_and_batch_close_consumes_allocations_in_order() {
    let mut s = snapshot();
    for id in ["a", "b", "c"] { open(&mut s, id, 100, 1000); }
    record_funding(&mut s, funding("split", Venue::Entropy, 1500, "-1")).unwrap();
    let r = report(&s);
    assert_eq!(r.lots.iter().map(|l| l.settled_funding.unwrap()).sum::<Decimal>(), d("-1"));
    close(&mut s, "batch", &[("a", 100), ("b", 50)], 2000);
    let r = report(&s);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].lot_id, "b");
    assert_eq!(r.lots[0].remaining_funding.unwrap(), r.lots[0].settled_funding.unwrap() / d("2"));
    assert_eq!(r.lots[1].remaining_funding, r.lots[1].settled_funding);
}

#[test]
fn failed_open_funding_stays_outside_later_lot_profit_but_inside_total() {
    let mut s = snapshot(); open(&mut s, "a", 100, 1000);
    fill(&mut s, "failed", Venue::Entropy, Side::Buy, 50, 1400, "100");
    fill(&mut s, "failed", Venue::Entropy, Side::Sell, 50, 1700, "100");
    record_funding(&mut s, funding("charge", Venue::Entropy, 1500, "-0.03")).unwrap();
    let r = report(&s);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].remaining_funding, Some(d("-0.02")));
    assert_eq!(r.unallocated_funding, d("-0.01"));
    assert_eq!(r.lots[0].estimated_exit_net.unwrap() + r.unallocated_funding,
        s.total_pnl(&books(10000)).unwrap());
}

#[test]
fn ambiguous_settlement_boundary_and_stale_quotes_never_display_false_profit() {
    let mut s = snapshot(); open(&mut s, "a", 100, 1000);
    record_funding(&mut s, funding("boundary", Venue::Entropy, 1001, "-0.03")).unwrap();
    assert!(!report(&s).funding_complete);
    assert_eq!(report(&s).lots[0].estimated_exit_net, None);
    let mut clean = snapshot(); open(&mut clean, "a", 100, 1000);
    let r = AccountingCache::default().report(&clean, &books(1000), 10000);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].estimated_exit_net, None);
}

#[test]
fn reporting_funding_does_not_change_existing_profit_exit_conditions() {
    let mut s = snapshot(); open(&mut s, "a", 1000, 1000);
    s.config.shared_exit_conditions = true;
    s.config.accumulation = Some(AccumulationRules { contraction_ratio: d("0.5"),
        entry_floor: d("5"), interval_ms: 900000, max_time_adds: 5 });
    assert!(strategy::group_exit_eligible(&s, &s.lots[0], &books(10000), 1000, 10000).unwrap());
    record_funding(&mut s, funding("cost", Venue::Entropy, 2000, "-0.8")).unwrap();
    assert_eq!(report(&s).lots[0].estimated_exit_net, Some(d("-0.1")));
    assert!(strategy::group_exit_eligible(&s, &s.lots[0], &books(10000), 1000, 10000).unwrap());
}

#[test]
fn accounting_cache_refreshes_after_settlement_and_after_partial_close() {
    let mut s = snapshot(); open(&mut s, "a", 200, 1000);
    let mut cache = AccountingCache::default();
    assert_eq!(cache.report(&s, &books(10000), 10000).lots[0].remaining_funding, Some(d("0")));
    record_funding(&mut s, funding("cost", Venue::Entropy, 1500, "-0.02")).unwrap();
    assert_eq!(cache.report(&s, &books(10000), 10000).lots[0].remaining_funding, Some(d("-0.02")));
    close(&mut s, "close", &[("a", 100)], 2000);
    assert_eq!(cache.report(&s, &books(10000), 10000).lots[0].remaining_funding, Some(d("-0.01")));
}

#[test]
fn reverse_direction_includes_actual_open_fees_and_protected_close_costs_once() {
    let mut s = snapshot();
    s.direction = Direction::LighterLong;
    s.config.execution_slippage_bps = d("1");
    s.config.fee_lighter = d("0.0001");
    s.config.fee_entropy = d("0.0004");
    for (venue, side, price, fee) in [(Venue::Lighter, Side::Buy, "100", "0.001"),
        (Venue::Entropy, Side::Sell, "112", "0.002")] {
        let f = Fill { id: format!("reverse-{venue:?}"), order_id: "reverse-v2-fill".into(),
            venue, side, units: 100, price: d(price), fee: d(fee), time_ms: 1000 };
        s.record_fill(&f, None).unwrap();
    }
    s.lots.push(Lot { id: "reverse".into(), level: 0, units: 100, opened_ms: 1001,
        entry_spread: d("12"), entry_net_spread: Some(d("11.7")) });
    record_funding(&mut s, funding("cost", Venue::Lighter, 2000, "-0.003")).unwrap();
    let mut current = books(10000);
    current[1].asks[0].price = d("110");
    let mut cache = AccountingCache::default();
    let r = cache.report(&s, &current, 10000);
    assert!(r.funding_complete);
    assert_eq!(r.lots[0].estimated_exit_net, Some(d("0.0632399665")));
    assert_eq!(r.lots[0].estimated_exit_net, Some(s.total_pnl(&current).unwrap()));
    current[1].asks[0].price = d("111");
    let changed = cache.report(&s, &current, 10000);
    assert!(changed.lots[0].estimated_exit_net < r.lots[0].estimated_exit_net);
    assert_eq!(changed.lots[0].estimated_exit_net, Some(s.total_pnl(&current).unwrap()));
}
