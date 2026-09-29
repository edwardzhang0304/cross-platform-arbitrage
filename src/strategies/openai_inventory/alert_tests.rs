use super::*;

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
    s.pending=None;s.status=Status::NeedsAttention;s.reason="execution recovered; review before resume".into();
    record(&mut db,&s,60_999);
    {let tx=db.transaction().unwrap();tick(&tx,100_000).unwrap();tx.commit().unwrap();}
    assert!(rows(&db).is_empty());
    pending(&mut s);s.pending.as_mut().unwrap().id="new-operation".into();
    record(&mut db,&s,200_000);record(&mut db,&s,259_999);assert!(rows(&db).is_empty());
    record(&mut db,&s,260_000);assert_eq!(rows(&db).len(),1);
    // Simulate an old release's immediate alert followed by recovery while offline.
    db.execute("UPDATE notification_policy SET version=0",[]).unwrap();
    db.execute("DELETE FROM notification_outbox",[]).unwrap();
    let old=r#"{"key":"old","action":"开仓","started_ms":1000,"notice_ms":1000,"urgent":true}"#;
    db.execute("UPDATE notification_state SET body=?1",[old]).unwrap();
    {let tx=db.transaction().unwrap();enqueue(&tx,1,"【多平台套利 · 交易异常】\nold").unwrap();enqueue(&tx,2,"【多平台套利 · 账本配平结果】\nold recovery").unwrap();tx.commit().unwrap();}
    s.pending=None;record(&mut db,&s,300_000);assert!(rows(&db).is_empty());
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
fn alerts_roll_back_with_trade_state_and_queue_is_bounded() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();s.status=Status::NeedsAttention;
    {let tx=db.transaction().unwrap();observe(&tx,&s,1000).unwrap();}
    assert!(rows(&db).is_empty());
    {let tx=db.transaction().unwrap();for at in 0..520{enqueue(&tx,at,"synthetic").unwrap();}tx.commit().unwrap();}
    assert_eq!(rows(&db).len(),512);
    let dropped:i64=db.query_row("SELECT dropped FROM notification_counters",[],|r|r.get(0)).unwrap();assert_eq!(dropped,8);
}
