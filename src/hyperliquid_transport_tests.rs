use super::*;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

#[tokio::test]
async fn saturated_routine_reads_do_not_starve_two_market_reconciliation_and_lookup() {
    let count=Arc::new(AtomicUsize::new(0));
    let counted=count.clone();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("http://{}/info",listener.local_addr().unwrap());
    let server=tokio::spawn(async move {
        axum::serve(listener,axum::Router::new().route("/info",axum::routing::post(move || {
            counted.fetch_add(1,Ordering::SeqCst);
            async { axum::Json(json!({"synthetic":true})) }
        }))).await.unwrap();
    });
    let mut window=InfoRateWindow::default();
    let now=crate::domain::now_ms();
    assert!(reserve_info_capacity(&mut window,now,300,false,false));
    INFO_RATE_WINDOWS.lock().unwrap().insert(url.clone(),window);
    let client=info_client().unwrap();
    let blocked=tokio::time::timeout(Duration::from_millis(100),trading_info(false,
        post_info::<Value>(&client,&url,json!({"type":"activeAssetData"})))).await.unwrap().unwrap_err();
    assert!(format!("{blocked:#}").contains("local info rate budget exhausted"));
    assert_eq!(count.load(Ordering::SeqCst),0,"denied read must never reach HTTP");
    // Reproduce concurrent OPENAI + ANTH: 4 account reads then status/fills.
    for _ in 0..2 {
        trading_info(true,async {
            let read=|kind| post_info::<Value>(&client,&url,json!({"type":kind}));
            tokio::try_join!(read("clearinghouseState"),read("openOrders"),
                read("activeAssetData"),read("spotClearinghouseState")).unwrap();
            read("orderStatus").await.unwrap();
            read("userFillsByTime").await.unwrap();
        }).await;
    }
    assert_eq!(count.load(Ordering::SeqCst),12);
    let mut windows=INFO_RATE_WINDOWS.lock().unwrap();
    let w=windows.get_mut(&url).unwrap();
    assert_eq!(w.used_weight,432);
    assert_eq!(w.routine_weight,300);
    assert!(reserve_info_capacity(w,now,468,false,true));
    assert!(!reserve_info_capacity(w,now,2,false,true));
    drop(windows);
    let blocked=trading_info(true,post_info::<Value>(&client,&url,json!({"type":"orderStatus"}))).await.unwrap_err();
    assert!(format!("{blocked:#}").contains("local info rate budget exhausted"));
    assert_eq!(count.load(Ordering::SeqCst),12);
    INFO_RATE_WINDOWS.lock().unwrap().remove(&url);
    server.abort();
}

#[tokio::test]
async fn trading_429_returns_once_and_obeys_server_cooldown_without_charging_again() {
    let count=Arc::new(AtomicUsize::new(0));
    let counted=count.clone();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("http://{}/info",listener.local_addr().unwrap());
    let server=tokio::spawn(async move {
        axum::serve(listener,axum::Router::new().route("/info",axum::routing::post(move || {
            counted.fetch_add(1,Ordering::SeqCst);
            async { (axum::http::StatusCode::TOO_MANY_REQUESTS,[("retry-after","45")],"synthetic throttle") }
        }))).await.unwrap();
    });
    let client=info_client().unwrap();
    let call=||trading_info(true,post_info::<Value>(&client,&url,json!({"type":"orderStatus"})));
    let first=tokio::time::timeout(Duration::from_secs(1),call()).await.unwrap().unwrap_err();
    assert!(format!("{first:#}").contains("429"));
    let second=call().await.unwrap_err();
    assert!(format!("{second:#}").contains("server cooldown"));
    assert_eq!(count.load(Ordering::SeqCst),1);
    assert_eq!(INFO_RATE_WINDOWS.lock().unwrap().remove(&url).unwrap().used_weight,2);
    assert!(INFO_COOLDOWN_UNTIL_MS.lock().unwrap().remove(&url).unwrap() >= crate::domain::now_ms()+40_000);
    server.abort();
}

#[tokio::test]
async fn nested_account_scope_preserves_recovery_priority_and_cancellation_restores_it() {
    assert!(TRADING_INFO.try_with(|v|*v).is_err());
    trading_info(true,async {
        trading_info(false,async { assert!(TRADING_INFO.with(|v|*v)); }).await;
    }).await;
    let result=tokio::time::timeout(Duration::from_millis(1),trading_info(true,std::future::pending::<()>())).await;
    assert!(result.is_err());
    assert!(TRADING_INFO.try_with(|v|*v).is_err());
}

#[tokio::test]
async fn slow_trading_http_reports_the_query_before_worker_deadline_without_hidden_retry() {
    let count=Arc::new(AtomicUsize::new(0));let counted=count.clone();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("http://{}/info",listener.local_addr().unwrap());
    let server=tokio::spawn(async move {
        axum::serve(listener,axum::Router::new().route("/info",axum::routing::post(move || {
            counted.fetch_add(1,Ordering::SeqCst);
            async { tokio::time::sleep(Duration::from_secs(10)).await; axum::Json(json!({})) }
        }))).await.unwrap();
    });
    let result=tokio::time::timeout(Duration::from_millis(3000),trading_info(true,
        post_info::<Value>(&info_client().unwrap(),&url,json!({"type":"orderStatus"})))).await.unwrap().unwrap_err();
    let reason=format!("{result:#}");
    assert!(reason.contains("orderStatus") && reason.contains("HTTP request failed"));
    assert_eq!(count.load(Ordering::SeqCst),1);
    INFO_RATE_WINDOWS.lock().unwrap().remove(&url);server.abort();
}

#[test]
fn response_weight_and_rolling_window_do_not_erase_critical_debt() {
    assert_eq!(response_extra_weight("userFillsByTime",&json!(vec![0;2000])),100);
    assert_eq!(response_extra_weight("userFunding",&json!(vec![0;500])),25);
    let mut w=InfoRateWindow::default();
    assert!(reserve_info_capacity(&mut w,1000,300,false,false));
    assert!(reserve_info_capacity(&mut w,2000,600,false,true));
    assert!(!reserve_info_capacity(&mut w,60999,1,false,true));
    assert!(reserve_info_capacity(&mut w,61000,300,false,false));
    assert_eq!(w.used_weight,900);
    prune_info_rate_window(&mut w,62000);
    assert_eq!(w.used_weight,300);
    assert_eq!(w.routine_weight,300);
}
