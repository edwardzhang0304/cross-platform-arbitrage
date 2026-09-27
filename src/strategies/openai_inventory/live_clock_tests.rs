use super::*;

fn fixture(market: MarketPair, venue: Venue, reduce_only: bool) -> (InventoryConfig, OrderRequest) {
    let mut config = InventoryConfig::default();
    config.market = market;
    let request = OrderRequest {
        id: "synthetic-clock-incident-v2-first".into(), venue, side: Side::Buy,
        units: if market == MarketPair::Anth { 700 } else { 490 },
        limit: Decimal::from(2100), arrival_mid: None, reduce_only,
        created_ms: 1_790_000_095_000, expires_ms: 1_790_000_100_000,
        signed_expires_ms: (venue == Venue::Lighter).then_some(1_790_000_694_000),
    };
    (config, request)
}

#[test]
fn rh_close_with_95_second_fast_local_clock_binds_full_exchange_identity() {
    let (config, request) = fixture(MarketPair::Openai, Venue::Lighter, true);
    let row = json!({"client_order_index":client_id(&request),"order_index":12345,
        "owner_account_index":7,"market_index":42,"is_ask":false,"reduce_only":true,
        "initial_base_amount":"0.0490","price":"2100.00","filled_base_amount":"0.0490",
        "remaining_base_amount":"0","status":"filled","transaction_time":1790000000500u64,
        "created_at":1790000000u64});
    let result = bound_lighter_order(&json!({"orders":[row.clone()]}), &config, 7, &request).unwrap().unwrap();
    assert_eq!(result.created_ms, 1_790_000_000_000);
    assert_eq!(result.filled_units, 490);
    assert_eq!(result.order_index, 12345);
    assert_eq!(request.created_ms, 1_790_000_095_000);
    for (key, value) in [
        ("owner_account_index", json!(8)), ("market_index", json!(38)),
        ("is_ask", json!(true)), ("reduce_only", json!(false)),
        ("initial_base_amount", json!("0.05")), ("price", json!("2101")),
        ("created_at", json!(1789900000u64)), ("filled_base_amount", json!("0.05")),
    ] {
        let mut bad = row.clone(); bad[key] = value;
        assert!(bound_lighter_order(&json!({"orders":[bad]}), &config, 7, &request).is_err(), "{key}");
    }
    assert!(bound_lighter_order(&json!({"orders":[row.clone(),row]}), &config, 7, &request).is_err());
}

fn entropy_fixture() -> (InventoryConfig, OrderRequest, hyperliquid::OrderStatusInfo, hyperliquid::UserFill) {
    let (config, request) = fixture(MarketPair::Anth, Venue::Entropy, false);
    let order = serde_json::from_value(json!({"status":"filled","statusTimestamp":1790000000500u64,
        "order":{"coin":"io:ANTH","side":"B","limitPx":"2100","origSz":"0.007",
        "sz":"0","oid":12345,"timestamp":1790000000000u64,"reduceOnly":false,
        "cloid":format!("0x{}",id(&request).simple())}})).unwrap();
    let fill = serde_json::from_value(json!({"coin":"io:ANTH","side":"B","sz":"0.007",
        "px":"2099.9","fee":"0.001322937","time":1790000000500u64,"oid":12345,
        "hash":"synthetic-fill-hash","tid":100,"startPosition":"0","dir":"Open Long",
        "closedPnl":"0","crossed":true})).unwrap();
    (config, request, order, fill)
}

#[test]
fn entropy_fill_search_uses_exchange_creation_and_retains_real_fill_time() {
    let (config, request, order, fill) = entropy_fixture();
    let start = bound_entropy_created(&config, &request, &order).unwrap().saturating_sub(1000);
    assert!(start <= fill.time && fill.time < request.created_ms.saturating_sub(1000));
    let result = entropy_result(&config, &request, &order, &[fill.clone()]).unwrap();
    assert!(result.terminal);
    assert_eq!(result.exchange_created_ms, Some(1_790_000_000_000));
    assert_eq!(result.fills[0].units, 700);
    assert_eq!(result.fills[0].time_ms, fill.time);
    assert!(!entropy_result(&config, &request, &order, &[]).unwrap().terminal);
    let mut wrong = fill.clone(); wrong.oid += 1;
    assert!(!entropy_result(&config, &request, &order, &[wrong]).unwrap().terminal);
    for change in 0..5 {
        let mut bad = order.clone();
        match change { 0 => bad.order.coin = "io:OAI".into(), 1 => bad.order.orig_sz = "0.07".into(),
            2 => bad.order.cloid = Some("wrong-order".into()), 3 => bad.order.side = "A".into(),
            _ => bad.order.reduce_only = true }
        assert!(entropy_result(&config, &request, &bad, &[fill.clone()]).is_err());
    }
    let mut old_shape = order.clone(); old_shape.order.sz = "0.007".into();
    assert!(!entropy_result(&config, &request, &old_shape, &[]).unwrap().terminal);
    assert!(entropy_result(&config, &request, &old_shape, &[fill]).unwrap().terminal);
}

#[test]
fn live_lighter_request_preserves_anth_and_openai_base_quantity() {
    for market in [MarketPair::Openai, MarketPair::Anth] {
        let (config, request) = fixture(market, Venue::Lighter, false);
        let wire = lighter_exact_request(&config, &request);
        assert_eq!(wire.size_decimals, market.quantity_decimals());
        assert_eq!(Decimal::new(wire.base_amount, wire.size_decimals), config.quantity(request.units));
        assert_eq!(wire.symbol, market.lighter_symbol());
        assert_eq!(wire.max_slippage_bps, 0.);
        let metadata: LighterMarket = serde_json::from_value(json!({
            "symbol":market.lighter_symbol(),"market_id":market.lighter_market_id(),
            "market_type":"perp","status":"active","min_base_amount":"0.00001",
            "min_quote_amount":"10","supported_size_decimals":market.quantity_decimals(),
            "supported_price_decimals":market.lighter_price_decimals(),
            "mark_price":"2100","index_price":"2100"
        })).unwrap();
        let plan = build_exact_base_order_plan_with_reference(&metadata,&wire,2100.).unwrap();
        assert_eq!(plan.base_amount,request.units);
        if market == MarketPair::Anth {
            let mut old = wire; old.size_decimals = 4;
            assert!(build_exact_base_order_plan_with_reference(&metadata,&old,2100.).is_err());
        }
    }
}

#[test]
fn signing_rejects_fast_slow_missing_or_stale_clock_evidence() {
    let start = 1_790_000_000_000;
    assert!(verify_submission_clock(Some((start, start, start+300)),start+400).is_ok());
    for evidence in [None, Some((start+95_000,start,start+95_300)),
        Some((start,start+95_000,start+300)), Some((start,start,start+6_000))] {
        assert!(verify_submission_clock(evidence, start+95_400).is_err());
    }
    assert!(verify_submission_clock(Some((start,start,start+300)),start+16_000).is_err());
    assert!(verify_submission_clock(Some((start,start,start+300)),start-1).is_err());
}
