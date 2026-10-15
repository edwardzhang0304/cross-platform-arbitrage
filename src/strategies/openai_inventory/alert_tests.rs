use super::*;

fn market_record(db: &mut Connection, s: &Snapshot, at: u64, healthy: bool) {
    let good = Book { bids: vec![Level { price: 2000.into(), units: 1000 }],
        asks: vec![Level { price: 2001.into(), units: 1000 }], received_ms: at, connected: true };
    let mut books = [good.clone(), good];
    if !healthy { books[1].received_ms = at.saturating_sub(10_000); }
    let tx = db.transaction().unwrap();
    observe_market(&tx, s, &books, at).unwrap();
    tx.commit().unwrap();
}

#[test]
fn market_outage_waits_for_observed_minute_and_recovers_without_spam() {
    for market in [MarketPair::Openai, MarketPair::Anth] {
        let mut db = db(); let mut c = InventoryConfig::default(); c.market = market;
        let mut s = Snapshot::new(c).unwrap(); s.status = Status::Running;
        s.reason = "secret-token-do-not-send".into();
        for at in (10_000..70_000).step_by(1000) {
            market_record(&mut db, &s, at, false); assert!(rows(&db).is_empty());
        }
        market_record(&mut db, &s, 70_000, false);
        let (id, body) = rows(&db).pop().unwrap();
        assert!(body.contains("60 秒盘口不可用")); assert!(body.contains(&market.id().to_uppercase()));
        assert!(!body.contains("secret-token")); assert!(!body.contains("异常已持续至少"));
        { let tx = db.transaction().unwrap();
          tx.execute("DELETE FROM notification_outbox WHERE id=?1", [&id]).unwrap();
          delivered(&tx, &id, 70_000).unwrap(); tx.commit().unwrap(); }
        for at in (71_000..370_000).step_by(1000) {
            market_record(&mut db, &s, at, false); assert!(rows(&db).is_empty());
        }
        market_record(&mut db, &s, 370_000, false); assert_eq!(rows(&db).len(), 1);
        market_record(&mut db, &s, 371_000, true); assert!(rows(&db).is_empty());
        { let tx = db.transaction().unwrap(); tick(&tx, 999_000).unwrap(); tx.commit().unwrap(); }
        assert!(rows(&db).is_empty());
    }
}

#[test]
fn market_brief_jitter_and_slow_healthy_observations_never_accumulate_into_outage() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap(); s.status = Status::Running;
    for slow in [false, true] {
        let mut db = db();
        for n in 0..900 {
            // rc14 falsely alerted for both of these traces.
            let good = if slow { n > 0 } else { n % 10 != 0 };
            market_record(&mut db, &s, 10_000 + n * if slow { 4000 } else { 1000 }, good);
            let tx = db.transaction().unwrap(); tick(&tx, 10_000 + n * if slow { 4000 } else { 1000 }).unwrap(); tx.commit().unwrap();
            assert!(rows(&db).is_empty());
        }
    }
}

#[test]
fn market_rest_blips_do_not_hide_mostly_unavailable_feed_and_venues_do_not_combine() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap(); s.status = Status::Running;
    let mut db = db();
    for n in 0..=60 { market_record(&mut db, &s, 10_000+n*1000, n % 15 == 1); }
    assert_eq!(rows(&db).len(), 1); assert!(rows(&db)[0].1.contains("56 秒盘口不可用"));
    let mut db = self::db();
    for n in 0..=300 {
        let at = 10_000+n*1000;
        let good = Book { bids:vec![Level{price:2000.into(),units:1000}], asks:vec![Level{price:2001.into(),units:1000}], connected:true,received_ms:at };
        let mut books = [good.clone(),good]; books[(n/10%2) as usize].connected=false;
        let tx=db.transaction().unwrap(); observe_market(&tx,&s,&books,at).unwrap(); tx.commit().unwrap();
        assert!(rows(&db).is_empty());
    }
}

