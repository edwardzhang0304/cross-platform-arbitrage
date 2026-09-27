use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use hyperliquid_rust_sdk::{
    BaseUrl, InfoClient, Message as HyperliquidMessage, Subscription as HyperliquidSubscription,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::{
    domain::now_ms,
    lighter::{
        LighterClient, LighterEndpoints, LighterEnvironment, LighterMarket, LighterOrderBook,
        connect_lighter_websocket,
    },
};

use super::{
    AggressorSide, OrderBook, PriceLevel, PriceSource, ShadowTradePrint, automatic_trade_clock,
};

const PUBLIC_RECOVERY_CHECK_MS: u64 = 1_000;
const PUBLIC_BOOK_RECOVERY_AFTER_MS: u64 = 10_000;
const PUBLIC_CONTEXT_RECOVERY_AFTER_MS: u64 = 10_000;
const PUBLIC_RECOVERY_MIN_INTERVAL_MS: u64 = 15_000;
const PUBLIC_RECOVERY_REQUEST_TIMEOUT_MS: u64 = 5_000;
const TRADE_SESSION_SETUP_TIMEOUT_MS: u64 = 4_000;
const TRADE_MIN_CONNECTION_CYCLE_MS: u64 = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketVenue {
    Lighter,
    Trade,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VenueMarketContext {
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
    pub funding_rate: Option<f64>,
    pub source: PriceSource,
    pub exchange_timestamp_ms: u64,
    pub received_timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MarketDataEvent {
    Connected {
        venue: MarketVenue,
        received_at_ms: u64,
    },
    Book {
        venue: MarketVenue,
        book: OrderBook,
    },
    Context {
        venue: MarketVenue,
        context: VenueMarketContext,
    },
    Trade {
        venue: MarketVenue,
        trade: ShadowTradePrint,
    },
    Gap {
        venue: MarketVenue,
        detail: String,
        received_at_ms: u64,
    },
    Disconnected {
        venue: MarketVenue,
        detail: String,
        received_at_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub struct MarketStreamConfig {
    pub queue_capacity: usize,
    pub stale_timeout_ms: u64,
    pub reconnect_delay_ms: u64,
}

impl Default for MarketStreamConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            // Transport idleness is not the market-data freshness gate. A
            // quiet book can legitimately receive no mutation for several
            // seconds; freshness remains enforced independently by
            // SourceEvidencePolicy before a snapshot can qualify.
            stale_timeout_ms: 30_000,
            reconnect_delay_ms: 1_000,
        }
    }
}

impl MarketStreamConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            (8..=65_536).contains(&self.queue_capacity),
            "market stream queue capacity must be between 8 and 65536"
        );
        ensure!(
            (500..=60_000).contains(&self.stale_timeout_ms),
            "market stream stale timeout must be between 500 and 60000ms"
        );
        ensure!(
            (250..=60_000).contains(&self.reconnect_delay_ms),
            "market stream reconnect delay must be between 250 and 60000ms"
        );
        Ok(())
    }
}

#[derive(Debug)]
struct LighterRemoteClosed {
    session_lifetime_ms: u64,
    close_code: Option<u16>,
}

impl fmt::Display for LighterRemoteClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Lighter public websocket closed code={:?} session_lifetime_ms={}",
            self.close_code, self.session_lifetime_ms
        )
    }
}

impl std::error::Error for LighterRemoteClosed {}

fn lighter_reconnect_delay_ms(error: &anyhow::Error, configured_delay_ms: u64) -> u64 {
    match error.downcast_ref::<LighterRemoteClosed>() {
        Some(closed) if closed.session_lifetime_ms >= 30_000 => 0,
        _ => configured_delay_ms,
    }
}

pub fn spawn_lighter_market_stream(
    endpoints: LighterEndpoints,
    market_id: i32,
    configured_source: PriceSource,
    config: MarketStreamConfig,
) -> Result<mpsc::Receiver<MarketDataEvent>> {
    endpoints.validate()?;
    config.validate()?;
    ensure!(market_id >= 0, "Lighter market id must be non-negative");
    let (sender, receiver) = mpsc::channel(config.queue_capacity);
    // Give the websocket the first opportunity to establish and deliver its
    // subscription snapshots. REST is only a bounded stale-data fallback.
    let started_at_ms = now_ms();
    let last_book_at_ms = Arc::new(AtomicU64::new(started_at_ms));
    let last_context_at_ms = Arc::new(AtomicU64::new(started_at_ms));
    let recovery_client = LighterClient::new(LighterEnvironment::Robinhood, endpoints.clone())?;
    tokio::spawn(run_lighter_public_recovery(
        recovery_client,
        market_id,
        configured_source,
        sender.clone(),
        Arc::clone(&last_book_at_ms),
        Arc::clone(&last_context_at_ms),
    ));
    tokio::spawn(async move {
        loop {
            if sender.is_closed() {
                return;
            }
            let result = run_lighter_session(
                &endpoints,
                market_id,
                configured_source,
                &config,
                &sender,
                &last_book_at_ms,
                &last_context_at_ms,
            )
            .await;
            if sender.is_closed() {
                return;
            }
            let reconnect_delay_ms = result
                .as_ref()
                .err()
                .map(|error| lighter_reconnect_delay_ms(error, config.reconnect_delay_ms))
                .unwrap_or(config.reconnect_delay_ms);
            let detail = result
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "Lighter websocket ended".to_string());
            if try_emit(
                &sender,
                MarketDataEvent::Disconnected {
                    venue: MarketVenue::Lighter,
                    detail,
                    received_at_ms: now_ms(),
                },
            )
            .is_err()
            {
                return;
            }
            if reconnect_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(reconnect_delay_ms)).await;
            }
        }
    });
    Ok(receiver)
}

