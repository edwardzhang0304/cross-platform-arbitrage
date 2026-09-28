fn legacy_rules_snapshot(market: MarketPair, reverse: bool, mode: Mode) -> Snapshot {
    let mut cfg: InventoryConfig = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-strategy.json")).unwrap();
    cfg.market = market; cfg.mode = mode;
    cfg.lighter_address = Some(format!("0x{}", "3".repeat(40)));
    if market == MarketPair::Anth && mode == Mode::Live {
        cfg.lighter_account = "anth:live:lighter".into();
        cfg.entropy_account = "anth:live:entropy".into();
    }
    let mut s = Snapshot::new(cfg).unwrap();
    if reverse { s.direction = Direction::LighterShort; }
    s.status = Status::Stopped; s.paused = true; s.stop_requested = true;
    s.anchor = Some(d(10)); s.time_adds_used = 4;
    s.last_open_completed = Some((1_000_000, d(16)));
    s.closed_groups = 7; s.opened_groups = 12;
    s.positions[0] = Position {units: s.direction.sign()*5000, average:d(100),
        fees: Decimal::new(2,2), funding: Decimal::new(-1,2), realized:d(2)};
    s.positions[1] = Position {units: -s.direction.sign()*5000, average:d(110),
        fees: Decimal::new(1,2), funding: Decimal::new(2,2), realized:d(3)};
    s.lots = [0,1,2,3,24].into_iter().enumerate().map(|(i,level)| Lot {
        id:format!("old-{i}"),level,units:1000,opened_ms:900_000+i as u64,
        entry_spread:d(10+2*i as i64),entry_net_spread:Some(Decimal::new(999+200*i as i64,2))
    }).collect();
    s.armed[..4].fill(false);
    s
}

fn hourly_rules_snapshot(market: MarketPair, reverse: bool, mode: Mode) -> Snapshot {
    let mut s = legacy_rules_snapshot(market, reverse, mode);
    s.config = rules_upgrade::current_config(&s.config).unwrap();
    s.config.accumulation.as_mut().unwrap().interval_ms = 3_600_000;
    // A slot may already have re-armed while its older lot still exists.
    s.armed[0] = true;
    s
}

#[test]
fn hourly_upgrade_changes_only_interval_preserving_slots_quota_and_completion_clock() {
    for market in [MarketPair::Openai, MarketPair::Anth] {
        for reverse in [false, true] { for mode in [Mode::Paper, Mode::Live] {
            for used in [0, 4, 5] {
                let mut s = hourly_rules_snapshot(market, reverse, mode);
                s.time_adds_used = used;
                let before = serde_json::to_value(&s).unwrap();
                let desired = rules_upgrade::current_config(&s.config).unwrap();
                let mut expected_config = s.config.clone();
                expected_config.accumulation.as_mut().unwrap().interval_ms = 1_800_000;
                assert_eq!(desired, expected_config);
                assert!(rules_upgrade::migrate_snapshot(&mut s, &desired).unwrap());
                let mut actual = serde_json::to_value(&s).unwrap();
                for key in ["config", "entry_attempts_remaining", "resume_after_recovery", "reason"] {
                    actual[key] = before[key].clone();
                }
                assert_eq!(actual, before, "lots, armed slots, quota and last fill clock must remain exact");
                let after = serde_json::to_value(&s).unwrap();
                assert!(!rules_upgrade::migrate_snapshot(&mut s, &desired).unwrap());
                assert_eq!(serde_json::to_value(&s).unwrap(), after);
            }
        }}
    }
}

#[test]
fn hourly_upgrade_preserves_smaller_quota_and_rejects_unknown_interval_or_running_state() {
    let mut s = hourly_rules_snapshot(MarketPair::Openai, false, Mode::Live);
    s.config.accumulation.as_mut().unwrap().max_time_adds = 3;
    let desired = rules_upgrade::current_config(&s.config).unwrap();
    assert_eq!(desired.accumulation.as_ref().unwrap().max_time_adds, 3);
    s.status = Status::Running;
    let before = serde_json::to_value(&s).unwrap();
    assert!(rules_upgrade::migrate_snapshot(&mut s, &desired).is_err());
    assert_eq!(serde_json::to_value(&s).unwrap(), before);
    s.config.accumulation.as_mut().unwrap().interval_ms = 2_700_000;
    assert!(rules_upgrade::current_config(&s.config).is_err());
}