#[test]
fn market_48_second_threshold_current_recovery_and_observation_gaps() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap(); s.status=Status::Running;
    for healthy_seconds in [12,13] {
        let mut db=db();
        for n in 0..=60 { market_record(&mut db,&s,10_000+n*1000,n<healthy_seconds); }
        assert_eq!(rows(&db).len(),usize::from(healthy_seconds==12));
        market_record(&mut db,&s,71_000,true); assert!(rows(&db).is_empty());
        // Losing observation continuity must never mature a notification.
        market_record(&mut db,&s,100_000,false);
        {let tx=db.transaction().unwrap();tick(&tx,160_000).unwrap();tx.commit().unwrap();}
        assert!(rows(&db).is_empty());
    }
    let mut db=db();
    for n in 0..=60 { market_record(&mut db,&s,10_000+n*1000,n==60); }
    assert!(rows(&db).is_empty()); // fresh at the boundary cancels immediately
    market_record(&mut db,&s,9000,false); // backward clock resets evidence
    assert!(rows(&db).is_empty());
}

#[test]
fn market_alerts_monitor_held_stopped_positions_without_canceling_execution_alerts() {
    let mut db=db(); let mut s=Snapshot::new(InventoryConfig::default()).unwrap();
    s.status=Status::Stopped; s.paused=true; s.stop_requested=true;
    for at in (10_000..=70_000).step_by(1000) {market_record(&mut db,&s,at,false);}
    assert!(rows(&db).is_empty());
    s.positions[0].units=-700; s.positions[1].units=700;
    for at in (80_000..=140_000).step_by(1000) {market_record(&mut db,&s,at,false);}
    assert_eq!(rows(&db).len(),1);
    pending(&mut s); s.pending.as_mut().unwrap().failed=true;
    record(&mut db,&s,140_000); record(&mut db,&s,200_000); assert_eq!(rows(&db).len(),2);
    market_record(&mut db,&s,201_000,true);
    assert_eq!(rows(&db).len(),1); assert!(rows(&db)[0].1.contains("持续交易异常"));
}

