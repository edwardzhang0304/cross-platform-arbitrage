use super::*;
use crate::InfoClient;
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::{mpsc, oneshot}, time::timeout};
use tokio_tungstenite::accept_async;

async fn next_subscription(ws: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        let frame = ws.next().await.unwrap().unwrap();
        if let protocol::Message::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["method"] != "ping" { return value; }
        }
    }
}

fn book(time: u64) -> protocol::Message {
    protocol::Message::Text(json!({"channel":"l2Book", "data":{
        "coin":"io:OAI", "time":time,
        "levels":[[{"px":"1600","sz":"0.2","n":1}], [{"px":"1601","sz":"0.3","n":2}]]
    }}).to_string())
}

#[tokio::test]
async fn fast_book_routes_deduplicates_rejects_mixed_modes_and_unsubscribes() {
    timeout(Duration::from_secs(10), async {
        for fast in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (ready, received) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(socket).await.unwrap();
                let subscription = next_subscription(&mut ws).await;
                assert_eq!(subscription["method"], "subscribe");
                assert_eq!(subscription["subscription"]["type"], "l2Book");
                assert_eq!(subscription["subscription"]["coin"], "io:OAI");
                assert_eq!(subscription["subscription"]["fast"].as_bool(), fast.then_some(true));
                received.await.unwrap();
                ws.send(book(1)).await.unwrap();
                let unsubscribe = next_subscription(&mut ws).await;
                assert_eq!(unsubscribe["method"], "unsubscribe");
                assert_eq!(unsubscribe["subscription"], subscription["subscription"]);
            });
            let mut client = InfoClient::new(None, None).await.unwrap();
            client.http_client.base_url = format!("http://{address}");
            let (tx, mut rx) = mpsc::unbounded_channel();
            let first = if fast {
                client.subscribe_l2_book_fast("io:OAI".into(), tx.clone()).await
            } else {
                client.subscribe(Subscription::L2Book {coin:"io:OAI".into()}, tx.clone()).await
            }.unwrap();
            let second = if fast {
                client.subscribe_l2_book_fast("io:OAI".into(), tx.clone()).await
            } else {
                client.subscribe(Subscription::L2Book {coin:"io:OAI".into()}, tx.clone()).await
            }.unwrap();
            let conflict = if fast {
                client.subscribe(Subscription::L2Book {coin:"io:OAI".into()}, tx).await
            } else {
                client.subscribe_l2_book_fast("io:OAI".into(), tx).await
            };
            assert!(conflict.unwrap_err().to_string().contains("Conflicting L2 book modes"));
            ready.send(()).unwrap();
            for _ in 0..2 {
                assert!(matches!(rx.recv().await.unwrap(), Message::L2Book(b) if b.data.time==1));
            }
            client.unsubscribe(first).await.unwrap();
            client.unsubscribe(second).await.unwrap();
            server.await.unwrap();
        }
    }).await.unwrap();
}

#[tokio::test]
async fn fast_book_reconnect_replays_fast_wire_subscription() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for stamp in [1, 2] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(socket).await.unwrap();
                let subscription = next_subscription(&mut ws).await;
                assert_eq!(subscription["subscription"], json!({"type":"l2Book","coin":"io:OAI","fast":true}));
                ws.send(book(stamp)).await.unwrap();
                if stamp == 2 {
                    let unsubscribe = next_subscription(&mut ws).await;
                    assert_eq!(unsubscribe["method"], "unsubscribe");
                    assert_eq!(unsubscribe["subscription"], subscription["subscription"]);
                }
                // Dropping the first socket forces an actual reconnect.
            }
        });
        let mut client = InfoClient::with_reconnect(None, None).await.unwrap();
        client.http_client.base_url = format!("http://{address}");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = client.subscribe_l2_book_fast("io:OAI".into(), tx).await.unwrap();
        let mut times = vec![];
        while times.len() < 2 {
            if let Message::L2Book(b) = rx.recv().await.unwrap() {times.push(b.data.time);}
        }
        assert_eq!(times, vec![1,2]);
        client.unsubscribe(id).await.unwrap();
        server.await.unwrap();
    }).await.unwrap();
}
