//! Inventory-only public feed. WS recovery and rate-limited REST recovery are
//! independent; neither can stamp cached prices as fresh or change trading gates.
use super::*;
use std::{collections::VecDeque, sync::Mutex};

const FALLBACK_AFTER_MS: u64 = 1_500;
const FALLBACK_INTERVAL_MS: u64 = 1_000;
const REST_DEADLINE_MS: u64 = 1_250;
const MAX_SOURCE_AGE_MS: u64 = 1_500;
const HISTORY_LIMIT: usize = 32;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MarketStreamHealth {
    pub source: String,
    pub ws_connected: bool,
    pub reconnect_count: u64,
    pub ws_books: u64,
    pub rest_books: u64,
    pub rest_failures: u64,
    pub last_ws_book_ms: u64,
    pub last_rest_book_ms: u64,
    pub last_error_ms: u64,
    pub last_error: String,
    pub history: VecDeque<TransportIncident>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransportIncident {
    pub at_ms: u64,
    pub kind: String,
    pub detail: String,
}

/// Public transports never receive credentials. Still bound/redact remote error
/// text before it is exposed by the local API or persisted in a diagnostic row.
pub fn safe_transport_error(value: &str) -> String {
    let mut redact_next = false;
    value.split_whitespace().take(64).map(|word| {
        let lower = word.to_ascii_lowercase();
        let redact = redact_next || lower.contains("://") || lower.contains("0x")
            || lower.contains("token") || lower.contains("secret") || lower.contains("key=")
            || lower.contains('@') || word.len() > 80;
        redact_next = lower == "bearer";
        if redact { "[omitted]".to_string() } else { word.chars().take(80).collect() }
    }).collect::<Vec<_>>().join(" ").chars().take(480).collect()
}
impl MarketStreamHealth {
    pub fn incident(&mut self, at: u64, kind: &str, detail: &str) {
        let detail = safe_transport_error(detail);
        self.last_error_ms = at;
        self.last_error = detail.clone();
        // Repeated identical failures keep a current timestamp without erasing
        // all useful earlier causes from the bounded history.
        if let Some(last) = self.history.back_mut() {
            if last.kind == kind && last.detail == detail { last.at_ms = at; return; }
        }
        self.history.push_back(TransportIncident { at_ms: at, kind: kind.into(), detail });
        while self.history.len() > HISTORY_LIMIT { self.history.pop_front(); }
    }
    pub fn preserve_history(&mut self, old: &Self) {
        let mut items:Vec<_> = old.history.iter().chain(self.history.iter()).cloned().collect();
        items.sort_by_key(|x|x.at_ms);items.dedup();
        let mut history:VecDeque<TransportIncident>=VecDeque::new();
        for item in items {
            if let Some(last)=history.back_mut() {
                if last.kind==item.kind && last.detail==item.detail {last.at_ms=item.at_ms;continue;}
            }
            history.push_back(item);
        }
        while history.len() > HISTORY_LIMIT { history.pop_front(); }
        self.history = history;
        if self.last_error_ms == 0 {
            self.last_error_ms = old.last_error_ms;
            self.last_error = old.last_error.clone();
        }
    }
}

struct Publisher {
    health: MarketStreamHealth,
    last_exchange_ms: u64,
    last_ws_exchange_ms: u64,
}
impl Publisher {
    fn health_event(&self, sender: &mpsc::Sender<MarketDataEvent>) -> Result<()> {
        try_emit(sender, MarketDataEvent::Transport { venue: MarketVenue::Trade, health: self.health.clone() })
    }
    fn failure(&mut self, sender: &mpsc::Sender<MarketDataEvent>, kind: &str, error: &str) {
        self.health.incident(now_ms(), kind, error);
        let _ = self.health_event(sender);
    }
    fn publish(&mut self, sender: &mpsc::Sender<MarketDataEvent>, mut book: OrderBook,
        source: &str, at: u64) -> Result<bool> {
        // The exchange timestamp, not HTTP completion or a cached receive time,
        // controls freshness. Drop duplicate/backwards snapshots across sources.
        ensure!(book.exchange_timestamp_ms > 0
            && book.exchange_timestamp_ms <= at.saturating_add(1_000)
            && at.saturating_sub(book.exchange_timestamp_ms) <= MAX_SOURCE_AGE_MS,
            "source book timestamp stale or in future");
        TradeBookMode::Fast.limit_recovery_book(&mut book);
        TradeBookMode::Fast.validate_stream_book(&book)?;
        let ws_progress = source == "websocket" && book.exchange_timestamp_ms > self.last_ws_exchange_ms;
        if ws_progress {
            self.last_ws_exchange_ms = book.exchange_timestamp_ms;
            self.health.last_ws_book_ms = at;
        }
        if book.exchange_timestamp_ms <= self.last_exchange_ms {
            self.health_event(sender)?;
            return Ok(ws_progress);
        }
        book.received_timestamp_ms = book.exchange_timestamp_ms.min(at);
        self.health.source = source.into();
        if source == "websocket" {
            self.health.ws_books += 1;
            self.health.last_ws_book_ms = at;
        } else {
            self.health.rest_books += 1;
            self.health.last_rest_book_ms = at;
        }
        self.health_event(sender)?;
        self.last_exchange_ms = book.exchange_timestamp_ms;
        try_emit(sender, MarketDataEvent::Book { venue: MarketVenue::Trade, book })?;
        Ok(true)
    }
}

pub(super) fn spawn(environment: &str, coin: String, source: PriceSource, config: MarketStreamConfig)
    -> Result<mpsc::Receiver<MarketDataEvent>> {
    config.validate()?;
    ensure!(!coin.trim().is_empty(), "trade coin is required");
    let environment = environment.trim().to_ascii_lowercase();
    let url = match environment.as_str() {
        "mainnet" => "wss://api.hyperliquid.xyz/ws",
        "testnet" => "wss://api.hyperliquid-testnet.xyz/ws",
        _ => bail!("unsupported trade environment"),
    };
    let request_coin = coin.clone();
    spawn_inner(url.into(), coin, source, config, move || {
        let environment=environment.clone(); let coin=request_coin.clone();
        async move { crate::hyperliquid::trading_info(false,
            crate::hyperliquid::fetch_l2_book_snapshot(&environment,&coin)).await }
    })
}

fn spawn_inner<F, R>(url: String, coin: String, source: PriceSource, config: MarketStreamConfig, mut fetch: F)
    -> Result<mpsc::Receiver<MarketDataEvent>>
where F: FnMut() -> R + Send + 'static, R: std::future::Future<Output=Result<Value>> + Send {
    let (sender, receiver) = mpsc::channel(config.queue_capacity);
    let publisher = Arc::new(Mutex::new(Publisher { health: MarketStreamHealth::default(), last_exchange_ms: 0, last_ws_exchange_ms: 0 }));
    let started = now_ms();
    {
        let sender = sender.clone(); let publisher = publisher.clone(); let coin = coin.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut attempted = 0;
            loop {
                tokio::select! { _ = sender.closed() => return, _ = tick.tick() => {} }
                let at = now_ms();
                let last_ws = publisher.lock().unwrap().health.last_ws_book_ms.max(started);
                if !should_attempt_recovery(last_ws, attempted, at, FALLBACK_AFTER_MS, FALLBACK_INTERVAL_MS) { continue; }
                attempted = at;
                // Use the shared routine budget (2 weight/request), one attempt,
                // bounded timeout and server 429 cooldown. No execution reserve.
                let result = tokio::select! {
                    _ = sender.closed() => return,
                    r = tokio::time::timeout(Duration::from_millis(REST_DEADLINE_MS),
                        fetch()) =>
                        r.context("REST book recovery deadline exceeded").and_then(|r|r),
                };
                let at = now_ms();
                let result = result.and_then(|v| {
                    let wire: TradeRestBook = serde_json::from_value(v).context("invalid REST book response")?;
                    let book = trade_rest_book(&coin, wire, at)?;
                    publisher.lock().unwrap().publish(&sender, book, "rest", at)
                });
                if let Err(error) = result {
                    let mut p = publisher.lock().unwrap();
                    p.health.rest_failures += 1;
                    p.failure(&sender, "rest_error", &format!("{error:#}"));
                }
            }
        });
    }
    tokio::spawn(async move {
        loop {
            let started = Instant::now();
            // Own the socket directly: malformed/irrelevant SDK messages can no
            // longer disappear in a detached reader without a diagnosable cause.
            let result = tokio::select! {
                _ = sender.closed() => return,
                r = session(&url, &coin, source, &sender, &publisher) => r,
            };
            {
                let mut p = publisher.lock().unwrap();
                p.health.ws_connected = false;
                p.health.reconnect_count += 1;
                p.failure(&sender, "ws_reconnect", &format!("{:#}", result.err().unwrap_or_else(||anyhow::anyhow!("websocket ended"))));
                // A failing WS must not invalidate a fresh independently verified
                // REST snapshot. Both sources still expire through Book::validate.
            }
            let delay = trade_reconnect_delay_ms(started.elapsed().as_millis() as u64, config.reconnect_delay_ms);
            tokio::select! { _ = sender.closed() => return, _ = tokio::time::sleep(Duration::from_millis(delay)) => {} }
        }
    });
    Ok(receiver)
}

