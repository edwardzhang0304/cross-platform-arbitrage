use super::*;
use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::{Role, CloseFrame, frame::coding::CloseCode}};

fn book_frame(snapshot: bool, nonce: i64, previous: i64) -> Message {
    Message::Text(json!({"type":if snapshot {"subscribed/order_book"} else {"update/order_book"},
        "timestamp":1000,"order_book":{"asks":[{"price":"101","size":"2"}],
        "bids":[{"price":"99","size":"2"}],"nonce":nonce,"begin_nonce":previous,
        "last_updated_at":1000000}}).to_string())
}

#[tokio::test(start_paused = true)]
async fn lighter_keepalive_survives_idle_windows_without_refreshing_market_evidence() {
    // Each reconnection must subscribe again and require its own nonce snapshot.
    for round in 0..2 {
        let (a,b) = tokio::io::duplex(8192);
        let client = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        let (tx,mut rx) = mpsc::channel(64);
        let last_book = Arc::new(AtomicU64::new(0));
        let shared = last_book.clone();
        let task = tokio::spawn(async move {
            read_lighter_session(client,42,PriceSource::Unknown,&MarketStreamConfig::default(),&tx,&shared,&AtomicU64::new(0)).await
        });
        server.send(Message::Text(json!({"type":"connected"}).to_string())).await.unwrap();
        for channel in ["order_book/42","market_stats/42"] {
            let Message::Text(text) = server.next().await.unwrap().unwrap() else {panic!("subscription missing")};
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(),json!({"type":"subscribe","channel":channel}));
        }
        assert!(matches!(rx.recv().await.unwrap(),MarketDataEvent::Connected{..}));
        if round == 1 {
            server.send(book_frame(false, 11, 10)).await.unwrap();
            assert!(task.await.unwrap().unwrap_err().to_string().contains("before snapshot"));
            assert_eq!(last_book.load(Ordering::Relaxed),0);
            continue;
        }
        server.send(book_frame(true,10,1)).await.unwrap();
        assert!(matches!(rx.recv().await.unwrap(),MarketDataEvent::Book{..}));
        let original = last_book.load(Ordering::Relaxed);
        assert!(original > 0);
        // Busy transport, but no new book: neither outbound pings nor incoming
        // pongs may fabricate prices, reset freshness, or emit book events.
        for second in (10..=300).step_by(10) {
            tokio::time::advance(Duration::from_secs(10)).await;
            server.send(Message::Text(json!({"type":"pong"}).to_string())).await.unwrap();
            if second % 30 == 0 {
                assert_eq!(server.next().await.unwrap().unwrap(),Message::Text(json!({"type":"ping"}).to_string()));
            }
            tokio::task::yield_now().await;
            assert!(rx.try_recv().is_err());
            assert_eq!(last_book.load(Ordering::Relaxed),original);
        }
        // Compatibility with server-initiated application and protocol ping.
        server.send(Message::Text(json!({"type":"ping"}).to_string())).await.unwrap();
        assert_eq!(server.next().await.unwrap().unwrap(),Message::Text(json!({"type":"pong"}).to_string()));
        server.send(Message::Ping(vec![1,2,3])).await.unwrap();
        assert_eq!(server.next().await.unwrap().unwrap(),Message::Pong(vec![1,2,3]));
        server.close(Some(CloseFrame {code:CloseCode::Normal, reason:"read tcp 10.0.0.1:8888: i/o timeout".into()})).await.unwrap();
        let error = task.await.unwrap().unwrap_err();
        let closed = error.downcast_ref::<LighterRemoteClosed>().unwrap();
        assert_eq!((closed.close_code,closed.pings_sent,closed.pongs_received,closed.cause),(Some(1000),10,30,"read_timeout"));
        let detail = safe_transport_error(&closed.to_string());
        assert!(detail.contains("pings= 10 pongs= 30 cause= read_timeout"));
        assert!(!detail.contains("10.0.0.1"));
    }
}

#[tokio::test]
async fn lighter_keepalive_stops_with_its_last_consumer() {
    let (a,_peer) = tokio::io::duplex(4096);
    let client=WebSocketStream::from_raw_socket(a,Role::Client,None).await;
    let (tx,rx)=mpsc::channel(8);
    let task=tokio::spawn(async move {
        read_lighter_session(client,42,PriceSource::Unknown,&MarketStreamConfig::default(),&tx,
            &AtomicU64::new(0),&AtomicU64::new(0)).await
    });
    drop(rx);
    tokio::time::timeout(Duration::from_secs(1),task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
#[ignore = "Public read-only 300-second network probe; no credentials, strategies or account workers"]
async fn lighter_public_keepalive_smoke() {
    let endpoints=LighterEndpoints::official(LighterEnvironment::Robinhood);
    let (tx,mut rx)=mpsc::channel(256);
    let task=tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(300),run_lighter_session(&endpoints,42,
            PriceSource::Unknown,&MarketStreamConfig::default(),&tx,&AtomicU64::new(0),&AtomicU64::new(0))).await
    });
    let mut books=0;let mut connections=0;
    while let Some(event)=rx.recv().await {
        match event { MarketDataEvent::Book{..}=>books+=1,MarketDataEvent::Connected{..}=>connections+=1,_=>{} }
    }
    let result=task.await.unwrap();
    assert!(result.is_err(),"session ended before 300 seconds: {result:?}");
    assert_eq!(connections,1);
    assert!(books>100);
    println!("LIGHTER_KEEPALIVE_PROBE {}",json!({"readonly":true,"duration_s":300,"connections":connections,"book_messages":books,"strategy_started":false}));
}
