use super::*;
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
#[cfg(any(test, feature="paper-runtime"))]
use std::sync::RwLock;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
};
use tokio::sync::{mpsc, oneshot};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub trait VenueBackend: Send + 'static {
    fn security_ready(&self) -> bool {
        true
    }
    fn lease(&self) -> Option<Arc<dyn Fn() -> bool + Send + Sync>> {
        None
    }
    fn prepare(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn reconcile_account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        self.account()
    }
    /// Paper risk events only. Live adapters must not infer liquidation from a missing position.
    fn liquidations(&mut self) -> BoxFuture<'_, Vec<Fill>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult>;
    fn lookup(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult>;
    fn funding(&mut self, _start: u64, _end: u64) -> BoxFuture<'_, Vec<Funding>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn account(&mut self) -> BoxFuture<'_, AccountEvidence>;
}
enum Message {
    Liquidations(oneshot::Sender<Result<Vec<Fill>>>),
    Submit(OrderRequest, oneshot::Sender<Result<OrderResult>>),
    Lookup(OrderRequest, oneshot::Sender<Result<OrderResult>>),
    Funding(u64, u64, oneshot::Sender<Result<Vec<Funding>>>),
    Account(bool, oneshot::Sender<Result<AccountEvidence>>),
}
#[derive(Clone)]
pub struct AccountWorker {
    tx: mpsc::Sender<Message>,
}
impl AccountWorker {
    pub fn spawn(
        venue: Venue,
        mode: Mode,
        process_dry_run: bool,
        backend: Box<dyn VenueBackend>,
    ) -> Result<Self> {
        Self::spawn_with_clock(venue, mode, process_dry_run, backend, None)
    }
    /// Only a concrete paper backend may use recorded historical request time.
    /// Real workers always keep the wall-clock expiry gate in `spawn`.
    #[cfg(any(test, feature="paper-runtime"))]
    pub fn spawn_replay(venue: Venue, backend: PaperBackend,
        clock: Arc<std::sync::atomic::AtomicU64>) -> Result<Self> {
        ensure!(backend.config.mode == Mode::Paper && backend.venue == venue,
            "offline worker requires matching paper backend");
        Self::spawn_with_clock(venue, Mode::Paper, true,
            Box::new(backend.with_logical_clock(clock.clone())), Some(clock))
    }
    fn spawn_with_clock(
        venue: Venue, mode: Mode, process_dry_run: bool,
        mut backend: Box<dyn VenueBackend>,
        replay_clock: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> Result<Self> {
        ensure!(
            mode != Mode::Live || (cfg!(feature="openai-inventory-live") && !process_dry_run),
            "process dry-run blocks live account workers"
        );
        let (tx, mut rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let mut prepared = false;
            loop {
                let msg = tokio::select! {
                    msg=rx.recv()=>match msg {Some(m)=>m,None=>break},
                    _=tokio::time::sleep(std::time::Duration::from_millis(250))=>{if !backend.security_ready(){break;}continue;}
                };
                if !backend.security_ready() {
                    break;
                }
                let lease = backend.lease();
                match msg {
                    Message::Liquidations(reply) => {
                        let _=reply.send(bounded(backend.liquidations(),3000,lease.clone()).await);
                    }
                    Message::Submit(r, reply) => {
                        let now = replay_clock.as_ref().map_or_else(crate::domain::now_ms,
                            |c|c.load(std::sync::atomic::Ordering::SeqCst));
                        let result = if r.venue != venue
                            || r.units <= 0
                            || now > r.expires_ms
                            || r.limit <= Decimal::ZERO
                        {
                            Err(anyhow::anyhow!(
                                "account-worker final request check rejected"
                            ))
                        } else {
                            let deadline = r.expires_ms.saturating_sub(now).min(10_000).max(1);
                            bounded(backend.submit(r), deadline, lease.clone()).await
                        };
                        let _ = reply.send(result);
                    }
                    Message::Lookup(r, reply) => {
                        // Entropy absence reconciliation performs orderStatus followed by
                        // chain, open-order and fill queries. This is read-only;
                        // submission expiry and its deadline above stay unchanged.
                        let deadline = if venue == Venue::Entropy {
                            10_000
                        } else {
                            3000
                        };
                        let _ =
                            reply.send(bounded(backend.lookup(r), deadline, lease.clone()).await);
                    }
                    Message::Funding(start, end, reply) => {
                        let _ = reply
                            .send(bounded(backend.funding(start, end), 5000, lease.clone()).await);
                    }
                    Message::Account(force, reply) => {
                        let result = async {
                            if !prepared {
                                bounded(backend.prepare(), 10_000, lease.clone()).await?;
                                prepared = true;
                            }
                            if force {
                                bounded(
                                    backend.reconcile_account(),
                                    if venue == Venue::Entropy {
                                        10_000
                                    } else {
                                        3000
                                    },
                                    lease.clone(),
                                )
                                .await
                            } else {
                                bounded(backend.account(), 3000, lease.clone()).await
                            }
                        }
                        .await;
                        let _ = reply.send(result);
                    }
                }
            }
        });
        Ok(Self { tx })
    }
    pub(crate) async fn submit(&self, r: OrderRequest) -> Result<OrderResult> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Message::Submit(r, tx))
            .context("account worker unavailable or full")?;
        rx.await.context("account worker stopped")?
    }
    pub(crate) async fn lookup(&self, r: OrderRequest) -> Result<OrderResult> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Message::Lookup(r, tx))
            .context("account worker unavailable or full")?;
        rx.await.context("account worker stopped")?
    }
    pub async fn funding(&self, start: u64, end: u64) -> Result<Vec<Funding>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Message::Funding(start, end, tx))
            .context("account worker unavailable")?;
        rx.await.context("account worker stopped")?
    }
    pub async fn reconcile_account(&self) -> Result<AccountEvidence> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Message::Account(true, tx))
            .context("account worker unavailable")?;
        rx.await.context("account worker stopped")?
    }
    pub(crate) async fn liquidations(&self) -> Result<Vec<Fill>> {
        let (tx,rx)=oneshot::channel();
        self.tx.try_send(Message::Liquidations(tx)).context("account worker unavailable")?;
        rx.await.context("account worker stopped")?
    }
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }
    pub async fn account(&self) -> Result<AccountEvidence> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Message::Account(false, tx))
            .context("account worker unavailable or full")?;
        rx.await.context("account worker stopped")?
    }
}