#[test]
fn market_migration_restart_and_delivered_rate_limit_do_not_replay_stale_notices() {
    let path=std::env::temp_dir().join(format!("cpa-market-alert-{}.sqlite",uuid::Uuid::new_v4()));
    let mut s=Snapshot::new(InventoryConfig::default()).unwrap(); s.status=Status::Running;
    {let mut db=Connection::open(&path).unwrap(); db.execute_batch(SCHEMA).unwrap();
     let tx=db.transaction().unwrap();
     enqueue(&tx,1,"【多平台套利 · 行情持续异常】old false alarm").unwrap();
     enqueue(&tx,2,"【多平台套利 · 持续交易异常】keep").unwrap();
     tx.execute("INSERT INTO market_notification_state VALUES(1,?1)",[r#"{"incident":{"started_ms":1},"healthy_since_ms":null,"last_observed_ms":2}"#]).unwrap();
     reset_market(&tx,10_000).unwrap(); tx.commit().unwrap();
     assert_eq!(rows(&db).len(),1); assert!(rows(&db)[0].1.contains("keep"));
     db.execute("DELETE FROM notification_outbox",[]).unwrap();
     for at in (10_000..=70_000).step_by(1000) {market_record(&mut db,&s,at,false);}
     assert_eq!(rows(&db).len(),1);}
    {let mut db=Connection::open(&path).unwrap();
     let id=rows(&db)[0].0.clone(); let tx=db.transaction().unwrap();
     tx.execute("DELETE FROM notification_outbox",[]).unwrap(); delivered(&tx,&id,70_000).unwrap();
     reset_market(&tx,71_000).unwrap(); tx.commit().unwrap();
     for at in (71_000..370_000).step_by(1000) {market_record(&mut db,&s,at,false); assert!(rows(&db).is_empty());}
     market_record(&mut db,&s,370_000,false); assert_eq!(rows(&db).len(),1);
     let tx=db.transaction().unwrap(); reset_market(&tx,370_001).unwrap(); tx.commit().unwrap();
     assert!(rows(&db).is_empty());}
    std::fs::remove_file(path).unwrap();
}

#[test]
fn market_evidence_history_is_bounded_and_contains_no_private_context() {
    let mut db=db(); let mut s=Snapshot::new(InventoryConfig::default()).unwrap(); s.status=Status::Running;
    s.reason="secret-not-for-history".into();
    for n in 0..600 {
        market_record(&mut db,&s,10_000+n*1000,n>60 && n%10==0);
    }
    let count:i64=db.query_row("SELECT COUNT(*) FROM market_notification_history",[],|r|r.get(0)).unwrap();
    assert_eq!(count,64);
    let body:String=db.query_row("SELECT body FROM market_notification_history ORDER BY seq DESC LIMIT 1",[],|r|r.get(0)).unwrap();
    assert!(body.contains("unavailable_ms")); assert!(!body.contains("secret-not-for-history"));
    let state:String=db.query_row("SELECT body FROM market_notification_state",[],|r|r.get(0)).unwrap();
    let value:serde_json::Value=serde_json::from_str(&state).unwrap();
    assert!(value["samples"].as_array().unwrap().len()<=62);
}

fn db()->Connection {let db=Connection::open_in_memory().unwrap();db.execute_batch(SCHEMA).unwrap();db}
fn record(db:&mut Connection,s:&Snapshot,at:u64){let tx=db.transaction().unwrap();observe(&tx,s,at).unwrap();tx.commit().unwrap();}
fn rows(db:&Connection)->Vec<(String,String)> {
    db.prepare("SELECT id,body FROM notification_outbox ORDER BY rowid").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().map(|r|r.unwrap()).collect()
}
fn pending(s:&mut Snapshot){s.pending=Some(serde_json::from_value(serde_json::json!({
    "id":"synthetic-operation","action":"open","level":0,"requested_units":700,"created_ms":1000,
    "first":null,"hedge":null,"repair":null,"first_terminal":false,"hedge_terminal":false,
    "repair_terminal":false,"first_filled":0,"hedge_filled":0,"repair_filled":0,
    "first_value":"0","hedge_value":"0","failed":false,"first_venue":"entropy"
})).unwrap());}

#[test]
fn transient_failures_are_silent_and_sustained_alerts_do_not_spam() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        let mut db=db();let mut c=InventoryConfig::default();c.market=market;
        let mut s=Snapshot::new(c).unwrap();pending(&mut s);
        s.status=Status::RecoveringExposure;s.reason="secret-DO-NOT-SEND https://private.example/token".into();
        record(&mut db,&s,1000);
        for at in 1001..1050 {record(&mut db,&s,at);}
        assert!(rows(&db).is_empty());
        s.status=Status::NeedsAttention;record(&mut db,&s,1051);
        record(&mut db,&s,60_999);assert!(rows(&db).is_empty());
        // The independent timer still matures the alert if all actor commits stop.
        {let tx=db.transaction().unwrap();tick(&tx,61_000).unwrap();tx.commit().unwrap();}
        let first=rows(&db);assert_eq!(first.len(),1);assert!(first[0].1.contains("开仓"));
        assert!(!first[0].1.contains("secret-DO-NOT-SEND"));assert!(!first[0].1.contains("private.example"));
        record(&mut db,&s,900_000);assert_eq!(rows(&db).len(),1); // offline queue coalesces
        // The persisted cursor retains its deduplication state.
        let cursor:String=db.query_row("SELECT body FROM notification_state",[],|r|r.get(0)).unwrap();
        let _:Option<Incident>=serde_json::from_str(&cursor).unwrap();
        let id=first[0].0.clone();
        {let tx=db.transaction().unwrap();tx.execute("DELETE FROM notification_outbox WHERE id=?1",[&id]).unwrap();delivered(&tx,&id,900_000).unwrap();tx.commit().unwrap();}
        record(&mut db,&s,1_199_999);assert!(rows(&db).is_empty());
        record(&mut db,&s,1_200_000);assert_eq!(rows(&db).len(),1);
        s.pending=None;s.status=Status::Stopped;record(&mut db,&s,1_200_001);
        record(&mut db,&s,1_500_000);assert!(rows(&db).is_empty()); // cancel reminder; no success message
    }
}