async fn run_lighter_session(
    endpoints: &LighterEndpoints,
    market_id: i32,
    configured_source: PriceSource,
    config: &MarketStreamConfig,
    sender: &mpsc::Sender<MarketDataEvent>,
    last_book_at_ms: &AtomicU64,
    last_context_at_ms: &AtomicU64,
) -> Result<()> {
    let (stream, _) = tokio::time::timeout(
        Duration::from_millis(config.stale_timeout_ms),
        connect_lighter_websocket(&endpoints.public_readonly_ws_url()),
    )
    .await
    .context("timed out connecting Lighter public websocket")?
    .context("failed to connect Lighter public websocket")?;
    let (mut writer, mut reader) = stream.split();
    let connected_at = Instant::now();
    let mut subscriptions_sent = false;
    let mut assembler = LighterBookAssembler::default();

    loop {
        let message = tokio::time::timeout(
            Duration::from_millis(config.stale_timeout_ms),
            reader.next(),
        )
        .await
        .context("Lighter market websocket became stale")?;
        let Some(message) = message else {
            return Err(LighterRemoteClosed {
                close_code: None,
                session_lifetime_ms: u64::try_from(connected_at.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
            }
            .into());
        };
        match message.context("Lighter public websocket read failed")? {
            Message::Text(text) => {
                let value: Value =
                    serde_json::from_str(&text).context("invalid Lighter market websocket JSON")?;
                match value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "connected" if !subscriptions_sent => {
                        for channel in [
                            format!("order_book/{market_id}"),
                            format!("market_stats/{market_id}"),
                        ] {
                            writer
                                .send(Message::Text(
                                    json!({"type":"subscribe","channel":channel}).to_string(),
                                ))
                                .await
                                .context("failed to subscribe Lighter market websocket")?;
                        }
                        subscriptions_sent = true;
                        try_emit(
                            sender,
                            MarketDataEvent::Connected {
                                venue: MarketVenue::Lighter,
                                received_at_ms: now_ms(),
                            },
                        )?;
                    }
                    "ping" => {
                        writer
                            .send(Message::Text(json!({"type":"pong"}).to_string()))
                            .await
                            .context("failed to reply to Lighter ping")?;
                    }
                    "subscribed/order_book" | "update/order_book" => {
                        match assembler.apply_value(&value) {
                            Ok(book) => {
                                last_book_at_ms
                                    .store(book.received_timestamp_ms, Ordering::Relaxed);
                                try_emit(
                                    sender,
                                    MarketDataEvent::Book {
                                        venue: MarketVenue::Lighter,
                                        book,
                                    },
                                )?
                            }
                            Err(error) => {
                                let detail = format!("{error:#}");
                                let _ = try_emit(
                                    sender,
                                    MarketDataEvent::Gap {
                                        venue: MarketVenue::Lighter,
                                        detail: detail.clone(),
                                        received_at_ms: now_ms(),
                                    },
                                );
                                bail!("Lighter order book continuity failed: {detail}");
                            }
                        }
                    }
                    "subscribed/market_stats" | "update/market_stats" => {
                        let context = parse_lighter_context(&value, configured_source)?;
                        last_context_at_ms.store(context.received_timestamp_ms, Ordering::Relaxed);
                        try_emit(
                            sender,
                            MarketDataEvent::Context {
                                venue: MarketVenue::Lighter,
                                context,
                            },
                        )?;
                    }
                    _ => {}
                }
            }
            Message::Ping(payload) => writer
                .send(Message::Pong(payload))
                .await
                .context("failed to reply to Lighter websocket ping")?,
            Message::Close(frame) => {
                return Err(LighterRemoteClosed {
                    close_code: frame.map(|frame| u16::from(frame.code)),
                    session_lifetime_ms: u64::try_from(connected_at.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                }
                .into());
            }
            _ => {}
        }
    }
}

async fn run_lighter_public_recovery(
    client: LighterClient,
    market_id: i32,
    configured_source: PriceSource,
    sender: mpsc::Sender<MarketDataEvent>,
    last_book_at_ms: Arc<AtomicU64>,
    last_context_at_ms: Arc<AtomicU64>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(PUBLIC_RECOVERY_CHECK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_book_attempt_ms = 0;
    let mut last_context_attempt_ms = 0;
    loop {
        ticker.tick().await;
        if sender.is_closed() {
            return;
        }
        let checked_at_ms = now_ms();
        if should_attempt_recovery(
            last_book_at_ms.load(Ordering::Relaxed),
            last_book_attempt_ms,
            checked_at_ms,
            PUBLIC_BOOK_RECOVERY_AFTER_MS,
            PUBLIC_RECOVERY_MIN_INTERVAL_MS,
        ) {
            last_book_attempt_ms = checked_at_ms;
            if let Ok(Ok(book)) = tokio::time::timeout(
                Duration::from_millis(PUBLIC_RECOVERY_REQUEST_TIMEOUT_MS),
                client.order_book(market_id, 100),
            )
            .await
            {
                let received_at_ms = now_ms();
                if let Ok(book) = lighter_rest_book(book, received_at_ms) {
                    last_book_at_ms.store(received_at_ms, Ordering::Relaxed);
                    if try_emit(
                        &sender,
                        MarketDataEvent::Book {
                            venue: MarketVenue::Lighter,
                            book,
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                }
            }
        }
        if should_attempt_recovery(
            last_context_at_ms.load(Ordering::Relaxed),
            last_context_attempt_ms,
            checked_at_ms,
            PUBLIC_CONTEXT_RECOVERY_AFTER_MS,
            PUBLIC_RECOVERY_MIN_INTERVAL_MS,
        ) {
            last_context_attempt_ms = checked_at_ms;
            if let Ok(Ok((market, funding_rate))) = tokio::time::timeout(
                Duration::from_millis(PUBLIC_RECOVERY_REQUEST_TIMEOUT_MS),
                async {
                    tokio::try_join!(
                        client.market_by_id(market_id),
                        client.funding_rate(market_id)
                    )
                },
            )
            .await
            {
                let received_at_ms = now_ms();
                if let Ok(context) =
                    lighter_rest_context(&market, funding_rate, configured_source, received_at_ms)
                {
                    last_context_at_ms.store(received_at_ms, Ordering::Relaxed);
                    if try_emit(
                        &sender,
                        MarketDataEvent::Context {
                            venue: MarketVenue::Lighter,
                            context,
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

pub fn spawn_trade_market_stream(
    environment: &str,
    coin: String,
    configured_source: PriceSource,
    config: MarketStreamConfig,
) -> Result<mpsc::Receiver<MarketDataEvent>> {
    spawn_trade_market_stream_with_mode(environment, coin, configured_source, config, TradeBookMode::Standard)
}

/// Public five-level snapshots; enabled explicitly by the OPENAI inventory service.
pub fn spawn_trade_fast_market_stream(
    environment: &str,
    coin: String,
    configured_source: PriceSource,
    config: MarketStreamConfig,
) -> Result<mpsc::Receiver<MarketDataEvent>> {
    spawn_trade_market_stream_with_mode(environment, coin, configured_source, config, TradeBookMode::Fast)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TradeBookMode {
    Standard,
    Fast,
}

impl TradeBookMode {
    fn validate_stream_book(self, book: &OrderBook) -> Result<()> {
        if self == Self::Fast {
            ensure!(book.bids.len() <= 5 && book.asks.len() <= 5,
                "fast L2 subscription returned more than five levels");
        }
        book.validate()
    }

    fn limit_recovery_book(self, book: &mut OrderBook) {
        if self == Self::Fast {
            // REST recovery is also a full replacement. Never retain deeper
            // levels from an older standard snapshot under a fresh timestamp.
            book.bids.truncate(5);
            book.asks.truncate(5);
        }
    }
}

fn spawn_trade_market_stream_with_mode(
    environment: &str,
    coin: String,
    configured_source: PriceSource,
    config: MarketStreamConfig,
    book_mode: TradeBookMode,
) -> Result<mpsc::Receiver<MarketDataEvent>> {
    config.validate()?;
    ensure!(!coin.trim().is_empty(), "trade coin is required");
    let normalized_environment = environment.trim().to_ascii_lowercase();
    let (base_url, info_url) = match normalized_environment.as_str() {
        "mainnet" => (BaseUrl::Mainnet, "https://api.hyperliquid.xyz/info"),
        "testnet" => (BaseUrl::Testnet, "https://api.hyperliquid-testnet.xyz/info"),
        other => bail!("unsupported trade environment {other}"),
    };
    let (sender, receiver) = mpsc::channel(config.queue_capacity);
    let last_book_at_ms = Arc::new(AtomicU64::new(now_ms()));
    let recovery_http = reqwest::Client::builder()
        .timeout(Duration::from_millis(PUBLIC_RECOVERY_REQUEST_TIMEOUT_MS))
        .build()
        .context("failed to build trade public recovery client")?;
    tokio::spawn(run_trade_public_book_recovery(
        recovery_http,
        info_url,
        coin.clone(),
        sender.clone(),
        Arc::clone(&last_book_at_ms),
        book_mode,
    ));
    tokio::spawn(async move {
        loop {
            if sender.is_closed() {
                return;
            }
            let attempt_started = Instant::now();
            let result = run_trade_session(
                base_url.clone(),
                &coin,
                configured_source,
                &config,
                &sender,
                &last_book_at_ms,
                book_mode,
            )
            .await;
            if sender.is_closed() {
                return;
            }
            let detail = result
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "trade websocket ended".to_string());
            if try_emit(
                &sender,
                MarketDataEvent::Disconnected {
                    venue: MarketVenue::Trade,
                    detail,
                    received_at_ms: now_ms(),
                },
            )
            .is_err()
            {
                return;
            }
            let reconnect_delay_ms = trade_reconnect_delay_ms(
                u64::try_from(attempt_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                config.reconnect_delay_ms,
            );
            tokio::time::sleep(Duration::from_millis(reconnect_delay_ms)).await;
        }
    });
    Ok(receiver)
}

fn trade_reconnect_delay_ms(attempt_elapsed_ms: u64, configured_delay_ms: u64) -> u64 {
    configured_delay_ms.max(TRADE_MIN_CONNECTION_CYCLE_MS.saturating_sub(attempt_elapsed_ms))
}

async fn run_trade_session(
    base_url: BaseUrl,
    coin: &str,
    configured_source: PriceSource,
    config: &MarketStreamConfig,
    sender: &mpsc::Sender<MarketDataEvent>,
    last_book_at_ms: &AtomicU64,
    book_mode: TradeBookMode,
) -> Result<()> {
    // Own reconnection here instead of allowing the SDK's background reader
    // and this outer supervisor to race. All three subscriptions share one
    // setup deadline so a wedged socket cannot amplify into repeated 30s
    // outages.
    let (
        mut client,
        mut sdk_receiver,
        book_subscription,
        context_subscription,
        trades_subscription,
    ) = tokio::time::timeout(
        Duration::from_millis(TRADE_SESSION_SETUP_TIMEOUT_MS),
        async {
            let mut client = InfoClient::new(None, Some(base_url))
                .await
                .context("failed to initialize trade websocket")?;
            let (sdk_sender, sdk_receiver) = tokio::sync::mpsc::unbounded_channel();
            let book_subscription = if book_mode == TradeBookMode::Fast {
                client.subscribe_l2_book_fast(coin.to_string(), sdk_sender.clone()).await
            } else {
                client.subscribe(
                    HyperliquidSubscription::L2Book {
                        coin: coin.to_string(),
                    },
                    sdk_sender.clone(),
                )
                .await
            }.context("failed to subscribe trade L2 book")?;
            let context_subscription = client
                .subscribe(
                    HyperliquidSubscription::ActiveAssetCtx {
                        coin: coin.to_string(),
                    },
                    sdk_sender.clone(),
                )
                .await
                .context("failed to subscribe trade asset context")?;
            let trades_subscription = client
                .subscribe(
                    HyperliquidSubscription::Trades {
                        coin: coin.to_string(),
                    },
                    sdk_sender,
                )
                .await
                .context("failed to subscribe trade prints")?;
            Ok::<_, anyhow::Error>((
                client,
                sdk_receiver,
                book_subscription,
                context_subscription,
                trades_subscription,
            ))
        },
    )
    .await
    .context("timed out establishing trade websocket subscriptions")??;
    try_emit(
        sender,
        MarketDataEvent::Connected {
            venue: MarketVenue::Trade,
            received_at_ms: now_ms(),
        },
    )?;

    loop {
        let message = tokio::time::timeout(
            Duration::from_millis(config.stale_timeout_ms),
            sdk_receiver.recv(),
        )
        .await
        .context("trade market websocket became stale")?;
        let Some(message) = message else {
            bail!("trade websocket receiver closed");
        };
        match message {
            HyperliquidMessage::NoData => {
                bail!("trade websocket disconnected");
            }
            HyperliquidMessage::L2Book(book) => {
                let data = book.data;
                ensure!(
                    data.coin.eq_ignore_ascii_case(coin),
                    "trade websocket returned the wrong coin"
                );
                let bids = parse_hyperliquid_side(data.levels.first())?;
                let asks = parse_hyperliquid_side(data.levels.get(1))?;
                let book = OrderBook {
                    bids,
                    asks,
                    exchange_timestamp_ms: data.time,
                    received_timestamp_ms: now_ms(),
                    sequence_valid: true,
                };
                book_mode.validate_stream_book(&book)?;
                last_book_at_ms.store(book.received_timestamp_ms, Ordering::Relaxed);
                try_emit(
                    sender,
                    MarketDataEvent::Book {
                        venue: MarketVenue::Trade,
                        book,
                    },
                )?;
            }
            HyperliquidMessage::ActiveAssetCtx(active) => {
                use hyperliquid_rust_sdk::AssetCtx;
                let data = active.data;
                ensure!(
                    data.coin.eq_ignore_ascii_case(coin),
                    "trade context returned the wrong coin"
                );
                let AssetCtx::Perps(context) = data.ctx else {
                    bail!("HIP-3 counterparty context unexpectedly described a spot asset");
                };
                let received_timestamp_ms = now_ms();
                let source = if configured_source == PriceSource::Unknown {
                    automatic_trade_clock(received_timestamp_ms)?.source
                } else {
                    configured_source
                };
                try_emit(
                    sender,
                    MarketDataEvent::Context {
                        venue: MarketVenue::Trade,
                        context: VenueMarketContext {
                            mark_price: parse_optional_positive(Some(&context.shared.mark_px))?,
                            index_price: parse_optional_positive(Some(&context.oracle_px))?,
                            funding_rate: parse_optional_finite(Some(&context.funding))?,
                            source,
                            exchange_timestamp_ms: received_timestamp_ms,
                            received_timestamp_ms,
                        },
                    },
                )?;
            }
            HyperliquidMessage::Trades(trades) => {
                let received_timestamp_ms = now_ms();
                for trade in trades.data {
                    ensure!(
                        trade.coin.eq_ignore_ascii_case(coin),
                        "trade print returned the wrong coin"
                    );
                    try_emit(
                        sender,
                        MarketDataEvent::Trade {
                            venue: MarketVenue::Trade,
                            trade: parse_hyperliquid_trade(&trade, received_timestamp_ms)?,
                        },
                    )?;
                }
            }
            HyperliquidMessage::HyperliquidError(error) => {
                bail!("trade websocket error: {error}");
            }
            _ => {}
        }
        if sender.is_closed() {
            let _ = client.unsubscribe(book_subscription).await;
            let _ = client.unsubscribe(context_subscription).await;
            let _ = client.unsubscribe(trades_subscription).await;
            return Ok(());
        }
    }
}

#[derive(Debug, Deserialize)]
struct TradeRestBook {
    coin: String,
    time: u64,
    levels: [Vec<TradeRestLevel>; 2],
}

#[derive(Debug, Deserialize)]
struct TradeRestLevel {
    px: String,
    sz: String,
}

async fn run_trade_public_book_recovery(
    http: reqwest::Client,
    info_url: &'static str,
    coin: String,
    sender: mpsc::Sender<MarketDataEvent>,
    last_book_at_ms: Arc<AtomicU64>,
    book_mode: TradeBookMode,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(PUBLIC_RECOVERY_CHECK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_attempt_ms = 0;
    loop {
        ticker.tick().await;
        if sender.is_closed() {
            return;
        }
        let checked_at_ms = now_ms();
        if !should_attempt_recovery(
            last_book_at_ms.load(Ordering::Relaxed),
            last_attempt_ms,
            checked_at_ms,
            PUBLIC_BOOK_RECOVERY_AFTER_MS,
            PUBLIC_RECOVERY_MIN_INTERVAL_MS,
        ) {
            continue;
        }
        last_attempt_ms = checked_at_ms;
        let Ok(response) = http
            .post(info_url)
            .json(&json!({"type":"l2Book","coin":coin}))
            .send()
            .await
        else {
            continue;
        };
        let Ok(response) = response.error_for_status() else {
            continue;
        };
        let Ok(wire) = response.json::<TradeRestBook>().await else {
            continue;
        };
        let received_at_ms = now_ms();
        let Ok(mut book) = trade_rest_book(&coin, wire, received_at_ms) else {
            continue;
        };
        book_mode.limit_recovery_book(&mut book);
        if book_mode.validate_stream_book(&book).is_err() {
            continue;
        }
        last_book_at_ms.store(received_at_ms, Ordering::Relaxed);
        if try_emit(
            &sender,
            MarketDataEvent::Book {
                venue: MarketVenue::Trade,
                book,
            },
        )
        .is_err()
        {
            return;
        }
    }
}

fn parse_hyperliquid_trade(
    trade: &hyperliquid_rust_sdk::Trade,
    received_timestamp_ms: u64,
) -> Result<ShadowTradePrint> {
    let aggressor = match trade.side.trim().to_ascii_uppercase().as_str() {
        "B" => AggressorSide::Buy,
        "A" => AggressorSide::Sell,
        other => bail!("unsupported trade aggressor side {other}"),
    };
    let price = trade.px.parse::<f64>().context("invalid trade price")?;
    let size = trade.sz.parse::<f64>().context("invalid trade size")?;
    ensure!(
        price.is_finite() && price > 0.0,
        "trade price must be positive"
    );
    ensure!(
        size.is_finite() && size > 0.0,
        "trade size must be positive"
    );
    ensure!(
        received_timestamp_ms >= trade.time,
        "trade receipt predates exchange timestamp"
    );
    Ok(ShadowTradePrint {
        // Hyperliquid documents (block time, coin, tid) as the globally
        // unique public-trade identity.
        trade_id: format!("{}:{}:{}", trade.time, trade.coin, trade.tid),
        aggressor,
        price,
        size,
        exchange_timestamp_ms: trade.time,
        received_timestamp_ms,
    })
}

fn try_emit(sender: &mpsc::Sender<MarketDataEvent>, event: MarketDataEvent) -> Result<()> {
    sender.try_send(event).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => {
            anyhow::anyhow!("market event consumer fell behind; stream failed closed")
        }
        mpsc::error::TrySendError::Closed(_) => anyhow::anyhow!("market event receiver closed"),
    })
}

fn parse_hyperliquid_side(
    levels: Option<&Vec<hyperliquid_rust_sdk::BookLevel>>,
) -> Result<Vec<PriceLevel>> {
    levels
        .context("trade L2 book side is missing")?
        .iter()
        .map(|level| parse_level(&level.px, &level.sz))
        .collect()
}

#[derive(Debug, Deserialize)]
struct LighterWireLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct LighterWireBook {
    #[serde(default)]
    asks: Vec<LighterWireLevel>,
    #[serde(default)]
    bids: Vec<LighterWireLevel>,
    nonce: i64,
    begin_nonce: i64,
    #[serde(default)]
    last_updated_at: u64,
}

#[derive(Debug, Deserialize)]
struct LighterWireEnvelope {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    timestamp: u64,
    order_book: LighterWireBook,
}

#[derive(Debug, Default)]
struct LighterBookAssembler {
    bids: HashMap<u64, PriceLevel>,
    asks: HashMap<u64, PriceLevel>,
    last_nonce: Option<i64>,
}

impl LighterBookAssembler {
    fn apply_value(&mut self, value: &Value) -> Result<OrderBook> {
        self.apply_value_at(value, now_ms())
    }

    fn apply_value_at(&mut self, value: &Value, received_timestamp_ms: u64) -> Result<OrderBook> {
        let envelope: LighterWireEnvelope = serde_json::from_value(value.clone())
            .context("failed to decode Lighter order book frame")?;
        ensure!(
            envelope.order_book.nonce >= 0 && envelope.order_book.begin_nonce >= 0,
            "Lighter order book returned a negative nonce"
        );
        match envelope.message_type.as_str() {
            "subscribed/order_book" => {
                self.bids.clear();
                self.asks.clear();
                apply_lighter_levels(&mut self.bids, envelope.order_book.bids)?;
                apply_lighter_levels(&mut self.asks, envelope.order_book.asks)?;
            }
            "update/order_book" => {
                let previous = self
                    .last_nonce
                    .context("Lighter order book update arrived before snapshot")?;
                ensure!(
                    envelope.order_book.begin_nonce == previous,
                    "Lighter order book nonce gap: begin_nonce {} != previous nonce {}",
                    envelope.order_book.begin_nonce,
                    previous
                );
                apply_lighter_levels(&mut self.bids, envelope.order_book.bids)?;
                apply_lighter_levels(&mut self.asks, envelope.order_book.asks)?;
            }
            other => bail!("unsupported Lighter order book frame {other}"),
        }
        self.last_nonce = Some(envelope.order_book.nonce);
        let mut bids: Vec<PriceLevel> = self.bids.values().copied().collect();
        let mut asks: Vec<PriceLevel> = self.asks.values().copied().collect();
        bids.sort_by(|left, right| right.price.total_cmp(&left.price));
        asks.sort_by(|left, right| left.price.total_cmp(&right.price));
        let exchange_timestamp_ms = if envelope.timestamp > 0 {
            envelope.timestamp
        } else {
            // last_updated_at is documented in microseconds.
            envelope.order_book.last_updated_at / 1_000
        };
        let book = OrderBook {
            bids,
            asks,
            exchange_timestamp_ms,
            received_timestamp_ms,
            sequence_valid: true,
        };
        book.validate()?;
        Ok(book)
    }
}

fn apply_lighter_levels(
    target: &mut HashMap<u64, PriceLevel>,
    levels: Vec<LighterWireLevel>,
) -> Result<()> {
    for level in levels {
        let parsed = parse_level_allow_zero(&level.price, &level.size)?;
        let key = parsed.price.to_bits();
        if parsed.size == 0.0 {
            target.remove(&key);
        } else {
            target.insert(key, parsed);
        }
    }
    Ok(())
}

fn should_attempt_recovery(
    last_success_ms: u64,
    last_attempt_ms: u64,
    now_ms: u64,
    recovery_after_ms: u64,
    minimum_attempt_interval_ms: u64,
) -> bool {
    now_ms.saturating_sub(last_success_ms) >= recovery_after_ms
        && now_ms.saturating_sub(last_attempt_ms) >= minimum_attempt_interval_ms
}

fn lighter_rest_book(response: LighterOrderBook, received_at_ms: u64) -> Result<OrderBook> {
    let mut bids = response
        .bids
        .iter()
        .map(|level| parse_level(&level.price, &level.remaining_base_amount))
        .collect::<Result<Vec<_>>>()?;
    let mut asks = response
        .asks
        .iter()
        .map(|level| parse_level(&level.price, &level.remaining_base_amount))
        .collect::<Result<Vec<_>>>()?;
    bids.sort_by(|left, right| right.price.total_cmp(&left.price));
    asks.sort_by(|left, right| left.price.total_cmp(&right.price));
    let book = OrderBook {
        bids,
        asks,
        // The REST endpoint does not expose a server-side snapshot time. The
        // receipt time is explicit and is never reused after a failed call.
        exchange_timestamp_ms: received_at_ms,
        received_timestamp_ms: received_at_ms,
        sequence_valid: true,
    };
    book.validate()?;
    Ok(book)
}

fn lighter_rest_context(
    market: &LighterMarket,
    funding_rate: f64,
    source: PriceSource,
    received_at_ms: u64,
) -> Result<VenueMarketContext> {
    ensure!(market.is_active_perp(), "Lighter REST market is not active");
    ensure!(
        funding_rate.is_finite(),
        "Lighter REST funding rate is invalid"
    );
    Ok(VenueMarketContext {
        mark_price: Some(market.mark_price_f64()?),
        index_price: Some(
            parse_optional_positive(Some(&market.index_price))?
                .context("Lighter REST market is missing a positive index price")?,
        ),
        funding_rate: Some(funding_rate),
        source,
        exchange_timestamp_ms: received_at_ms,
        received_timestamp_ms: received_at_ms,
    })
}

fn trade_rest_book(
    expected_coin: &str,
    wire: TradeRestBook,
    received_at_ms: u64,
) -> Result<OrderBook> {
    ensure!(
        wire.coin.eq_ignore_ascii_case(expected_coin),
        "trade REST recovery returned the wrong coin"
    );
    let parse_side = |levels: &[TradeRestLevel]| -> Result<Vec<PriceLevel>> {
        levels
            .iter()
            .map(|level| parse_level(&level.px, &level.sz))
            .collect()
    };
    let book = OrderBook {
        bids: parse_side(&wire.levels[0])?,
        asks: parse_side(&wire.levels[1])?,
        exchange_timestamp_ms: wire.time,
        received_timestamp_ms: received_at_ms,
        sequence_valid: true,
    };
    book.validate()?;
    Ok(book)
}

fn parse_lighter_context(value: &Value, source: PriceSource) -> Result<VenueMarketContext> {
    let stats = value
        .get("market_stats")
        .context("Lighter market stats frame is missing market_stats")?;
    Ok(VenueMarketContext {
        mark_price: parse_optional_positive(stats.get("mark_price").and_then(Value::as_str))?,
        index_price: parse_optional_positive(stats.get("index_price").and_then(Value::as_str))?,
        funding_rate: parse_optional_finite(
            stats.get("current_funding_rate").and_then(Value::as_str),
        )?,
        source,
        exchange_timestamp_ms: value.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
        received_timestamp_ms: now_ms(),
    })
}

fn parse_level(price: &str, size: &str) -> Result<PriceLevel> {
    let level = parse_level_allow_zero(price, size)?;
    ensure!(level.size > 0.0, "book size must be positive");
    Ok(level)
}

fn parse_level_allow_zero(price: &str, size: &str) -> Result<PriceLevel> {
    let price = price.parse::<f64>().context("invalid book price")?;
    let size = size.parse::<f64>().context("invalid book size")?;
    ensure!(
        price.is_finite() && price > 0.0,
        "book price must be positive"
    );
    ensure!(
        size.is_finite() && size >= 0.0,
        "book size cannot be negative"
    );
    Ok(PriceLevel { price, size })
}

fn parse_optional_positive(value: Option<&str>) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = value.parse::<f64>().context("invalid positive decimal")?;
    ensure!(parsed.is_finite() && parsed > 0.0, "price must be positive");
    Ok(Some(parsed))
}

fn parse_optional_finite(value: Option<&str>) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = value.parse::<f64>().context("invalid decimal")?;
    ensure!(parsed.is_finite(), "decimal must be finite");
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_book_recovery_caps_depth_and_rejects_wrong_stream_or_empty_side() {
        let mut book = OrderBook {
            bids: (0..20).map(|i|PriceLevel{price:1600.0-i as f64,size:0.001}).collect(),
            asks: (0..20).map(|i|PriceLevel{price:1601.0+i as f64,size:0.001}).collect(),
            exchange_timestamp_ms:1000, received_timestamp_ms:1001, sequence_valid:true,
        };
        assert!(TradeBookMode::Standard.validate_stream_book(&book).is_ok());
        assert!(TradeBookMode::Fast.validate_stream_book(&book).is_err());
        TradeBookMode::Fast.limit_recovery_book(&mut book);
        assert_eq!((book.bids.len(),book.asks.len()), (5,5));
        assert_eq!(book.bids.last().unwrap().price,1596.0);
        assert_eq!(book.asks.last().unwrap().price,1605.0);
        assert!(TradeBookMode::Fast.validate_stream_book(&book).is_ok());
        book.asks.clear();
        assert!(TradeBookMode::Fast.validate_stream_book(&book).is_err());
    }

    #[tokio::test]
    #[ignore = "Public read-only network probe; does not launch any strategy or account worker"]
    async fn fast_entropy_public_feed_smoke() {
        let mut rx = spawn_trade_fast_market_stream("mainnet", "io:OAI".into(), PriceSource::Unknown,
            MarketStreamConfig::default()).unwrap();
        let deadline = tokio::time::Instant::now()+Duration::from_secs(45);
        let mut arrivals = vec![];
        let mut contexts = 0;
        let mut reconnects = 0;
        while arrivals.len()<60 {
            let Ok(Some(event))=tokio::time::timeout_at(deadline,rx.recv()).await else {break;};
            match event {
                MarketDataEvent::Book {book,..} => {
                    assert!(TradeBookMode::Fast.validate_stream_book(&book).is_ok());
                    assert!(book.bids.len()<=5 && book.asks.len()<=5);
                    arrivals.push(Instant::now());
                },
                MarketDataEvent::Context {..} => contexts+=1,
                MarketDataEvent::Disconnected {..} => reconnects+=1,
                _=>{},
            }
        }
        drop(rx);
        assert!(arrivals.len()>=20,"too few fast snapshots: {}",arrivals.len());
        let mut gaps:Vec<_>=arrivals.windows(2).map(|w|w[1].duration_since(w[0]).as_millis()).collect();
        gaps.sort();
        println!("FAST_FEED_PROBE {}",json!({"read_only":true,"strategy_started":false,
            "book_messages":arrivals.len(),"median_ms":gaps[gaps.len()/2],
            "p90_ms":gaps[gaps.len()*9/10],"max_ms":gaps.last(),"contexts":contexts,"disconnects":reconnects}));
        assert!(contexts>0,"mark-price/context subscription must still work");
        assert!(gaps[gaps.len()/2]<1500,"fast book cadence not observed");
    }

    #[derive(Debug, Deserialize)]
    struct RecordedTradeFrame {
        channel: String,
        data: Value,
    }

    #[derive(Debug, Deserialize)]
    struct RecordedTradeBook {
        coin: String,
        time: u64,
        levels: [Vec<RecordedTradeLevel>; 2],
    }

    #[derive(Debug, Deserialize)]
    struct RecordedTradeLevel {
        px: String,
        sz: String,
    }

    #[derive(Debug, PartialEq)]
    struct TradeReplay {
        books: Vec<OrderBook>,
        trades: Vec<ShadowTradePrint>,
    }

    fn replay_lighter_fixture(frames: &[Value]) -> Result<Vec<OrderBook>> {
        let mut assembler = LighterBookAssembler::default();
        frames
            .iter()
            .enumerate()
            .map(|(index, frame)| assembler.apply_value_at(frame, 9_000 + index as u64))
            .collect()
    }

    fn replay_trade_fixture(frames: &[RecordedTradeFrame]) -> Result<TradeReplay> {
        let mut replay = TradeReplay {
            books: Vec::new(),
            trades: Vec::new(),
        };
        for frame in frames {
            match frame.channel.as_str() {
                "l2Book" => {
                    let data: RecordedTradeBook = serde_json::from_value(frame.data.clone())?;
                    ensure!(
                        data.coin.eq_ignore_ascii_case("xyz:NVDA"),
                        "fixture returned the wrong trade coin"
                    );
                    let parse_side = |levels: &[RecordedTradeLevel]| -> Result<Vec<PriceLevel>> {
                        levels
                            .iter()
                            .map(|level| parse_level(&level.px, &level.sz))
                            .collect()
                    };
                    let book = OrderBook {
                        bids: parse_side(&data.levels[0])?,
                        asks: parse_side(&data.levels[1])?,
                        exchange_timestamp_ms: data.time,
                        received_timestamp_ms: 9_000 + replay.books.len() as u64,
                        sequence_valid: true,
                    };
                    book.validate()?;
                    replay.books.push(book);
                }
                "trades" => {
                    let trades: Vec<hyperliquid_rust_sdk::Trade> =
                        serde_json::from_value(frame.data.clone())?;
                    for trade in trades {
                        ensure!(
                            trade.coin.eq_ignore_ascii_case("xyz:NVDA"),
                            "fixture returned the wrong trade coin"
                        );
                        let received_timestamp_ms = trade.time;
                        replay
                            .trades
                            .push(parse_hyperliquid_trade(&trade, received_timestamp_ms)?);
                    }
                }
                other => bail!("unsupported recorded trade channel {other}"),
            }
        }
        Ok(replay)
    }

    fn snapshot() -> Value {
        json!({
            "type":"subscribed/order_book",
            "timestamp":1000,
            "order_book":{
                "asks":[{"price":"101","size":"2"},{"price":"102","size":"3"}],
                "bids":[{"price":"99","size":"2"},{"price":"98","size":"3"}],
                "nonce":10,
                "begin_nonce":1,
                "last_updated_at":1000000
            }
        })
    }

    #[test]
    fn lighter_snapshot_delta_and_zero_size_delete_reconstruct_book() {
        let mut assembler = LighterBookAssembler::default();
        let initial = assembler.apply_value(&snapshot()).unwrap();
        assert_eq!(initial.asks[0].price, 101.0);
        let updated = assembler
            .apply_value(&json!({
                "type":"update/order_book",
                "timestamp":1050,
                "order_book":{
                    "asks":[{"price":"101.0","size":"0"},{"price":"100.5","size":"1"}],
                    "bids":[{"price":"99","size":"4"}],
                    "nonce":12,
                    "begin_nonce":10,
                    "last_updated_at":1050000
                }
            }))
            .unwrap();
        assert_eq!(updated.asks[0].price, 100.5);
        assert!(updated.asks.iter().all(|level| level.price != 101.0));
        assert_eq!(updated.bids[0].size, 4.0);
    }

    #[test]
    fn clean_lighter_remote_close_reconnects_without_fixed_delay() {
        let clean = anyhow::Error::new(LighterRemoteClosed {
            close_code: Some(1000),
            session_lifetime_ms: 120_000,
        });
        assert_eq!(lighter_reconnect_delay_ms(&clean, 1_000), 0);
        let unstable = anyhow::Error::new(LighterRemoteClosed {
            close_code: None,
            session_lifetime_ms: 100,
        });
        assert_eq!(lighter_reconnect_delay_ms(&unstable, 1_000), 1_000);
        let transport = anyhow::anyhow!("TLS handshake failed");
        assert_eq!(lighter_reconnect_delay_ms(&transport, 1_000), 1_000);
    }

    #[test]
    fn public_recovery_requires_staleness_and_independent_attempt_cooldown() {
        assert!(!should_attempt_recovery(95_000, 0, 100_000, 10_000, 15_000));
        assert!(should_attempt_recovery(90_000, 0, 100_000, 10_000, 15_000));
        assert!(!should_attempt_recovery(
            80_000, 90_001, 100_000, 10_000, 15_000
        ));
        assert!(should_attempt_recovery(
            80_000, 85_000, 100_000, 10_000, 15_000
        ));
    }

    #[test]
    fn trade_reconnect_cycle_is_fast_but_connection_rate_bounded() {
        assert_eq!(trade_reconnect_delay_ms(0, 250), 4_000);
        assert_eq!(trade_reconnect_delay_ms(1_000, 250), 3_000);
        assert_eq!(trade_reconnect_delay_ms(4_000, 250), 250);
        assert_eq!(trade_reconnect_delay_ms(10_000, 1_000), 1_000);
        assert!(
            6_u64.saturating_mul(
                TRADE_SESSION_SETUP_TIMEOUT_MS
                    + trade_reconnect_delay_ms(TRADE_SESSION_SETUP_TIMEOUT_MS, 250)
            ) < 30_000
        );
    }

    #[test]
    fn lighter_nonce_gap_fails_closed() {
        let mut assembler = LighterBookAssembler::default();
        assembler.apply_value(&snapshot()).unwrap();
        let error = assembler.apply_value(&json!({
            "type":"update/order_book",
            "timestamp":1050,
            "order_book":{"asks":[],"bids":[],"nonce":13,"begin_nonce":11,"last_updated_at":1050000}
        })).unwrap_err();
        assert!(error.to_string().contains("nonce gap"));
    }

    #[test]
    fn lighter_update_before_snapshot_is_rejected() {
        let mut assembler = LighterBookAssembler::default();
        assert!(assembler.apply_value(&json!({
            "type":"update/order_book",
            "timestamp":1050,
            "order_book":{"asks":[],"bids":[],"nonce":2,"begin_nonce":1,"last_updated_at":1050000}
        })).is_err());
    }

    #[test]
    fn trade_aggressor_notation_is_mapped_and_identity_is_stable() {
        let buy = parse_hyperliquid_trade(
            &hyperliquid_rust_sdk::Trade {
                coin: "xyz:NVDA".into(),
                side: "B".into(),
                px: "200.5".into(),
                sz: "0.25".into(),
                time: 123,
                hash: "0xabc".into(),
                tid: 456,
            },
            124,
        )
        .unwrap();
        assert_eq!(buy.aggressor, AggressorSide::Buy);
        assert_eq!(buy.trade_id, "123:xyz:NVDA:456");
        assert_eq!(buy.received_timestamp_ms, 124);

        let mut sell = hyperliquid_rust_sdk::Trade {
            coin: "xyz:NVDA".into(),
            side: "?".into(),
            px: "200.5".into(),
            sz: "0.25".into(),
            time: 123,
            hash: "0xabc".into(),
            tid: 457,
        };
        assert!(parse_hyperliquid_trade(&sell, 124).is_err());
        sell.side = "A".into();
        assert_eq!(
            parse_hyperliquid_trade(&sell, 124).unwrap().aggressor,
            AggressorSide::Sell
        );
    }

    #[test]
    fn recorded_lighter_raw_frames_replay_deterministically_and_gap_fails_closed() {
        let frames: Vec<Value> = serde_json::from_str(include_str!(
            "../../tests/fixtures/crossvenue_a/lighter_orderbook.json"
        ))
        .unwrap();
        let first = replay_lighter_fixture(&frames).unwrap();
        let second = replay_lighter_fixture(&frames).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.last().unwrap().bids[0].price, 99.5);
        assert_eq!(first.last().unwrap().asks[0].price, 100.5);

        let mut gap = frames;
        gap[2]["order_book"]["begin_nonce"] = json!(102);
        assert!(
            replay_lighter_fixture(&gap)
                .unwrap_err()
                .to_string()
                .contains("nonce gap")
        );
    }

    #[test]
    fn recorded_trade_raw_frames_replay_deterministically() {
        let frames: Vec<RecordedTradeFrame> = serde_json::from_str(include_str!(
            "../../tests/fixtures/crossvenue_a/trade_market.json"
        ))
        .unwrap();
        let first = replay_trade_fixture(&frames).unwrap();
        let second = replay_trade_fixture(&frames).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.books[0].bids[0].price, 99.0);
        assert_eq!(first.books[0].asks[0].price, 101.0);
        assert_eq!(first.trades.len(), 2);
        assert_eq!(first.trades[0].trade_id, "1724000000050:xyz:NVDA:7001");
        assert_eq!(first.trades[1].aggressor, AggressorSide::Sell);
    }
}