/// Virtual backend; absent from ordinary live builds.
#[cfg(any(test, feature="paper-runtime"))]
pub struct PaperBackend {
    orderbook_matching: bool,
    funding_history: std::collections::BTreeMap<String,Funding>,
    isolation: Option<(super::liquidation::IsolationSpec,super::liquidation::PaperIsolation,Arc<RwLock<[super::liquidation::Mark;2]>>)>,
    logical_clock: Option<Arc<std::sync::atomic::AtomicU64>>,
    pub venue: Venue,
    pub config: InventoryConfig,
    pub position: Position,
    pub books: Arc<RwLock<[Book; 2]>>,
    pub orders: std::collections::BTreeMap<String, OrderResult>,
    journal: Option<rusqlite::Connection>,
}
#[cfg(any(test, feature="paper-runtime"))]
impl PaperBackend {
    pub fn new(
        venue: Venue,
        config: InventoryConfig,
        position: Position,
        books: Arc<RwLock<[Book; 2]>>,
    ) -> Self {
        Self {
            orderbook_matching: false,
            funding_history: Default::default(),
            isolation: None,
            logical_clock: None,
            venue,
            config,
            position,
            books,
            orders: Default::default(),
            journal: None,
        }
    }
}
#[cfg(any(test, feature="paper-runtime"))]
impl PaperBackend {
    pub fn with_orderbook_matching(mut self) -> Self { self.orderbook_matching=true; self }
    pub fn with_isolation(mut self,spec:super::liquidation::IsolationSpec,marks:Arc<RwLock<[super::liquidation::Mark;2]>>)->Result<Self> {
        use rusqlite::OptionalExtension;
        spec.validate(&self.config)?;
        let existing=if let Some(db)=&self.journal {
            db.execute_batch("CREATE TABLE IF NOT EXISTS paper_isolation(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);")?;
            db.query_row("SELECT body FROM paper_isolation WHERE id=1",[],|r|r.get::<_,String>(0)).optional()?
        }else{None};
        let isolation=if let Some(body)=existing {
            let (old,state):(super::liquidation::IsolationSpec,super::liquidation::PaperIsolation)=serde_json::from_str(&body)?;
            ensure!(old==spec,"isolated paper risk parameters changed");state
        }else {
            ensure!(self.position.units==0,"enable paper isolation only when flat; do not invent historical collateral");
            super::liquidation::PaperIsolation::default()
        };
        self.isolation=Some((spec,isolation,marks));self.persist()?;Ok(self)
    }
    fn simulate_liquidation(&mut self)->Result<()> {
        let now=self.now_ms();
        let mut changed=false;
        if let Some((spec,isolated,marks))=&mut self.isolation {
            let i=self.venue.index();
            if let Some(mark)=marks.read().unwrap()[i].valid(now,spec.mark_max_age_ms) {
                if isolated.breached_for(&self.position,mark,spec.maintenance_rates[i],self.config.market) {
                    let fill=isolated.forced_fill_for(&self.position,self.venue,mark,spec.liquidation_fee_rates[i],now,self.config.market)?;
                    self.position.apply_for(&fill, self.config.market)?;
                    isolated.collateral=Decimal::ZERO;
                    isolated.events.push(fill);
                    changed=true;
                }
            }
        }
        if changed {self.persist()?;}Ok(())
    }
    fn reject(&mut self,id:String,reason:&str)->Result<OrderResult> {
        let result=OrderResult{exchange_created_ms: None, terminal:true,fills:vec![],reason:reason.into()};
        self.orders.insert(id,result.clone());self.persist()?;Ok(result)
    }
    /// Shared frame time for paired paper experiments only; ordinary service uses wall time.
    pub fn with_logical_clock(mut self, clock: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.logical_clock=Some(clock); self
    }
    fn now_ms(&self) -> u64 {
        self.logical_clock.as_ref().map(|c|c.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or_else(crate::domain::now_ms)
    }
    pub fn durable(
        venue: Venue,
        config: InventoryConfig,
        position: Position,
        books: Arc<RwLock<[Book; 2]>>,
        path: &std::path::Path,
    ) -> Result<Self> {
        use rusqlite::OptionalExtension;
        ensure!(config.mode == Mode::Paper, "virtual journal cannot accept a live profile");
        let mut out = Self::new(venue, config, position, books);
        let db = rusqlite::Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=3000; CREATE TABLE IF NOT EXISTS paper_remote(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS profile_binding(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);")?;
        let c=&out.config;
        let identity = serde_json::to_string(&(venue,c.market,c.mode,&c.lighter_account,&c.entropy_account,
            c.lighter_account_index,&c.lighter_address,&c.entropy_address,c.leverage,c.paper_capital_per_venue,c.fee_lighter,c.fee_entropy))?;
        let old: Option<String> = db.query_row("SELECT body FROM profile_binding WHERE id=1",[],|r|r.get(0)).optional()?;
        if let Some(old) = old { ensure!(old==identity, "virtual journal profile/venue/config mismatch"); }
        else {
            let count:i64=db.query_row("SELECT COUNT(*) FROM paper_remote",[],|r|r.get(0))?;
            ensure!(count==0, "unbound virtual journal cannot be imported");
            db.execute("INSERT INTO profile_binding VALUES(1,?1)",[identity])?;
        }
        let body: Option<String> = db
            .query_row("SELECT body FROM paper_remote WHERE id=1", [], |r| r.get(0))
            .optional()?;
        if let Some(body) = body {
            (out.position, out.orders) = serde_json::from_str(&body)?;
        }
        db.execute_batch("CREATE TABLE IF NOT EXISTS virtual_funding(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);")?;
        let funding: Option<String> = db.query_row("SELECT body FROM virtual_funding WHERE id=1",[],|r|r.get(0)).optional()?;
        if let Some(body) = funding {out.funding_history=serde_json::from_str(&body)?;}
        out.journal = Some(db);
        out.persist()?;
        Ok(out)
    }
    fn persist(&self) -> Result<()> {
        self.persist_inner().context("paper persistence failure")
    }
    fn persist_inner(&self) -> Result<()> {
        if let Some(db) = &self.journal {
            let tx=db.unchecked_transaction()?;
            tx.execute("INSERT INTO paper_remote VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[serde_json::to_string(&(&self.position,&self.orders))?])?;
            if let Some((spec,state,_))=&self.isolation {
                tx.execute("INSERT INTO paper_isolation VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[serde_json::to_string(&(spec,state))?])?;
            }
            tx.execute("INSERT INTO virtual_funding VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[serde_json::to_string(&self.funding_history)?])?;
            tx.commit()?;
        }
        Ok(())
    }
}
#[cfg(any(test, feature="paper-runtime"))]
impl VenueBackend for PaperBackend {
    fn funding(&mut self,start:u64,end:u64)->BoxFuture<'_,Vec<Funding>> {
        Box::pin(async move {
            if !self.orderbook_matching { return Ok(vec![]); }
            if let Some(first)=self.orders.values().flat_map(|o|&o.fills).map(|f|f.time_ms).min() {
                let hour=paper_funding::HOUR;
                let from=self.funding_history.values().map(|f|f.time_ms).max().map(|t|t+hour).unwrap_or((first/hour+1)*hour);
                let until=end/hour*hour;
                if from<=until {
                    let values=paper_funding::per_base(self.venue,self.config.market,from,until).await?;
                    for (t,rate) in values {
                        let f=paper_funding::estimate(self.venue,self.config.market,&self.orders,t,rate)?;
                        ensure!(!self.funding_history.contains_key(&f.id),"duplicate virtual funding");
                        self.position.funding+=f.amount;
                        self.funding_history.insert(f.id.clone(),f);
                    }
                    self.persist()?;
                }
            }
            Ok(self.funding_history.values().filter(|f|f.time_ms>=start&&f.time_ms<=end).cloned().collect())
        })
    }
    fn liquidations(&mut self)->BoxFuture<'_,Vec<Fill>> {
        Box::pin(async move {
            self.simulate_liquidation()?;
            Ok(self.isolation.as_ref().map(|(_,s,_)|s.events.clone()).unwrap_or_default())
        })
    }
    fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        Box::pin(async move {
            ensure!(!self.orders.contains_key(&r.id), "duplicate paper order");
            // Risk is evaluated at each shared frame before dispatch, not halfway through
            // a paper operation. The outer B runner freezes mark and book observations.
            if !r.reduce_only {
                if let Some((spec,state,marks))=&self.isolation {
                    let reason=if !state.events.is_empty(){Some("paper liquidation latch blocks new exposure")}
                        else if marks.read().unwrap()[self.venue.index()].valid(self.now_ms(),spec.mark_max_age_ms).is_none(){Some("isolated paper mark disconnected or stale")}else{None};
                    if let Some(reason)=reason {return self.reject(r.id,reason);}
                }
            }
            let book = self.books.read().unwrap()[self.venue.index()].clone();
            book.validate(self.now_ms(), self.config.book_max_age_ms)?;
            if r.reduce_only {
                ensure!(
                    self.position.units.signum() != r.side.sign()
                        && r.units <= self.position.units.abs(),
                    "invalid reduce-only order"
                );
            }
            let mut r = r;
            if self.orderbook_matching {
                ensure!(r.venue==self.venue && r.units>0 && r.units%self.config.market.venue_step(self.venue)==0,
                    "paper venue/quantity binding rejected");
                if !r.reduce_only && (r.units<self.config.market.minimum_units(self.venue)
                    || self.config.quantity(r.units)*r.limit<Decimal::from(10)) {
                    return self.reject(r.id,"paper minimum order size");
                }
                r.limit=self.config.market.protected_price(self.venue,r.limit,r.side==Side::Buy)?;
                let levels=if r.side==Side::Buy {&book.asks} else {&book.bids};
                let available:i64=levels.iter().take_while(|l|if r.side==Side::Buy {l.price<=r.limit}else{l.price>=r.limit})
                    .map(|l|l.units).sum();
                let step=self.config.market.venue_step(self.venue);
                r.units=r.units.min(available)/step*step;
                if r.units==0 {return self.reject(r.id,"paper IOC: no protected depth");}
            }
            let (price, worst) = book.vwap(r.side, r.units)?;
            if (r.side == Side::Buy && worst > r.limit) || (r.side == Side::Sell && worst < r.limit)
            {
                let out = OrderResult {
                    exchange_created_ms: None,
                    terminal: true,
                    fills: vec![],
                    reason: "paper IOC cancelled by price".into(),
                };
                self.orders.insert(r.id, out.clone());
                self.persist()?;
                return Ok(out);
            }
            // Runtime simulation walks visible depth. The slippage setting is a
            // limit, not an extra charge on an already executable book price.
            let price = if self.orderbook_matching { price } else { price
                * (Decimal::ONE
                    + Decimal::from(r.side.sign()) * self.config.execution_slippage_bps
                        / Decimal::from(10_000)) };
            let price = if r.side == Side::Buy {
                price.min(r.limit)
            } else {
                price.max(r.limit)
            };
            let fee = self.config.quantity(r.units)
                * price
                * if self.venue == Venue::Entropy {
                    self.config.fee_entropy
                } else {
                    self.config.fee_lighter
                };
            let f = Fill {
                id: format!("paper:{}", r.id),
                order_id: r.id.clone(),
                venue: r.venue,
                side: r.side,
                units: r.units,
                price,
                fee,
                time_ms: self.now_ms(),
            };
            if let Some((_,state,_))=&mut self.isolation {
                if !r.reduce_only {
                    let free=self.config.paper_capital_per_venue+self.position.realized-self.position.fees+self.position.funding-state.collateral;
                    if free<self.config.quantity(f.units)*f.price/Decimal::from(self.config.leverage) {
                        return self.reject(r.id,"insufficient isolated paper cash");
                    }
                }
                state.apply_for(&self.position,&f,self.config.leverage,self.config.market)?;
            }
            self.position.apply_for(&f, self.config.market)?;
            let out = OrderResult {
                exchange_created_ms: None,
                terminal: true,
                fills: vec![f],
                reason: "paper fill; not a venue execution".into(),
            };
            self.orders.insert(r.id, out.clone());
            self.persist()?;
            Ok(out)
        })
    }
    fn lookup(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        Box::pin(async move {
            Ok(self.orders.get(&r.id).cloned().unwrap_or(OrderResult {
                exchange_created_ms: None,
                terminal: false,
                fills: vec![],
                reason: "paper order history unavailable after restart; reconciliation required"
                    .into(),
            }))
        })
    }
    fn account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        Box::pin(async move {
            let mid = self.books.read().unwrap()[self.venue.index()]
                .mid()
                .unwrap_or(self.position.average);
            let p = &self.position;
            let equity = self.config.paper_capital_per_venue + p.realized - p.fees
                + p.funding
                + p.unrealized_for(mid, self.config.market);
            Ok(AccountEvidence {
                venue: self.venue,
                account: if self.venue == Venue::Lighter {
                    self.config.lighter_account.clone()
                } else {
                    self.config.entropy_account.clone()
                },
                observed_ms: self.now_ms(),
                position_units: p.units,
                free_margin: self.isolation.as_ref().map(|(_,s,_)|self.config.paper_capital_per_venue+p.realized-p.fees+p.funding-s.collateral)
                    .unwrap_or(equity-self.config.quantity(p.units.abs())*mid/Decimal::from(self.config.leverage)),
                equity,
                leverage: self.config.leverage,
                isolated: true,
                open_orders: 0,
                authenticated: true,
                liquidation_price: self.isolation.as_ref().and_then(|(spec,s,_)|s.liquidation_price_for(p,spec.maintenance_rates[self.venue.index()],self.config.market)),
            })
        })
    }
}

/// Revokes and drops all signing material when the controlling Vault session expires or locks.
pub struct GuardedBackend {
    pub inner: Box<dyn VenueBackend>,
    pub lease: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl VenueBackend for GuardedBackend {
    fn security_ready(&self) -> bool {
        (self.lease)() && self.inner.security_ready()
    }
    fn lease(&self) -> Option<Arc<dyn Fn() -> bool + Send + Sync>> {
        Some(self.lease.clone())
    }
    fn prepare(&mut self) -> BoxFuture<'_, ()> {
        self.inner.prepare()
    }
    fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        self.inner.submit(r)
    }
    fn lookup(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        self.inner.lookup(r)
    }
    fn account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        self.inner.account()
    }
    fn reconcile_account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        self.inner.reconcile_account()
    }
    fn funding(&mut self, start: u64, end: u64) -> BoxFuture<'_, Vec<Funding>> {
        self.inner.funding(start, end)
    }
}

