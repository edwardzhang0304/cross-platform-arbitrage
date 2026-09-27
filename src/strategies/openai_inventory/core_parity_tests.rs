#[test]
fn paper_and_live_share_the_same_decision_trace() {
    use sha2::{Digest, Sha256};
    let mut trace = Vec::new();
    for market in [MarketPair::Openai, MarketPair::Anth] {
        for reverse in [false, true] {
            for case in ["first", "grid", "time", "quota_exhausted", "close", "missing_funding",
                "stale_funding", "future_funding", "disconnected", "stale_book", "funds", "emergency"] {
                let now = 4_000_000;
                let (mut paper, mut accounts) = accumulation_fixture(now, reverse);
                paper.config.market = market;
                paper.config.grid = d(5); paper.config.entry_offset = Decimal::ZERO;
                paper.config.entry_confirmation_ms = Some(5000);
                let rules = paper.config.accumulation.as_mut().unwrap();
                rules.interval_ms = 3_600_000; rules.max_time_adds = 5;
                rules.quota_scope = TimeAddQuotaScope::GridStage;
                paper.instance_id = format!("parity-{}-{reverse}-{case}", market.id());
                paper.last_open_completed = Some((now - 3_600_000, d(10)));
                paper.funding_synced_ms = match case {
                    "missing_funding" | "emergency" => 0,
                    "stale_funding" => now - 90_001,
                    "future_funding" => now + 60_000,
                    _ => now,
                };
                if case == "first" {
                    paper.lots.clear(); paper.positions = Default::default(); paper.anchor = None;
                    paper.last_open_completed = None;
                    for a in &mut accounts { a.position_units = 0; }
                }
                if case == "quota_exhausted" { paper.time_adds_used = 5; }
                if case == "grid" { paper.time_adds_used = 5; }
                if case == "funds" { accounts[0].free_margin = Decimal::ZERO; }
                if case == "emergency" { paper.close_requested = true; }
                let spread = match case {"grid" => 15, "close" => 5, _ => 11};
                let mut live = paper.clone(); live.config.mode = Mode::Live;
                let mut final_action = None;
                for offset in [0, 5000] {
                    let t = now + offset;
                    for a in &mut accounts { a.observed_ms = t; }
                    let mut b = bidir_books(t, spread, reverse);
                    if case == "disconnected" { b[0].connected = false; }
                    if case == "stale_book" { b[0].received_ms = t - 10_000; }
                    let p = strategy::evaluate(&mut paper, &b, &accounts, t);
                    let l = strategy::evaluate(&mut live, &b, &accounts, t);
                    let result = |r: &anyhow::Result<Option<Operation>>| match r {
                        Ok(op) => serde_json::json!({"operation":op}),
                        Err(e) => serde_json::json!({"error":e.to_string()}),
                    };
                    assert_eq!(result(&p), result(&l), "{market:?}/{reverse}/{case}/{offset}");
                    let mut normalized_live = serde_json::to_value(&live).unwrap();
                    normalized_live["config"]["mode"] = serde_json::json!("paper");
                    assert_eq!(serde_json::to_value(&paper).unwrap(), normalized_live);
                    if let Ok(Some(op)) = &p { final_action = Some(op.action); }
                    trace.push(serde_json::json!({"market":market,"reverse":reverse,"case":case,
                        "offset":offset,"result":result(&p),"status":paper.status,
                        "time_adds_used":paper.time_adds_used}));
                }
                let expected = match case {
                    "first" | "grid" | "time" => Some(Action::Open),
                    "close" | "emergency" => Some(Action::Close), _ => None,
                };
                assert_eq!(final_action, expected, "{market:?}/{reverse}/{case}");
            }
        }
    }
    if let Some(path) = std::env::var_os("STRATEGY_PARITY_REPORT") {
        let mut report = crate::build_info::current();
        report["trace_sha256"] = format!("{:x}", Sha256::digest(serde_json::to_vec(&trace).unwrap())).into();
        report["observations"] = trace.len().into();
        report["parameters"] = serde_json::json!([MarketPair::Openai,MarketPair::Anth].map(|m|
            crate::build_info::rules_fingerprint(&crate::profiles::paper_config(m).unwrap())));
        let path = std::path::Path::new(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