#[test]
fn grace_period_and_recovery_cancellation_survive_actual_database_reopen() {
    let path=std::env::temp_dir().join(format!("cpa-alert-reopen-{}.sqlite",uuid::Uuid::new_v4()));
    let mut s=Snapshot::new(InventoryConfig::default()).unwrap();pending(&mut s);
    s.pending.as_mut().unwrap().action=Action::Close;s.status=Status::RecoveringExposure;
    {
        let mut db=Connection::open(&path).unwrap();db.execute_batch(SCHEMA).unwrap();
        record(&mut db,&s,1000);assert!(rows(&db).is_empty());
    }
    {
        let mut db=Connection::open(&path).unwrap();
        {let tx=db.transaction().unwrap();tick(&tx,60_999).unwrap();tx.commit().unwrap();}
        assert!(rows(&db).is_empty());
        {let tx=db.transaction().unwrap();tick(&tx,61_000).unwrap();tx.commit().unwrap();}
        assert_eq!(rows(&db).len(),1);assert!(rows(&db)[0].1.contains("平仓"));
    }
    {
        let mut db=Connection::open(&path).unwrap();
        {let tx=db.transaction().unwrap();tick(&tx,900_000).unwrap();tx.commit().unwrap();}
        assert_eq!(rows(&db).len(),1);
        s.pending=None;s.status=Status::Stopped;record(&mut db,&s,900_001);
    }
    {
        let mut db=Connection::open(&path).unwrap();
        {let tx=db.transaction().unwrap();tick(&tx,1_200_000).unwrap();tx.commit().unwrap();}
        assert!(rows(&db).is_empty());
    }
    std::fs::remove_file(path).unwrap();
}
#[test]
fn ordinary_first_leg_does_not_alert_but_partial_fill_does_and_unbalanced_state_never_recovers() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();pending(&mut s);s.status=Status::Running;
    record(&mut db,&s,1000);assert!(rows(&db).is_empty());
    let p=s.pending.as_mut().unwrap();p.first_terminal=true;p.first_filled=60;
    p.first=Some(OrderRequest{id:"synthetic".into(),venue:Venue::Entropy,side:Side::Buy,units:70,limit:2000.into(),arrival_mid:None,reduce_only:false,created_ms:1000,expires_ms:6000,signed_expires_ms:None});
    record(&mut db,&s,2000);assert!(rows(&db).is_empty());
    s.pending=None;s.positions[1].units=60;
    record(&mut db,&s,3000);assert!(rows(&db).is_empty());
    {let tx=db.transaction().unwrap();tick(&tx,62_000).unwrap();tx.commit().unwrap();}
    assert_eq!(rows(&db).len(),1);
}

#[test]
fn rapid_recovery_new_operation_and_legacy_queue_do_not_leak_stale_notifications() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();pending(&mut s);
    s.pending.as_mut().unwrap().failed=true;
    record(&mut db,&s,1000);
    s.pending=None;s.status=Status::Warming;s.reason.clear();
    record(&mut db,&s,60_999);
    {let tx=db.transaction().unwrap();tick(&tx,100_000).unwrap();tx.commit().unwrap();}
    assert!(rows(&db).is_empty());
    pending(&mut s);s.pending.as_mut().unwrap().id="new-operation".into();s.status=Status::RecoveringExposure;
    record(&mut db,&s,200_000);record(&mut db,&s,259_999);assert!(rows(&db).is_empty());
    record(&mut db,&s,260_000);assert_eq!(rows(&db).len(),1);
    // Simulate an old release's immediate alert followed by recovery while offline.
    db.execute("UPDATE notification_policy SET version=0",[]).unwrap();
    db.execute("DELETE FROM notification_outbox",[]).unwrap();
    let old=r#"{"key":"old","action":"开仓","started_ms":1000,"notice_ms":1000,"urgent":true}"#;
    db.execute("UPDATE notification_state SET body=?1",[old]).unwrap();
    {let tx=db.transaction().unwrap();enqueue(&tx,1,"【多平台套利 · 交易异常】\nold").unwrap();enqueue(&tx,2,"【多平台套利 · 账本配平结果】\nold recovery").unwrap();tx.commit().unwrap();}
    s.pending=None;s.status=Status::Stopped;record(&mut db,&s,300_000);assert!(rows(&db).is_empty());
}

#[test]
fn emergency_account_fault_still_alerts_immediately_and_transaction_rollback_preserves_cursor() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();s.status=Status::NeedsAttention;
    record(&mut db,&s,1000);assert_eq!(rows(&db).len(),1);
    let id=rows(&db)[0].0.clone();
    s.status=Status::Stopped;
    {let tx=db.transaction().unwrap();observe(&tx,&s,2000).unwrap();}
    assert_eq!(rows(&db)[0].0,id);
    record(&mut db,&s,2000);assert!(rows(&db).is_empty());
}