#[test]
fn rule_upgrade_preserves_money_inventory_and_used_quota_for_both_markets_and_modes() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for reverse in [false,true] { for mode in [Mode::Paper,Mode::Live] {
            let mut s=legacy_rules_snapshot(market,reverse,mode);
            let before=serde_json::to_value(&s).unwrap();
            let desired=rules_upgrade::current_config(&s.config).unwrap();
            assert_eq!(desired.grid,d(5));
            let r=desired.accumulation.as_ref().unwrap();
            assert_eq!((r.interval_ms,r.max_time_adds,r.quota_scope),(1_800_000,5,TimeAddQuotaScope::GridStage));
            assert!(rules_upgrade::migrate_snapshot(&mut s,&desired).unwrap());
            assert_eq!(s.lots.iter().map(|l|l.level).collect::<Vec<_>>(),vec![0,0,0,1,24]);
            assert_eq!(s.time_adds_used,4);
            let mut actual=serde_json::to_value(&s).unwrap();
            for (i,lot) in actual["lots"].as_array_mut().unwrap().iter_mut().enumerate() {lot["level"]=before["lots"][i]["level"].clone();}
            for key in ["config","armed","entry_attempts_remaining","resume_after_recovery","reason"] {actual[key]=before[key].clone();}
            assert_eq!(actual,before,"balances, fills, prices, fees, funding, group IDs and history must stay intact");
            let after=serde_json::to_value(&s).unwrap();
            assert!(!rules_upgrade::migrate_snapshot(&mut s,&desired).unwrap());
            assert_eq!(serde_json::to_value(&s).unwrap(),after);
        }}
    }
}

#[test]
fn rule_upgrade_rejects_running_unpaired_pending_and_changed_identity_without_mutation() {
    let now=4_000_000;
    let (mut candidate,mut accounts)=accumulation_fixture(now,false);
    strategy::evaluate(&mut candidate,&books(now,12),&accounts,now).unwrap();
    for a in &mut accounts {a.observed_ms=now+1000;}
    let pending=strategy::evaluate(&mut candidate,&books(now+1000,12),&accounts,now+1000).unwrap().unwrap();
    for bad in ["running","unpaired","pending","identity","symbol","close","unknown_rule"] {
        let mut s=legacy_rules_snapshot(MarketPair::Openai,false,Mode::Live);
        let mut desired=rules_upgrade::current_config(&s.config).unwrap();
        match bad {
            "running"=>s.status=Status::Running,
            "unpaired"=>s.positions[0].units+=10,
            "pending"=>s.pending=Some(pending.clone()),
            "identity"=>desired.entropy_address=format!("0x{}","4".repeat(40)),
            "symbol"=>desired.market=MarketPair::Anth,
            "close"=>s.close_requested=true,
            _=>s.config.grid=d(7),
        }
        let before=serde_json::to_value(&s).unwrap();
        assert!(rules_upgrade::migrate_snapshot(&mut s,&desired).is_err(),"{bad}");
        assert_eq!(serde_json::to_value(&s).unwrap(),before);
    }
}

#[test]
fn migrated_inventory_uses_new_grid_and_half_hourly_gate_before_granting_next_stage_quota() {
    let now=4_000_000;
    let (mut s,mut a)=accumulation_fixture(now,false);
    s.status=Status::Stopped;s.stop_requested=true;s.paused=true;s.time_adds_used=4;
    s.config.accumulation.as_mut().unwrap().max_time_adds=5;
    s.last_open_completed=Some((now-900_000,d(10)));
    let desired=rules_upgrade::current_config(&s.config).unwrap();
    rules_upgrade::migrate_snapshot(&mut s,&desired).unwrap();
    s.status=Status::Running;s.stop_requested=false;s.paused=false;s.entry_attempts_remaining=None;
    assert!(strategy::evaluate(&mut s,&books(now,12),&a,now).unwrap().is_none());
    for x in &mut a{x.observed_ms=now+1000;}
    assert!(strategy::evaluate(&mut s,&books(now+1000,12),&a,now+1000).unwrap().is_none(),"old 2U grid must not open");
    for x in &mut a{x.observed_ms=now+2000;}
    assert!(strategy::evaluate(&mut s,&books(now+2000,15),&a,now+2000).unwrap().is_none());
    for x in &mut a{x.observed_ms=now+3000;}
    let mut op=strategy::evaluate(&mut s,&books(now+3000,15),&a,now+3000).unwrap().unwrap();
    assert_eq!(op.level,1);
    op.first_filled=op.requested_units;op.hedge_filled=op.requested_units;
    op.first_terminal=true;op.hedge_terminal=true;
    op.first_value=d(115)*Decimal::from(op.requested_units);op.hedge_value=d(100)*Decimal::from(op.requested_units);
    for p in &mut s.positions{p.units+=p.units.signum()*op.requested_units;}
    s.pending=Some(op);s.finish_operation(now+3000).unwrap();
    assert_eq!(s.time_adds_used,0);
    assert_eq!(s.last_open_completed,Some((now+3000,d(15))));
}

