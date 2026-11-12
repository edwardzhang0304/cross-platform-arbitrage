#[test]
fn paper_and_live_share_the_same_decision_trace() {
    use sha2::{Digest, Sha256};
    let mut trace = Vec::new();
    for capital_multiplier in [1,10] {
    for market in [MarketPair::Openai, MarketPair::Anth] {
        for reverse in [false, true] {
            for case in ["first", "grid", "time", "quota_exhausted", "close", "missing_funding",
                "stale_funding", "future_funding", "disconnected", "stale_book", "funds", "emergency"] {
                let now = 4_000_000;
                let (mut paper, mut accounts) = accumulation_fixture(now, reverse);
                paper.config.market = market;
                if capital_multiplier == 10 {
                    paper.config.set_initial_capital(10).unwrap();
                    for p in &mut paper.positions {p.units *= 10;}
                    for l in &mut paper.lots {l.units *= 10;}
                    for a in &mut accounts {a.position_units *= 10;a.free_margin *= d(10);a.equity *= d(10);}
                }
                paper.config.grid = d(5); paper.config.entry_offset = Decimal::ZERO;
                paper.config.entry_confirmation_ms = Some(5000);
                let rules = paper.config.accumulation.as_mut().unwrap();
                rules.interval_ms = 3_600_000; rules.max_time_adds = 5;
                rules.quota_scope = TimeAddQuotaScope::GridStage;
                paper.instance_id = format!("parity-{}-{capital_multiplier}-{reverse}-{case}", market.id());
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
                    if capital_multiplier==10 {for book in &mut b {for level in book.bids.iter_mut().chain(&mut book.asks) {level.units*=10;}}}
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
                    if let Ok(Some(op)) = &p {
                        assert_eq!(op.entry_hedge_retry.is_some(),op.action==Action::Open);
                        final_action = Some(op.action);
                    }
                    trace.push(serde_json::json!({"market":market,"capital_multiplier":capital_multiplier,"reverse":reverse,"case":case,
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
    }
    // Risk valuation and reducing slices must also agree across OS/runtime.
    for scale in [1,10] {for market in [MarketPair::Openai,MarketPair::Anth] {for reverse in [false,true] {
        for loss in [false,true] {
            let now=4_000_000;let (mut paper,mut accounts,group)=thin_group_fixture(now,market,scale,reverse);
            paper.instance_id=format!("thin-parity-{}-{scale}-{reverse}-{loss}",market.id());
            if loss {paper.config.max_loss_usdc=d(scale);paper.config.close_slice_notional=d(150*scale);}
            let mut live=paper.clone();live.config.mode=Mode::Live;
            for offset in [0,15000] {
                let t=now+offset;for a in &mut accounts {a.observed_ms=t;}
                let b=thin_books(t,if loss {40}else{12},reverse,group);
                let p=strategy::evaluate(&mut paper,&b,&accounts,t).unwrap();
                let l=strategy::evaluate(&mut live,&b,&accounts,t).unwrap();
                assert_eq!(serde_json::to_value(&p).unwrap(),serde_json::to_value(&l).unwrap());
                if loss || offset>0 {assert_eq!(p.as_ref().map(|op|op.action),Some(Action::Close));}
                let mut normalized=serde_json::to_value(&live).unwrap();normalized["config"]["mode"]="paper".into();
                assert_eq!(serde_json::to_value(&paper).unwrap(),normalized);
                let risk=paper.risk_valuation(&b,t).unwrap();assert!(!risk.depth_complete);
                trace.push(serde_json::json!({"market":market,"capital_multiplier":scale,"reverse":reverse,
                    "case":if loss {"thin_loss"}else{"thin_profit"},"offset":offset,"operation":p,"risk":risk}));
            }
        }
    }}}
    // Per-lot age and settlement costs are part of the same release proof.
    for scale in [1,10] {for market in [MarketPair::Openai,MarketPair::Anth] {for reverse in [false,true] {
        for case in ["decay_fresh","decay_half","decay_mature","decay_funding_cost","decay_funding_unknown"] {
            let day=86_400_000;let now=20*day;
            let age=match case {"decay_fresh"=>0,"decay_half"=>7*day/2,_=>7*day};
            let mut paper=super::profit_decay_tests::seed(market,scale,reverse,age);
            paper.instance_id=format!("decay-parity-{}-{scale}-{reverse}-{case}",market.id());
            if case=="decay_funding_cost" || case=="decay_funding_unknown" {
                accounting::record_funding(&mut paper,Funding{id:"cost".into(),venue:Venue::Entropy,
                    time_ms:now-day,amount:-Decimal::new(7,1)*Decimal::from(scale)}).unwrap();
                if case=="decay_funding_unknown" {paper.funding_records.clear();}
            }
            let mut live=paper.clone();live.config.mode=Mode::Live;
            for offset in [0,1000] {
                let t=now+offset;let spread=if case=="decay_half"{"34"}else{"44"};
                let b=super::profit_decay_tests::books(&paper,spread,t);
                let a=super::profit_decay_tests::accounts(&paper,t);
                let p=strategy::evaluate(&mut paper,&b,&a,t).unwrap();
                let l=strategy::evaluate(&mut live,&b,&a,t).unwrap();
                assert_eq!(serde_json::to_value(&p).unwrap(),serde_json::to_value(&l).unwrap());
                let expected=offset>0 && matches!(case,"decay_half"|"decay_mature");
                assert_eq!(p.as_ref().map(|o|o.action),expected.then_some(Action::Close));
                let mut normalized=serde_json::to_value(&live).unwrap();normalized["config"]["mode"]="paper".into();
                assert_eq!(serde_json::to_value(&paper).unwrap(),normalized);
                trace.push(serde_json::json!({"market":market,"capital_multiplier":scale,"reverse":reverse,
                    "case":case,"offset":offset,"operation":p,
                    "target":strategy::group_profit_target(&paper.config,&paper.lots[0],t)}));
            }
        }
    }}}
    for scale in [1,10] {for market in [MarketPair::Openai,MarketPair::Anth] {for reverse in [false,true] {
        let now=4_000_000;
        let (mut paper,mut accounts,_)=thin_group_fixture(now,market,scale,reverse);
        paper.paused=false;
        paper.instance_id=format!("availability-parity-{}-{scale}-{reverse}",market.id());
        let mut live=paper.clone();live.config.mode=Mode::Live;
        for offset in 0..=12 {
            let t=now+offset*1000;
            for a in &mut accounts {a.observed_ms=t;a.free_margin=if offset==0 {Decimal::ZERO}else{d(1000)};}
            let b=thin_books(t,12,reverse,2_000_000);
            entry_availability::update(&mut paper,&b,&accounts,t);
            entry_availability::update(&mut live,&b,&accounts,t);
            let mut normalized=serde_json::to_value(&live).unwrap();normalized["config"]["mode"]="paper".into();
            assert_eq!(serde_json::to_value(&paper).unwrap(),normalized);
            assert_eq!(entry_availability::active(&paper),offset<11);
            trace.push(serde_json::json!({"case":"automatic_funding_recovery","market":market,
                "scale":scale,"reverse":reverse,"offset":offset,"status":paper.status,"pause":paper.automatic_entry_pause}));
        }
    }}}
    if let Some(path) = std::env::var_os("STRATEGY_PARITY_REPORT") {
        let mut report = crate::build_info::current();
        report["trace_sha256"] = format!("{:x}", Sha256::digest(serde_json::to_vec(&trace).unwrap())).into();
        report["observations"] = trace.len().into();
        report["parameters"] = serde_json::json!([1,10].into_iter().flat_map(|scale|[MarketPair::Openai,MarketPair::Anth].map(|m| {
            let mut c=crate::profiles::paper_config(m).unwrap();c.set_initial_capital(scale).unwrap();crate::build_info::rules_fingerprint(&c)
        })).collect::<Vec<_>>());
        let path = std::path::Path::new(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