async fn session(url: &str, coin: &str, source: PriceSource, sender: &mpsc::Sender<MarketDataEvent>,
    publisher: &Arc<Mutex<Publisher>>) -> Result<()> {
    let mut ws = tokio::time::timeout(Duration::from_millis(TRADE_SESSION_SETUP_TIMEOUT_MS), async {
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.context("websocket connect/TLS failed")?;
        for subscription in [json!({"type":"l2Book","coin":coin,"fast":true}), json!({"type":"activeAssetCtx","coin":coin})] {
            ws.send(Message::Text(json!({"method":"subscribe","subscription":subscription}).to_string()))
                .await.context("websocket subscribe write failed")?;
        }
        Ok::<_, anyhow::Error>(ws)
    }).await.context("websocket connection/subscription deadline exceeded")??;
    {
        let mut p = publisher.lock().unwrap(); p.health.ws_connected = true; p.health_event(sender)?;
    }
    let mut last_book = tokio::time::Instant::now();
    let mut heartbeat = tokio::time::interval_at(last_book + Duration::from_secs(20), Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let deadline = last_book + Duration::from_millis(FAST_BOOK_SILENCE_MS);
        ensure!(tokio::time::Instant::now() < deadline, "websocket L2 silent for 3000ms");
        let message = tokio::select! {
            frame = tokio::time::timeout_at(deadline, ws.next()) => frame
                .context("websocket L2 silent for 3000ms")?.context("websocket stream ended")?
                .context("websocket read failed")?,
            _ = heartbeat.tick() => {
                tokio::time::timeout_at(deadline, ws.send(Message::Text(json!({"method":"ping"}).to_string())))
                    .await.context("websocket ping write deadline")??;
                continue;
            }
        };
        match message {
            Message::Ping(payload) => {
                tokio::time::timeout_at(deadline, ws.send(Message::Pong(payload))).await
                    .context("websocket pong write deadline")??;
            }
            Message::Close(frame) => bail!("websocket remote close code={:?}",frame.map(|f|u16::from(f.code))),
            Message::Text(text) => {
                ensure!(text.len() <= 1_048_576, "public websocket frame too large");
                let value: Value = serde_json::from_str(&text).context("public websocket JSON invalid")?;
                match value["channel"].as_str().unwrap_or("") {
                    "l2Book" => {
                        let wire: TradeRestBook = serde_json::from_value(value["data"].clone()).context("websocket L2 payload invalid")?;
                        let at = now_ms(); let book = trade_rest_book(coin, wire, at)?;
                        ensure!(book.bids.len() <= 5 && book.asks.len() <= 5, "fast websocket L2 depth exceeds five");
                        if publisher.lock().unwrap().publish(sender, book, "websocket", at)? { last_book = tokio::time::Instant::now(); }
                    }
                    "activeAssetCtx" => {
                        // A malformed context cannot repeatedly tear down a good
                        // L2 feed. Keep the old mark's timestamp so it expires.
                        let result = (|| -> Result<()> {
                            let data = &value["data"]; ensure!(data["coin"].as_str()==Some(coin), "context coin mismatch");
                            let c=&data["ctx"]; let at=now_ms();
                            let context=VenueMarketContext {
                                mark_price:parse_optional_positive(c["markPx"].as_str())?,
                                index_price:parse_optional_positive(c["oraclePx"].as_str())?,
                                funding_rate:parse_optional_finite(c["funding"].as_str())?,
                                source:if source==PriceSource::Unknown {automatic_trade_clock(at)?.source}else{source},
                                exchange_timestamp_ms:at,received_timestamp_ms:at,
                            };
                            try_emit(sender,MarketDataEvent::Context{venue:MarketVenue::Trade,context})
                        })();
                        if let Err(e)=result { publisher.lock().unwrap().failure(sender,"context_error",&format!("{e:#}")); }
                    }
                    "error" => bail!("websocket server error: {}",safe_transport_error(&value["data"].to_string())),
                    _ => {}, // acknowledgements/pongs are not evidence of a fresh book
                }
            }
            _ => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wire(coin: &str, at: u64) -> Value {
        json!({"coin":coin,"time":at,"levels":[[{"px":"2000","sz":"1","n":1}],[{"px":"2001","sz":"1","n":1}]]})
    }
    fn publisher() -> Publisher {
        Publisher{health:MarketStreamHealth::default(),last_exchange_ms:0,last_ws_exchange_ms:0}
    }
    #[test]
    fn fallback_snapshot_cannot_refresh_cached_future_backwards_or_invalid_prices() {
        let (tx, mut rx)=mpsc::channel(128); let mut p=publisher();
        let book=|time|trade_rest_book("io:ANTH",serde_json::from_value(wire("io:ANTH",time)).unwrap(),10_000).unwrap();
        assert!(p.publish(&tx,book(10_000),"rest",10_100).unwrap());
        assert!(!p.publish(&tx,book(10_000),"rest",10_600).unwrap());
        assert!(!p.publish(&tx,book(9_999),"rest",10_700).unwrap());
        assert!(p.publish(&tx,book(8_499),"rest",10_000).is_err());
        assert!(p.publish(&tx,book(11_001),"rest",10_000).is_err());
        let mut emitted=vec![];
        while let Ok(e)=rx.try_recv() {if let MarketDataEvent::Book{book,..}=e {emitted.push(book);}}
        assert_eq!(emitted.len(),1); assert_eq!(emitted[0].received_timestamp_ms,10_000);
        // A WS snapshot overtaken by REST is valid WS progress, but never rolls
        // the displayed/tradable book backwards. Duplicate WS data is not progress.
        assert!(p.publish(&tx,book(9_999),"websocket",10_700).unwrap());
        assert!(!p.publish(&tx,book(9_999),"websocket",10_800).unwrap());
        assert_eq!(p.last_exchange_ms,10_000);
        assert!(trade_rest_book("io:ANTH",serde_json::from_value(wire("io:OAI",10_000)).unwrap(),10_000).is_err());
        let mut bad=book(10_001);bad.asks.clear();assert!(p.publish(&tx,bad,"rest",10_100).is_err());
    }
    #[test]
    fn transport_error_history_is_bounded_and_redacted() {
        let mut h=MarketStreamHealth::default();
        for n in 0..100 {h.incident(n,"ws_reconnect",&format!("connect failure {n} https://private.invalid/?secret=abc Bearer hidden-value"));}
        assert_eq!(h.history.len(),32);
        let text=serde_json::to_string(&h).unwrap();
        assert!(!text.contains("private.invalid"));assert!(!text.contains("hidden-value"));
        assert!(text.contains("connect failure"));
        let mut next=MarketStreamHealth::default();next.preserve_history(&h);assert_eq!(next.history,h.history);
    }
    #[tokio::test]
    async fn real_supervisor_recovers_silent_ws_while_rest_keeps_updating() {
        use tokio::{net::TcpListener,sync::oneshot};
        tokio::time::timeout(Duration::from_secs(14),async {
            let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
            let (closed_tx,closed_rx)=oneshot::channel();
            let server=tokio::spawn(async move {
                // First stream sends unrelated traffic but no L2 snapshots.
                for attempt in 0..2 {
                    let (socket,_)=listener.accept().await.unwrap();let mut ws=tokio_tungstenite::accept_async(socket).await.unwrap();
                    for _ in 0..2 {ws.next().await.unwrap().unwrap();}
                    if attempt==0 {
                        let mut interval=tokio::time::interval(Duration::from_millis(100));
                        loop {tokio::select! {
                            frame=ws.next()=>if !matches!(frame,Some(Ok(_))) {break;},
                            _=interval.tick()=>{
                                if ws.send(Message::Text(json!({"channel":"pong"}).to_string())).await.is_err(){break;}
                            }
                        }}
                    } else {
                        let mut interval=tokio::time::interval(Duration::from_millis(100));
                        loop {tokio::select! {
                            frame=ws.next()=>if !matches!(frame,Some(Ok(_))) {break;},
                            _=interval.tick()=>{
                                // Bad ancillary data must not tear down healthy books.
                                if ws.send(Message::Text(json!({"channel":"activeAssetCtx","data":{"coin":"io:ANTH","ctx":{"markPx":"bad"}}}).to_string())).await.is_err(){break;}
                                if ws.send(Message::Text(json!({"channel":"l2Book","data":wire("io:ANTH",now_ms())}).to_string())).await.is_err(){break;}
                            }
                        }}
                    }
                }
                let _=closed_tx.send(());
            });
            let calls=Arc::new(AtomicU64::new(0));let counter=calls.clone();
            let mut rx=spawn_inner(format!("ws://{address}"),"io:ANTH".into(),PriceSource::Unknown,MarketStreamConfig::default(),move|| {
                counter.fetch_add(1,Ordering::Relaxed);async{Ok(wire("io:ANTH",now_ms()))}
            }).unwrap();
            let mut rest=0;let mut ws=0;let mut reconnect=false;let mut context_error=false;
            loop {
                if let MarketDataEvent::Transport{health,..}=rx.recv().await.unwrap() {
                    rest=rest.max(health.rest_books);ws=ws.max(health.ws_books);
                    reconnect|=health.history.iter().any(|x|x.kind=="ws_reconnect"&&x.detail.contains("silent"));
                    context_error|=health.history.iter().any(|x|x.kind=="context_error");
                }
                if ws>=20 {break;}
            }
            assert!(rest>=2,"REST failed to bridge dead WS");assert!(reconnect);assert!(context_error);
            let count=calls.load(Ordering::Relaxed);
            // A healthy restored stream switches off fallback reads.
            let until=tokio::time::Instant::now()+Duration::from_secs(2);
            while let Ok(Some(_))=tokio::time::timeout_at(until,rx.recv()).await {}
            assert_eq!(calls.load(Ordering::Relaxed),count);
            drop(rx);closed_rx.await.unwrap();server.await.unwrap();
        }).await.unwrap();
    }
    #[tokio::test]
    async fn rest_timeouts_and_connection_failures_are_diagnosable_without_books() {
        tokio::time::timeout(Duration::from_secs(6),async {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();drop(listener);
            let mut rx=spawn_inner(format!("ws://{addr}"),"io:OAI".into(),PriceSource::Unknown,MarketStreamConfig::default(),||async {
                std::future::pending::<Result<Value>>().await
            }).unwrap();
            let mut connection=false;
            loop {match rx.recv().await.unwrap() {
                MarketDataEvent::Book{..}=>panic!("failed requests manufactured a fresh book"),
                MarketDataEvent::Transport{health,..}=>{
                    connection|=health.last_error.contains("connect/TLS");
                    if health.rest_failures>0 {assert!(connection);assert!(health.last_error.contains("deadline"));break;}
                },_=>{}
            }}
        }).await.unwrap();
    }
}