#[test]
fn balanced_manual_review_alerts_on_upgrade_and_reminds_until_operator_resumes() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        for failures in [1,3] {
            let mut db=db();let mut c=InventoryConfig::default();c.market=market;
            let mut s=Snapshot::new(c).unwrap();
            s.status=Status::NeedsAttention;s.positions[0].units=-700;s.positions[1].units=700;
            s.consecutive_rollbacks=failures;
            s.reason="execution recovered; review before resume (repeated failure or non-entry recovery)".into();
            // The old policy already initialized its cursor to null. Loading
            // the same halted snapshot on upgrade must still enqueue a notice.
            db.execute("UPDATE notification_policy SET version=2",[]).unwrap();
            record(&mut db,&s,1000);
            let (id,body)=rows(&db).pop().unwrap();
            assert!(body.contains("需要处理"));assert!(body.contains("人工检查后恢复"));
            assert!(body.contains(&market.id().to_uppercase()));
            if failures==3 {assert!(body.contains("连续回滚"));}
            record(&mut db,&s,2000);assert_eq!(rows(&db).len(),1);
            {let tx=db.transaction().unwrap();tx.execute("DELETE FROM notification_outbox WHERE id=?1",[&id]).unwrap();delivered(&tx,&id,2000).unwrap();tx.commit().unwrap();}
            record(&mut db,&s,301_999);assert!(rows(&db).is_empty());
            {let tx=db.transaction().unwrap();tick(&tx,302_000).unwrap();tx.commit().unwrap();}
            assert_eq!(rows(&db).len(),1);
            s.status=Status::Warming;s.reason.clear();s.consecutive_rollbacks=0;
            record(&mut db,&s,302_001);assert!(rows(&db).is_empty());
        }
    }
}

#[test]
fn automatic_rollback_cooldown_is_quiet_only_if_it_actually_recovers() {
    for resumes in [true,false] {
        let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();
        s.status=Status::NeedsAttention;s.reason="execution recovered; cooling down before verified resume".into();
        s.consecutive_rollbacks=1;s.recovery_after_ms=Some(31_000);
        record(&mut db,&s,1000);assert!(rows(&db).is_empty());
        if resumes {s.status=Status::Warming;s.recovery_after_ms=None;s.reason.clear();}
        record(&mut db,&s,31_000);assert!(rows(&db).is_empty());
        {let tx=db.transaction().unwrap();tick(&tx,61_000).unwrap();tx.commit().unwrap();}
        assert_eq!(rows(&db).len(),usize::from(!resumes));
    }
}
#[test]
fn alerts_roll_back_with_trade_state_and_queue_is_bounded() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();s.status=Status::NeedsAttention;
    {let tx=db.transaction().unwrap();observe(&tx,&s,1000).unwrap();}
    assert!(rows(&db).is_empty());
    {let tx=db.transaction().unwrap();for at in 0..520{enqueue(&tx,at,"synthetic").unwrap();}tx.commit().unwrap();}
    assert_eq!(rows(&db).len(),512);
    let dropped:i64=db.query_row("SELECT dropped FROM notification_counters",[],|r|r.get(0)).unwrap();assert_eq!(dropped,8);
}

#[test]
fn emergency_exit_alert_waits_for_grace_and_cancels_when_flat_and_reconciled() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();
    super::super::emergency_exit::latch(&mut s,1000).unwrap();
    record(&mut db,&s,1000);record(&mut db,&s,60_999);assert!(rows(&db).is_empty());
    record(&mut db,&s,61_000);assert_eq!(rows(&db).len(),1);assert!(rows(&db)[0].1.contains("紧急全部平仓"));
    s.emergency_exit.as_mut().unwrap().completed_ms=Some(61_001);s.status=Status::Stopped;s.close_requested=false;
    record(&mut db,&s,61_001);assert!(rows(&db).is_empty());
}

#[test]
fn market_recovery_during_http_post_still_rate_limits_confirmed_delivery() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();s.status=Status::Running;
    for at in (10_000..=70_000).step_by(1000) {market_record(&mut db,&s,at,false);}
    assert_eq!(rows(&db).len(),1);
    market_record(&mut db,&s,70_001,true);assert!(rows(&db).is_empty());
    // The platform accepted the message before the cancellation was observed.
    {let tx=db.transaction().unwrap();market_send_confirmed(&tx,70_002).unwrap();tx.commit().unwrap();}
    for at in (71_000..370_000).step_by(1000) {market_record(&mut db,&s,at,false);assert!(rows(&db).is_empty());}
    market_record(&mut db,&s,370_002,false);assert_eq!(rows(&db).len(),1);
}
