use super::*;
use axum::{Json,Router,extract::Query,routing::post};
use serde_json::{Value,json};
use std::sync::{Mutex,atomic::{AtomicBool,AtomicUsize,Ordering}};

fn config()->FeishuSettings {FeishuSettings{app_id:"cli_synthetic_test".into(),app_secret:"synthetic-secret-for-test-only".into(),receive_id_type:"open_id".into(),receive_id:"ou_synthetic_person".into()}}
fn directory()->PathBuf {std::env::temp_dir().join(format!("cpa-notification-test-{}",uuid::Uuid::new_v4()))}

#[tokio::test]
async fn rc10_readable_empty_queue_clears_prior_database_error_without_sending() {
    let dir=directory();std::fs::create_dir_all(&dir).unwrap();let path=dir.join("ledger.sqlite");
    let db=rusqlite::Connection::open(&path).unwrap();db.execute_batch(alerts::SCHEMA).unwrap();
    let handle=NotificationHandle::default();handle.configure(Some(config()));
    handle.status.write().unwrap().error=Some("通知队列暂不可读，将重试；请检查磁盘和文件权限".into());
    let mut sender=FeishuSender::new().unwrap();
    handle.deliver_once(&path,&mut sender).await.unwrap();
    assert!(handle.status().error.is_none());
    assert!(handle.status().last_sent_ms.is_none());assert_eq!(handle.status().pending,0);
    drop(db);std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn rc10_readable_queue_does_not_clear_backing_off_send_error() {
    let dir=directory();std::fs::create_dir_all(&dir).unwrap();let path=dir.join("ledger.sqlite");
    let mut db=rusqlite::Connection::open(&path).unwrap();db.execute_batch(alerts::SCHEMA).unwrap();
    {let tx=db.transaction().unwrap();alerts::enqueue(&tx,1,"synthetic delivery retry").unwrap();tx.commit().unwrap();}
    db.execute("UPDATE notification_outbox SET next_ms=?1",[crate::domain::now_ms()+60_000]).unwrap();
    let handle=NotificationHandle::default();handle.configure(Some(config()));
    handle.status.write().unwrap().error=Some("synthetic send failure still backing off".into());
    let mut sender=FeishuSender::new().unwrap();handle.deliver_once(&path,&mut sender).await.unwrap();
    assert_eq!(handle.status().error.as_deref(),Some("synthetic send failure still backing off"));
    assert_eq!(handle.status().pending,1);assert!(handle.status().last_sent_ms.is_none());
    drop(db);std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn public_config_never_contains_secret_and_destinations_are_typed() {
    let mut c=config();assert!(c.validate().is_ok());
    assert!(!c.public().to_string().contains(&c.app_secret));
    c.receive_id="https://untrusted.invalid/collect".into();assert!(c.validate().is_err());
    c.receive_id="oc_synthetic_room".into();assert!(c.validate().is_err());
    c.receive_id_type="chat_id".into();assert!(c.validate().is_ok());
    assert_eq!([0,1,2,10].map(retry_delay_ms),[5000,15000,60000,300000]);
}

#[test]
fn full_disk_alarm_needs_no_database_and_is_bounded_even_under_repeated_errors() {
    let handle=NotificationHandle::default();
    handle.report_persistence_failure(crate::openai_inventory::MarketPair::Anth,crate::openai_inventory::Mode::Live);
    let original=handle.emergency.read().unwrap().clone().unwrap();
    for _ in 0..100{handle.report_persistence_failure(crate::openai_inventory::MarketPair::Anth,crate::openai_inventory::Mode::Live);}
    let after=handle.emergency.read().unwrap().clone().unwrap();
    assert_eq!(original.id,after.id);assert!(after.body.contains("账本写入失败"));
    assert!(!handle.status().enabled);
}

#[test]
fn notification_vault_is_encrypted_and_cannot_be_used_as_trading_vault() {
    let dir=directory();let path=dir.join("feishu.vault");let c=config();let password="synthetic-long-password";
    crate::secrets::save_notification_settings(&path,password,&c).unwrap();
    let raw=std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(&c.app_secret));assert!(!raw.contains(password));assert!(!raw.contains(&c.receive_id));
    let restored=crate::secrets::load_notification_settings(&path,password).unwrap();
    assert_eq!(restored.app_secret,c.app_secret);assert_eq!(restored.receive_id,c.receive_id);
    assert!(crate::secrets::load_notification_settings(&path,"wrong-long-password").is_err());
    assert!(crate::secrets::unlock_vault(&path,password).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn failed_delivery_retries_same_uuid_and_recovers_after_reopening_queue() {
    let received=Arc::new(Mutex::new(Vec::<Value>::new()));let fail=Arc::new(AtomicBool::new(true));
    let tokens=Arc::new(AtomicUsize::new(0));
    let router=Router::new().route("/auth/v3/tenant_access_token/internal",post({let tokens=tokens.clone();move|Json(v):Json<Value>|{let tokens=tokens.clone();async move {
        assert_eq!(v["app_id"],"cli_synthetic_test");tokens.fetch_add(1,Ordering::SeqCst);
        Json(json!({"code":0,"tenant_access_token":"synthetic-token","expire":7200}))
    }}})).route("/im/v1/messages",post({let received=received.clone();let fail=fail.clone();move|h:axum::http::HeaderMap,Query(q):Query<std::collections::HashMap<String,String>>,Json(v):Json<Value>|{let received=received.clone();let fail=fail.clone();async move {
        assert_eq!(q.get("receive_id_type").map(String::as_str),Some("open_id"));
        assert_eq!(h["authorization"],"Bearer synthetic-token");received.lock().unwrap().push(v);
        Json(json!({"code":if fail.load(Ordering::SeqCst){999}else{0}}))
    }}}));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let task=tokio::spawn(async move{axum::serve(listener,router).await.unwrap()});
    let mut sender=FeishuSender::new().unwrap();sender.base=format!("http://{address}");
    let dir=directory();std::fs::create_dir_all(&dir).unwrap();let path=dir.join("ledger.sqlite");
    let mut db=rusqlite::Connection::open(&path).unwrap();db.execute_batch(alerts::SCHEMA).unwrap();
    let id={let tx=db.transaction().unwrap();let id=alerts::enqueue(&tx,1,"synthetic exception").unwrap();tx.commit().unwrap();id};drop(db);
    let handle=NotificationHandle::default();handle.configure(Some(config()));
    handle.deliver_once(&path,&mut sender).await.unwrap();
    assert_eq!(handle.status().pending,1);assert!(handle.status().error.is_some());
    {let db=alerts::open_delivery(&path).unwrap();let attempts:u32=db.query_row("SELECT attempts FROM notification_outbox",[],|r|r.get(0)).unwrap();assert_eq!(attempts,1);db.execute("UPDATE notification_outbox SET next_ms=0",[]).unwrap();}
    fail.store(false,Ordering::SeqCst);
    // The next attempt reopens the durable queue, including its original UUID.
    handle.deliver_once(&path,&mut sender).await.unwrap();
    assert_eq!(handle.status().pending,0);assert!(handle.status().last_sent_ms.is_some());assert!(handle.status().error.is_none());
    let records=received.lock().unwrap();assert_eq!(records.len(),2);assert_eq!(records[0]["uuid"],id);assert_eq!(records[1]["uuid"],id);
    assert_eq!(records[1]["receive_id"],"ou_synthetic_person");assert_eq!(records[1]["msg_type"],"text");
    assert!(records[1]["content"].is_string());drop(records);
    assert_eq!(tokens.load(Ordering::SeqCst),2);
    task.abort();std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn stalled_notification_cannot_hold_the_trading_database_lock() {
    let entered=Arc::new(tokio::sync::Notify::new());let seen=entered.clone();
    let router=Router::new().route("/auth/v3/tenant_access_token/internal",post(move||{let seen=seen.clone();async move{
        seen.notify_one();std::future::pending::<Json<Value>>().await
    }}));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let server=tokio::spawn(async move{axum::serve(listener,router).await.unwrap()});
    let dir=directory();std::fs::create_dir_all(&dir).unwrap();let path=dir.join("ledger.sqlite");
    let mut db=rusqlite::Connection::open(&path).unwrap();db.execute_batch(alerts::SCHEMA).unwrap();
    {let tx=db.transaction().unwrap();alerts::enqueue(&tx,1,"timeout fixture").unwrap();tx.commit().unwrap();}drop(db);
    let handle=NotificationHandle::default();handle.configure(Some(config()));
    let work=tokio::spawn({let path=path.clone();let handle=handle.clone();async move{
        let mut sender=FeishuSender::new().unwrap();sender.base=format!("http://{address}");
        sender.client=reqwest::Client::builder().timeout(Duration::from_millis(300)).build().unwrap();
        handle.deliver_once(&path,&mut sender).await.unwrap();
    }});
    tokio::time::timeout(Duration::from_secs(2),entered.notified()).await.unwrap();
    {let mut db=alerts::open_delivery(&path).unwrap();let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();alerts::enqueue(&tx,2,"second exception while HTTP stalled").unwrap();tx.commit().unwrap();}
    work.await.unwrap();assert!(handle.status().error.is_some());
    server.abort();std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn recovery_while_token_is_loading_cancels_the_message_before_post() {
    let entered=Arc::new(tokio::sync::Notify::new());
    let release=Arc::new(tokio::sync::Notify::new());
    let sent=Arc::new(AtomicUsize::new(0));
    let router=Router::new().route("/auth/v3/tenant_access_token/internal",post({
        let entered=entered.clone();let release=release.clone();move||{let entered=entered.clone();let release=release.clone();async move {
            entered.notify_one();release.notified().await;
            Json(json!({"code":0,"tenant_access_token":"synthetic-token","expire":7200}))
        }}
    })).route("/im/v1/messages",post({let sent=sent.clone();move||{let sent=sent.clone();async move {
        sent.fetch_add(1,Ordering::SeqCst);Json(json!({"code":0}))
    }}}));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let server=tokio::spawn(async move{axum::serve(listener,router).await.unwrap()});
    let dir=directory();std::fs::create_dir_all(&dir).unwrap();let path=dir.join("ledger.sqlite");
    let mut db=rusqlite::Connection::open(&path).unwrap();db.execute_batch(alerts::SCHEMA).unwrap();
    {let tx=db.transaction().unwrap();alerts::enqueue(&tx,1,"sustained incident").unwrap();tx.commit().unwrap();}drop(db);
    let handle=NotificationHandle::default();handle.configure(Some(config()));
    let work=tokio::spawn({let path=path.clone();let handle=handle.clone();async move {
        let mut sender=FeishuSender::new().unwrap();sender.base=format!("http://{address}");
        handle.deliver_once(&path,&mut sender).await.unwrap();
    }});
    tokio::time::timeout(Duration::from_secs(2),entered.notified()).await.unwrap();
    // Same durable cancellation used when observe() sees completed paired recovery.
    alerts::open_delivery(&path).unwrap().execute("DELETE FROM notification_outbox",[]).unwrap();
    release.notify_one();work.await.unwrap();
    assert_eq!(sent.load(Ordering::SeqCst),0);
    assert_eq!(handle.status().pending,0);assert!(handle.status().last_sent_ms.is_none());
    server.abort();std::fs::remove_dir_all(dir).unwrap();
}