async fn bounded<T>(
    future: BoxFuture<'_, T>,
    milliseconds: u64,
    lease: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<T> {
    tokio::pin!(future);
    let timeout = tokio::time::sleep(std::time::Duration::from_millis(milliseconds));
    tokio::pin!(timeout);
    let mut check = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            value=&mut future=>return value.map_err(|e| {let message=e.to_string();if message.contains("http") || message.to_ascii_lowercase().contains("authorization") || message.contains("auth_token") {anyhow::anyhow!("venue request failed; sensitive transport details suppressed")}else{anyhow::anyhow!("{}",message.chars().take(512).collect::<String>())}}),
            _=&mut timeout=>anyhow::bail!("account worker request deadline exceeded"),
            _=check.tick()=>ensure!(lease.as_ref().is_none_or(|f|f()),"account worker security lease revoked"),
        }
    }
}

#[cfg(test)]
mod lookup_timeout_tests {
    use super::*;
    struct SlowRead;
    impl VenueBackend for SlowRead {
        fn submit(&mut self, _: OrderRequest) -> BoxFuture<'_, OrderResult> {
            panic!("must not submit")
        }
        fn account(&mut self) -> BoxFuture<'_, AccountEvidence> {
            panic!("must not fetch account")
        }
        fn lookup(&mut self, _: OrderRequest) -> BoxFuture<'_, OrderResult> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
                Ok(OrderResult {
                    exchange_created_ms: None,
                    terminal: true,
                    fills: vec![],
                    reason: "verified absence".into(),
                })
            })
        }
    }
    #[tokio::test]
    async fn expired_request_can_complete_slow_read_without_resubmission() {
        let worker =
            AccountWorker::spawn(Venue::Entropy, Mode::Paper, true, Box::new(SlowRead)).unwrap();
        let r = OrderRequest {
            id: "original-expired".into(),
            venue: Venue::Entropy,
            side: Side::Sell,
            units: 90,
            limit: Decimal::ONE,
            arrival_mid: None,
            reduce_only: false,
            created_ms: 1,
            expires_ms: 2,
            signed_expires_ms: None,
        };
        assert!(worker.lookup(r.clone()).await.unwrap().terminal);
        assert!(worker.submit(r).await.is_err());
    }
}
