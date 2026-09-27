use super::*;
use rust_decimal::Decimal;

fn profiles() -> (InventoryConfig, InventoryConfig) {
    let old = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-one-entry.json")).unwrap();
    let mut new: InventoryConfig = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-strategy.json")).unwrap();
    // This fixture tests the earlier three-bound upgrade, before the separate time-add update.
    new.accumulation.as_mut().unwrap().max_time_adds = 2;
    (old, new)
}

fn held(config: InventoryConfig, now: u64) -> Snapshot {
    let mut s = Snapshot::new(config).unwrap();
    s.status = Status::Stopped;
    s.paused = true;
    s.stop_requested = true;
    s.direction = Direction::LighterShort;
    s.positions[0] = Position { units: -80, average: Decimal::from(1706), ..Default::default() };
    s.positions[1] = Position { units: 80, average: Decimal::from(1698), ..Default::default() };
    s.lots.push(Lot { id: "owned-first-entry".into(), level: 0, units: 80,
        opened_ms: now - 10_000, entry_spread: Decimal::from(8), entry_net_spread: Some(Decimal::from(8)) });
    s.anchor = Some(Decimal::from(8));
    s.last_open_completed = Some((now - 10_000, Decimal::from(8)));
    s.armed[0] = false;
    s.funding_synced_ms = now;
    s.entry_attempts_remaining = Some(0);
    s
}

fn evidence(s: &Snapshot, now: u64) -> ([AccountEvidence; 2], [Book; 2]) {
    let a = [Venue::Lighter, Venue::Entropy].map(|venue| AccountEvidence {
        venue, account: if venue == Venue::Lighter {s.config.lighter_account.clone()} else {s.config.entropy_account.clone()},
        observed_ms: now, position_units: s.positions[venue.index()].units,
        free_margin: Decimal::from(90), equity: Decimal::from(100), leverage: 3,
        isolated: true, open_orders: 0, authenticated: true, liquidation_price: None,
    });
    let b = [1706, 1698].map(|p| Book { bids: vec![Level {price:p.into(),units:10000}],
        asks:vec![Level {price:p.into(),units:10000}],received_ms:now,connected:true });
    (a,b)
}

#[test]
fn profile_restores_only_approved_bounds() {
    let (old, new) = profiles();
    new.validate_live_limit_upgrade(&old).unwrap();
    assert_eq!(new.entry_threshold(Decimal::from(6)), Decimal::from(6));
    assert_eq!(new.entry_confirmation_ms, Some(5000));
    assert_eq!(new.max_groups,20);
    assert_eq!(new.max_loss_usdc,Decimal::from(30));
    for kind in ["offset","account","slippage","size","loss"] {
        let mut changed = new.clone();
        match kind {
            "offset" => changed.entry_offset=Decimal::ONE,
            "account" => changed.entropy_account="different".into(),
            "slippage" => changed.execution_slippage_bps=Decimal::from(2),
            "size" => changed.group_notional=Decimal::from(20),
            _ => changed.max_loss_usdc=Decimal::from(31),
        }
        assert!(changed.validate_live_limit_upgrade(&old).is_err(),"{kind}");
    }
}

#[test]
fn explicit_upgrade_preserves_owned_inventory_and_stays_disarmed() {
    let now=100_000;let (old,new)=profiles();let s=held(old.clone(),now);
    let folder=std::env::temp_dir().join(format!("live-limits-{}",s.instance_id));let path=folder.join("state.sqlite");
    let (mut db,_)=store::Store::open(&path,&old).unwrap();db.commit(&s,now,"held").unwrap();drop(db);
    assert!(store::Store::open(&path,&new).is_err());
    let (db,mut restored)=store::Store::open_with_live_limit_upgrade(&path,&new).unwrap();
    assert_eq!(serde_json::to_value(&restored.lots).unwrap(),serde_json::to_value(&s.lots).unwrap());
    assert_eq!(serde_json::to_value(&restored.positions).unwrap(),serde_json::to_value(&s.positions).unwrap());
    assert_eq!(restored.anchor,s.anchor);assert_eq!(restored.last_open_completed,s.last_open_completed);
    assert_eq!(restored.armed.len(),20);assert!(!restored.armed[0]);assert!(restored.armed[1..].iter().all(|v|*v));
    assert_eq!(restored.entry_attempts_remaining,Some(0));assert!(restored.paused);
    // Account recovery must complete before a separate, explicit activation.
    let (a,b)=evidence(&restored,now);
    assert!(service::start_live_strategy(&mut restored,Some(&a),&b,now).is_err());
    restored.status=Status::PausedEntries;
    service::start_live_strategy(&mut restored,Some(&a),&b,now).unwrap();
    assert_eq!(restored.entry_attempts_remaining,None);assert!(!restored.paused);
    assert_eq!(restored.anchor,s.anchor);assert_eq!(restored.lots.len(),1);
    drop(db);std::fs::remove_dir_all(folder).unwrap();
}