#[test]
fn rule_upgrade_transaction_is_locked_idempotent_and_recovers_between_ledger_and_config_writes() {
    use crate::portable::{ProfilePaths,Settings};
    for market in [MarketPair::Openai, MarketPair::Anth] {
        let root=std::env::temp_dir().join(format!("rule-upgrade-{}",uuid::Uuid::new_v4()));
        let paths=ProfilePaths::new(&root,market);
        let s=hourly_rules_snapshot(market,false,Mode::Live);
        let cfg=s.config.clone();let desired=rules_upgrade::current_config(&cfg).unwrap();
        let (mut db,_)=store::Store::open(&paths.database,&cfg).unwrap();
        db.commit(&s,1,"synthetic-stopped").unwrap();
        let mut settings=Settings{accounts:crate::config::AppConfig::default(),strategy:cfg.clone()};
        paths.save(&settings).unwrap();
        std::fs::create_dir_all(paths.vault.parent().unwrap()).unwrap();
        let feishu = paths.vault.with_file_name("feishu.vault");
        // Opaque synthetic bytes: migration must never read or rewrite either vault.
        std::fs::write(&paths.vault, b"synthetic-trading-vault").unwrap();
        std::fs::write(&feishu, b"synthetic-feishu-vault").unwrap();
        let accounts_before = serde_json::to_value(&settings.accounts).unwrap();
        assert!(rules_upgrade::migrate_ledger(&paths.database,&desired).is_err(),"active DB lock");
        drop(db);
        assert!(rules_upgrade::migrate_ledger(&paths.database,&desired).unwrap());
        // Simulate a crash before the separate strategy file replacement.
        assert_eq!(paths.load().unwrap().unwrap().strategy,cfg);
        paths.upgrade_rules(&mut settings).unwrap();
        assert_eq!(settings.strategy,desired);
        assert_eq!(serde_json::to_value(&settings.accounts).unwrap(), accounts_before);
        assert_eq!(paths.load().unwrap().unwrap().strategy,desired);
        paths.upgrade_rules(&mut settings).unwrap();
        let raw=rusqlite::Connection::open(&paths.database).unwrap();
        let count:i64=raw.query_row("SELECT count(*) FROM events WHERE kind='rules_interval_30m_v1'",[],|r|r.get(0)).unwrap();
        assert_eq!(count,1);
        let event:String=raw.query_row("SELECT body FROM events WHERE kind='rules_interval_30m_v1'",[],|r|r.get(0)).unwrap();
        let event:serde_json::Value=serde_json::from_str(&event).unwrap();
        assert_eq!(event["from_interval_ms"],3_600_000);
        assert_eq!(event["to_interval_ms"],1_800_000);
        let body:String=raw.query_row("SELECT body FROM state WHERE id=1",[],|r|r.get(0)).unwrap();
        let restored:Snapshot=serde_json::from_str(&body).unwrap();
        assert_eq!(restored.config,desired);assert_eq!(restored.time_adds_used,4);assert_eq!(restored.closed_groups,7);
        assert_eq!(restored.armed,s.armed);
        assert_eq!(restored.last_open_completed,s.last_open_completed);
        assert_eq!(std::fs::read(&paths.vault).unwrap(),b"synthetic-trading-vault");
        assert_eq!(std::fs::read(&feishu).unwrap(),b"synthetic-feishu-vault");
        drop(raw);std::fs::remove_dir_all(root).unwrap();
    }
}
