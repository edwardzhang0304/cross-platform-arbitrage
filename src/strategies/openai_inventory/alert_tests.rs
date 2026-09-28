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
fn outage_partial_and_recovery_alerts_are_durable_deduplicated_and_redacted() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        let mut db=db();let mut c=InventoryConfig::default();c.market=market;
        let mut s=Snapshot::new(c).unwrap();pending(&mut s);
        s.status=Status::RecoveringExposure;s.reason="secret-DO-NOT-SEND https://private.example/token".into();
        record(&mut db,&s,1000);
        for at in 1001..1050 {record(&mut db,&s,at);}
        let first=rows(&db);assert_eq!(first.len(),1);assert!(first[0].1.contains("开仓"));
        assert!(!first[0].1.contains("secret-DO-NOT-SEND"));assert!(!first[0].1.contains("private.example"));
        s.status=Status::NeedsAttention;record(&mut db,&s,1051);assert_eq!(rows(&db).len(),2);
        record(&mut db,&s,2000);assert_eq!(rows(&db).len(),2);
        record(&mut db,&s,302_000);assert_eq!(rows(&db).len(),3);
        // Serialization/reopen of cursor retains its deduplication state.
        let cursor:String=db.query_row("SELECT body FROM notification_state",[],|r|r.get(0)).unwrap();
        let _:Option<Incident>=serde_json::from_str(&cursor).unwrap();
        s.pending=None;s.status=Status::Stopped;record(&mut db,&s,303_000);
        record(&mut db,&s,304_000);let out=rows(&db);assert_eq!(out.len(),4);
        assert!(out[3].1.contains("账本配平结果"));assert_ne!(out[0].0,out[3].0);
    }
}
#[test]
fn ordinary_first_leg_does_not_alert_but_partial_fill_does_and_unbalanced_state_never_recovers() {
    let mut db=db();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();pending(&mut s);s.status=Status::Running;
    record(&mut db,&s,1000);assert!(rows(&db).is_empty());
    let p=s.pending.as_mut().unwrap();p.first_terminal=true;p.first_filled=60;
    p.first=Some(OrderRequest{id:"synthetic".into(),venue:Venue::Entropy,side:Side::Buy,units:70,limit:2000.into(),arrival_mid:None,reduce_only:false,created_ms:1000,expires_ms:6000,signed_expires_ms:None});
    record(&mut db,&s,2000);assert_eq!(rows(&db).len(),1);
    s.pending=None;s.positions[1].units=60;
    record(&mut db,&s,3000);assert_eq!(rows(&db).len(),1);
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