#[test]
fn upgrade_rejects_active_and_risk_locked_inventory() {
    for bad in ["running","loss","exit"] {
        let (old,new)=profiles();let mut s=held(old.clone(),100_000);
        match bad {
            "running" => {s.status=Status::Running;s.paused=false;},
            "loss" => s.loss_stop=Some(LossStop{at_ms:1,net_pnl:Decimal::from(-2)}),
            _ => s.close_requested=true,
        }
        let folder=std::env::temp_dir().join(format!("live-limits-{}",s.instance_id));let path=folder.join("state.sqlite");
        let (mut db,_)=store::Store::open(&path,&old).unwrap();db.commit(&s,1,"blocked").unwrap();drop(db);
        assert!(store::Store::open_with_live_limit_upgrade(&path,&new).is_err(),"{bad}");
        let (db,restored)=store::Store::open(&path,&old).unwrap();assert_eq!(restored.config,old);
        drop(db);std::fs::remove_dir_all(folder).unwrap();
    }
}

#[test]
fn activation_cannot_bypass_account_risk_or_clear_loss_latch() {
    let (_,new)=profiles();let now=100_000;let s=held(new,now);let (a,b)=evidence(&s,now);
    for bad in ["stale","orders","cross","identity","position","future","funding","loss","attention"] {
        let mut next=s.clone();let mut a=a.clone();
        match bad {
            "stale"=>a[0].observed_ms=0,
            "orders"=>a[1].open_orders=1,
            "cross"=>a[0].isolated=false,
            "identity"=>a[1].account="different".into(),
            "position"=>a[1].position_units=0,
            "future"=>a[0].observed_ms=now+1,
            "funding"=>next.funding_synced_ms=0,
            "loss"=>next.loss_stop=Some(LossStop{at_ms:1,net_pnl:Decimal::from(-30)}),
            _=>next.status=Status::NeedsAttention,
        }
        assert!(service::start_live_strategy(&mut next,Some(&a),&b,now).is_err(),"{bad}");
        assert_eq!(next.entry_attempts_remaining,Some(0));assert!(next.paused);
    }
}

#[test]
fn time_quota_update_preserves_inventory_counters_and_survives_restart() {
    let (_, cfg) = profiles();
    let now = 1_000_000;
    let mut s = held(cfg.clone(), now);
    s.time_adds_used = 1;
    s.opened_groups = 2;
    s.entry_confirmations[1] = Some((now, 1));
    s.time_entry_confirmations[1] = Some((now, 21));
    let original = s.clone();
    let (a, _) = evidence(&s, now);
    service::set_max_time_adds(&mut s, 5, Some(&a), now).unwrap();
    let mut expected = cfg.clone();
    expected.accumulation.as_mut().unwrap().max_time_adds = 5;
    assert_eq!(s.config, expected);
    assert_eq!(s.time_adds_used, 1);
    assert_eq!(s.last_open_completed, original.last_open_completed);
    assert_eq!(s.armed, original.armed);
    assert_eq!(s.anchor, original.anchor);
    assert_eq!(s.opened_groups, 2);
    assert_eq!(serde_json::to_value(&s.lots).unwrap(), serde_json::to_value(&original.lots).unwrap());
    assert_eq!(serde_json::to_value(&s.positions).unwrap(), serde_json::to_value(&original.positions).unwrap());
    assert!(s.paused && s.stop_requested);
    assert_eq!(s.entry_attempts_remaining, Some(0));
    let folder = std::env::temp_dir().join(format!("time-add-update-{}", s.instance_id));
    let path = folder.join("state.sqlite");
    let (mut db, _) = store::Store::open(&path, &cfg).unwrap();
    s.entry_confirmations[1] = Some((now, 1));
    s.time_entry_confirmations[1] = Some((now, 21));
    db.command(&s, "quota-five", "set_max_time_adds", now).unwrap();
    drop(db);
    let (db, restored) = store::Store::open(&path, &expected).unwrap();
    assert_eq!(restored.time_adds_used, 1);
    assert_eq!(restored.last_open_completed, original.last_open_completed);
    assert_eq!(restored.entry_confirmations, [None; 2]);
    assert_eq!(restored.time_entry_confirmations, [None; 2]);
    drop(db);
    std::fs::remove_dir_all(folder).unwrap();
    let active: InventoryConfig = serde_json::from_str(include_str!("../../../tests/fixtures/inventory/live-strategy.json")).unwrap();
    assert_eq!(active, expected);
}

#[test]
fn time_quota_update_rejects_risk_locks_bad_accounts_and_unapproved_limit() {
    let (_, cfg) = profiles();
    let now = 1_000_000;
    for bad in ["running", "loss", "exit", "stale", "position", "identity", "orders", "too_many", "below_used"] {
        let mut s = held(cfg.clone(), now);
        s.time_adds_used = 1;
        let (mut a, _) = evidence(&s, now);
        let mut quota = 5;
        match bad {
            "running" => { s.status = Status::Running; s.paused = false; }
            "loss" => s.loss_stop = Some(LossStop { at_ms: now, net_pnl: Decimal::from(-30) }),
            "exit" => s.close_requested = true,
            "stale" => a[0].observed_ms = 0,
            "position" => a[0].position_units = 0,
            "identity" => a[0].account = "other".into(),
            "orders" => a[1].open_orders = 1,
            "too_many" => quota = 6,
            _ => quota = 0,
        }
        assert!(service::set_max_time_adds(&mut s, quota, Some(&a), now).is_err(), "{bad}");
        assert_eq!(s.config, cfg);
        assert_eq!(s.time_adds_used, 1);
    }
}
