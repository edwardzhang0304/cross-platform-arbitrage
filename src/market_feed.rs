//! One public subscription/cache per instrument. Contains no account identities or credentials.
use crate::{crossvenue_a::{MarketDataEvent,MarketStreamConfig,MarketVenue,PriceSource,spawn_lighter_market_stream,spawn_trade_fast_market_stream},
    lighter::{LighterClient,LighterEnvironment},openai_inventory::{Book,Level,MarketPair,liquidation}};
use anyhow::{Result,Context,ensure};
use rust_decimal::Decimal;
use std::{collections::BTreeMap,str::FromStr,sync::{Arc,RwLock,Weak,OnceLock}};
use tokio::sync::Mutex;

pub struct PublicMarketFeed {
    pub market:MarketPair,
    pub books:Arc<RwLock<[Book;2]>>,
    pub marks:Arc<RwLock<[liquidation::Mark;2]>>,
    pub transport:Arc<RwLock<[crate::crossvenue_a::MarketStreamHealth;2]>>,
    pub maintenance_rates:[Decimal;2],
    pub max_leverage:u32,
    tasks:Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for PublicMarketFeed {fn drop(&mut self){for task in &self.tasks {task.abort();}}}
#[derive(Default)]
struct FeedSlot {feed:RwLock<Weak<PublicMarketFeed>>,connect:Mutex<()>}
#[derive(Default)]
struct FeedPool {feeds:RwLock<BTreeMap<MarketPair,Arc<FeedSlot>>>}
impl FeedPool {
    async fn get_or_create<F,Fut>(&self,pair:MarketPair,create:F)->Result<Arc<PublicMarketFeed>>
    where F:FnOnce()->Fut,Fut:std::future::Future<Output=Result<PublicMarketFeed>> {
        // Only a first connection for this instrument waits. Health/diagnostics
        // and other instruments must not wait on a venue's metadata HTTP call.
        let slot=self.feeds.write().unwrap().entry(pair).or_default().clone();
        if let Some(feed)=slot.feed.read().unwrap().upgrade() {return Ok(feed);}
        let _connect=slot.connect.lock().await;
        if let Some(feed)=slot.feed.read().unwrap().upgrade() {return Ok(feed);}
        let feed=Arc::new(create().await?);ensure!(feed.market==pair,"public feed instrument mismatch");
        *slot.feed.write().unwrap()=Arc::downgrade(&feed);Ok(feed)
    }
}
fn pool()->&'static FeedPool {static POOL:OnceLock<FeedPool>=OnceLock::new();POOL.get_or_init(FeedPool::default)}
pub async fn acquire(pair:MarketPair,prior_transport:[crate::crossvenue_a::MarketStreamHealth;2])->Result<Arc<PublicMarketFeed>> {
    let history=prior_transport.clone();
    let feed=pool().get_or_create(pair,||connect(pair,prior_transport)).await?;
    merge_transport_history(&feed,&history);
    Ok(feed)
}
fn merge_transport_history(feed:&PublicMarketFeed,history:&[crate::crossvenue_a::MarketStreamHealth;2]) {
    // A second subscriber can have newer persisted incident evidence. Merge
    // it before saving the shared feed back into either ledger; never import
    // its old connection flags, books or marks into the live public source.
    for (current,old) in feed.transport.write().unwrap().iter_mut().zip(history) {current.preserve_history(old);}
}
pub async fn diagnostics()->serde_json::Value {
    let feeds=pool().feeds.read().unwrap();
    serde_json::json!(feeds.iter().filter_map(|(market,slot)|slot.feed.read().unwrap().upgrade().map(|feed|serde_json::json!({
        "market":market,"consumers":Arc::strong_count(&feed).saturating_sub(1),"public_connections":2,
        "received_ms":feed.books.read().unwrap().each_ref().map(|b|b.received_ms)
    }))).collect::<Vec<_>>())
}
fn validate_entropy_feed_contract(pair:MarketPair,asset:&crate::hyperliquid::DexAssetMeta)->Result<()> {
    ensure!(asset.name == pair.entropy_symbol() && asset.sz_decimals == 3
        && pair.supports_entropy_margin_mode(asset.margin_mode.as_deref())
        && asset.only_isolated != Some(false)
        && asset.max_leverage.is_some_and(|v| v >= 3),
        "Entropy instrument precision/margin/leverage metadata drift");
    Ok(())
}
async fn connect(pair:MarketPair,prior_transport:[crate::crossvenue_a::MarketStreamHealth;2])->Result<PublicMarketFeed> {
        let client = LighterClient::official(LighterEnvironment::Robinhood)?;
        let market = client.market_by_symbol(pair.lighter_symbol()).await?;
        pair.validate_lighter(&market)?;
        ensure!(
            market.is_active_perp() && market.market_id == pair.lighter_market_id() && market.effective_size_decimals() == pair.quantity_decimals(),
            "Lighter market metadata drift"
        );
        let meta = crate::hyperliquid::fetch_xyz_market_snapshot_cached("mainnet", "io", 0).await?;
        let asset = meta
            .meta
            .universe
            .iter()
            .find(|x| x.name == pair.entropy_symbol())
            .context("Entropy instrument missing")?;
        validate_entropy_feed_contract(pair,asset)?;
        let books = Arc::new(RwLock::new([Book::default(), Book::default()]));
        let marks=Arc::new(RwLock::new([liquidation::Mark::default(),liquidation::Mark::default()]));
        let receivers = [
            spawn_lighter_market_stream(
                client.endpoints().clone(),
                market.market_id,
                PriceSource::Unknown,
                MarketStreamConfig::default(),
            )?,
            spawn_trade_fast_market_stream(
                "mainnet",
                pair.entropy_symbol().into(),
                PriceSource::Unknown,
                MarketStreamConfig::default(),
            )?,
        ];
        let mut prior_transport=prior_transport;
        for h in &mut prior_transport {
            let mut fresh=crate::crossvenue_a::MarketStreamHealth::default();
            fresh.preserve_history(h);*h=fresh;
        }
        let transport = Arc::new(RwLock::new(prior_transport));
        let mut feed_tasks = vec![];
        for mut rx in receivers {
            let shared = books.clone();
            let shared_marks=marks.clone();
            let transport = transport.clone();
            feed_tasks.push(tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    match event {
                        MarketDataEvent::Connected {venue,..} => {
                            let i=if venue==MarketVenue::Lighter{0}else{1};
                            transport.write().unwrap()[i].ws_connected=true;
                        }
                        MarketDataEvent::Transport { venue, mut health } => {
                            let i=if venue==MarketVenue::Lighter{0}else{1};
                            if !health.ws_connected { shared_marks.write().unwrap()[i].connected=false; }
                            let mut stats=transport.write().unwrap();
                            health.preserve_history(&stats[i]); stats[i]=health;
                        }
                        MarketDataEvent::Context {venue,context} => {
                            let i=if venue==MarketVenue::Lighter{0}else{1};
                            shared_marks.write().unwrap()[i]=liquidation::Mark {
                                price:context.mark_price.and_then(|p|Decimal::from_str(&p.to_string()).ok()).filter(|p|*p>Decimal::ZERO),
                                received_ms:context.received_timestamp_ms,connected:true,
                            };
                        }
                        MarketDataEvent::Book { venue, book } => {
                            let levels =
                                |a: Vec<crate::crossvenue_a::PriceLevel>| -> Result<Vec<Level>> {
                                    a.into_iter()
                                        .map(|l| {
                                            let price = Decimal::from_str(&l.price.to_string())?;
                                            let units = pair.depth_units(Decimal::from_str(&l.size.to_string())?)?;
                                            Ok(Level { price, units })
                                        })
                                        .filter(|x| !matches!(x,Ok(l) if l.units==0))
                                        .collect()
                                };
                            let idx = if venue == MarketVenue::Lighter { 0 } else { 1 };
                            if venue==MarketVenue::Lighter {
                                // This adapter may also publish REST recovery;
                                // do not mislabel those books as websocket reads.
                                transport.write().unwrap()[idx].source="venue_feed".into();
                            }
                            if let (Ok(bids), Ok(asks)) = (levels(book.bids), levels(book.asks)) {
                                shared.write().unwrap()[idx] = Book {
                                    bids,
                                    asks,
                                    received_ms: book.received_timestamp_ms,
                                    connected: book.sequence_valid,
                                };
                            } else {
                                shared.write().unwrap()[idx].connected = false;
                            }
                        }
                        MarketDataEvent::Gap { venue, detail, received_at_ms }
                        | MarketDataEvent::Disconnected { venue, detail, received_at_ms } => {
                            let i=if venue==MarketVenue::Lighter{0}else{1};
                            let mut stats=transport.write().unwrap();
                            stats[i].ws_connected=false; stats[i].reconnect_count+=1;
                            stats[i].incident(received_at_ms,"ws_reconnect",&detail);
                            shared_marks.write().unwrap()[if venue==MarketVenue::Lighter{0}else{1}].connected=false;
                            shared.write().unwrap()
                                [if venue == MarketVenue::Lighter { 0 } else { 1 }]
                            .connected = false
                        }
                        _ => {}
                    }
                }
            }));
        }
        Ok(PublicMarketFeed{market:pair,books,marks,transport,
            maintenance_rates:[Decimal::from(market.maintenance_margin_fraction)/Decimal::from(10_000),
                Decimal::ONE/Decimal::from(asset.max_leverage.context("missing maximum leverage")?*2)],
            max_leverage:asset.max_leverage.context("missing maximum leverage")?,tasks:feed_tasks})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_feed_accepts_both_isolated_modes_but_keeps_precision_and_leverage_gates() {
        for market in [MarketPair::Openai,MarketPair::Anth] {
            for mode in ["noCross","strictIsolated"] {
                let mut asset:crate::hyperliquid::DexAssetMeta=serde_json::from_value(serde_json::json!({
                    "name":market.entropy_symbol(),"szDecimals":3,"maxLeverage":6,"marginMode":mode,"onlyIsolated":true})).unwrap();
                validate_entropy_feed_contract(market,&asset).unwrap();
                asset.max_leverage=Some(2);assert!(validate_entropy_feed_contract(market,&asset).is_err());
                asset.max_leverage=Some(6);asset.sz_decimals=4;assert!(validate_entropy_feed_contract(market,&asset).is_err());
                asset.sz_decimals=3;asset.margin_mode=Some("normal".into());assert!(validate_entropy_feed_contract(market,&asset).is_err());
                asset.margin_mode=None;assert!(validate_entropy_feed_contract(market,&asset).is_err());
                asset.margin_mode=Some(mode.into());asset.only_isolated=Some(false);assert!(validate_entropy_feed_contract(market,&asset).is_err());
            }
        }
    }
    fn fake(pair:MarketPair)->PublicMarketFeed {PublicMarketFeed{market:pair,
        books:Arc::new(RwLock::new(Default::default())),marks:Arc::new(RwLock::new(Default::default())),transport:Arc::new(RwLock::new(Default::default())),
        maintenance_rates:[Decimal::ZERO;2],max_leverage:3,tasks:vec![]}}
    #[test]
    fn later_subscriber_preserves_its_incidents_without_replacing_live_health() {
        let feed=fake(MarketPair::Openai);
        feed.transport.write().unwrap()[0].ws_connected=true;
        feed.transport.write().unwrap()[0].incident(100,"ws_reconnect","first saved cause");
        let mut prior:[crate::crossvenue_a::MarketStreamHealth;2]=Default::default();
        prior[0].incident(200,"ws_reconnect","second saved cause");
        merge_transport_history(&feed,&prior);merge_transport_history(&feed,&prior);
        let health=feed.transport.read().unwrap();assert!(health[0].ws_connected);assert_eq!(health[0].history.len(),2);
        assert_eq!(health[0].history.back().unwrap().at_ms,200);
    }
    #[tokio::test]
    async fn concurrent_same_instrument_loads_connect_once_and_share_book_health() {
        let pool=FeedPool::default();let calls=std::sync::atomic::AtomicUsize::new(0);
        let create=||async {calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);tokio::task::yield_now().await;Ok(fake(MarketPair::Openai))};
        let (a,b)=tokio::join!(pool.get_or_create(MarketPair::Openai,create),pool.get_or_create(MarketPair::Openai,create));
        let (a,b)=(a.unwrap(),b.unwrap());assert!(Arc::ptr_eq(&a,&b));assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),1);
        a.books.write().unwrap()[0].received_ms=123;a.books.write().unwrap()[0].connected=true;
        assert_eq!(b.books.read().unwrap()[0].received_ms,123);assert!(b.books.read().unwrap()[0].connected);
        a.books.write().unwrap()[0].connected=false;assert!(!b.books.read().unwrap()[0].connected);
        let c=pool.get_or_create(MarketPair::Anth,||async{Ok(fake(MarketPair::Anth))}).await.unwrap();assert!(!Arc::ptr_eq(&a,&c));assert_eq!(c.books.read().unwrap()[0].received_ms,0);
    }
    #[tokio::test]
    async fn subscription_lives_until_last_consumer_then_reconnects() {
        let pool=FeedPool::default();let (started,ready)=tokio::sync::oneshot::channel();
        let a=pool.get_or_create(MarketPair::Openai,||async {
            let mut f=fake(MarketPair::Openai);f.tasks.push(tokio::spawn(async move{let _=started.send(());std::future::pending::<()>().await;}));Ok(f)
        }).await.unwrap();ready.await.unwrap();
        let task=a.tasks[0].abort_handle();let weak=Arc::downgrade(&a);
        let b=pool.get_or_create(MarketPair::Openai,||async{panic!("second account must reuse feed")}).await.unwrap();
        drop(a);tokio::task::yield_now().await;assert!(!task.is_finished());assert!(weak.upgrade().is_some());
        drop(b);tokio::task::yield_now().await;assert!(task.is_finished());assert!(weak.upgrade().is_none());
        let next=pool.get_or_create(MarketPair::Openai,||async{Ok(fake(MarketPair::Openai))}).await.unwrap();assert_eq!(next.market,MarketPair::Openai);
    }
    #[tokio::test]
    async fn failed_or_wrong_market_connect_is_not_cached_and_other_market_remains_available() {
        let pool=FeedPool::default();
        assert!(pool.get_or_create(MarketPair::Openai,||async{anyhow::bail!("synthetic network failure")}).await.is_err());
        assert!(pool.get_or_create(MarketPair::Openai,||async{Ok(fake(MarketPair::Anth))}).await.is_err());
        let waiting=pool.get_or_create(MarketPair::Openai,||async{std::future::pending::<Result<PublicMarketFeed>>().await});tokio::pin!(waiting);
        tokio::select!{_=&mut waiting=>panic!("must wait"),_ = tokio::task::yield_now()=>{}}
        let anth=tokio::time::timeout(std::time::Duration::from_secs(1),pool.get_or_create(MarketPair::Anth,||async{Ok(fake(MarketPair::Anth))})).await.unwrap().unwrap();
        assert_eq!(anth.market,MarketPair::Anth);
        assert_eq!(pool.feeds.read().unwrap().len(),2,"diagnostics map remains readable during connection");
    }
}
