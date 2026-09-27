//! Paper-only isolated margin and post-liquidation emergency exit.
//! The simulator is a full-close approximation, not either venue's liquidation engine.
use super::{store::Store, venue::AccountWorker, *};
use anyhow::{ensure, Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mark {
    pub price: Option<Decimal>,
    pub received_ms: u64,
    pub connected: bool,
}
impl Mark {
    pub fn valid(&self, now: u64, ttl: u64) -> Option<Decimal> {
        self.price.filter(|p| *p>Decimal::ZERO && self.connected && self.received_ms<=now && now-self.received_ms<=ttl)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationSpec {
    pub maintenance_rates: [Decimal; 2],
    pub liquidation_fee_rates: [Decimal; 2],
    pub mark_max_age_ms: u64,
}
impl IsolationSpec {
    pub fn validate(&self, c: &InventoryConfig) -> Result<()> {
        ensure!(c.mode==Mode::Paper, "liquidation protection requires paper mode");
        ensure!((100..=5000).contains(&self.mark_max_age_ms), "invalid mark freshness limit");
        ensure!(self.maintenance_rates.iter().all(|r| *r>Decimal::ZERO && *r<Decimal::ONE/Decimal::from(c.leverage)), "invalid maintenance rates");
        ensure!(self.liquidation_fee_rates.iter().all(|r| *r>=Decimal::ZERO && *r<=Decimal::new(1,2)), "invalid liquidation fees");
        Ok(())
    }
}

/// Collateral excludes unrealized PnL; unused wallet cash never rescues this position.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaperIsolation {
    pub collateral: Decimal,
    pub events: Vec<Fill>,
}
impl PaperIsolation {
    pub fn apply(&mut self, before: &Position, f: &Fill, leverage: u32) -> Result<()> { self.apply_for(before,f,leverage,MarketPair::Openai) }
    pub fn apply_for(&mut self, before: &Position, f: &Fill, leverage: u32, market:MarketPair) -> Result<()> {
        if before.units==0 || before.units.signum()==f.side.sign() {
            ensure!(self.events.is_empty(), "paper liquidation latch blocks new exposure");
            self.collateral+=market.quantity(f.units)*f.price/Decimal::from(leverage)-f.fee;
        } else {
            ensure!(f.units<=before.units.abs(), "isolated close cannot flip position");
            self.collateral*=Decimal::from(before.units.abs()-f.units)/Decimal::from(before.units.abs());
        }
        Ok(())
    }
    pub fn liquidation_price(&self, p:&Position, rate:Decimal) -> Option<Decimal> { self.liquidation_price_for(p,rate,MarketPair::Openai) }
    pub fn liquidation_price_for(&self, p:&Position, rate:Decimal, market:MarketPair) -> Option<Decimal> {
        if p.units==0 {return None;}
        let sign=Decimal::from(p.units.signum());
        let price=(p.average-sign*self.collateral/market.quantity(p.units.abs()))/(Decimal::ONE-sign*rate);
        (price>Decimal::ZERO).then_some(price)
    }
    pub fn breached(&self,p:&Position,mark:Decimal,rate:Decimal)->bool { self.breached_for(p,mark,rate,MarketPair::Openai) }
    pub fn breached_for(&self,p:&Position,mark:Decimal,rate:Decimal,market:MarketPair)->bool {
        p.units!=0 && self.collateral+p.unrealized_for(mark,market)<=market.quantity(p.units.abs())*mark*rate
    }
    pub fn forced_fill(&self,p:&Position,venue:Venue,mark:Decimal,rate:Decimal,now:u64)->Result<Fill> { self.forced_fill_for(p,venue,mark,rate,now,MarketPair::Openai) }
    pub fn forced_fill_for(&self,p:&Position,venue:Venue,mark:Decimal,rate:Decimal,now:u64,market:MarketPair)->Result<Fill> {
        ensure!(p.units!=0 && mark>Decimal::ZERO,"invalid liquidation position/mark");
        let q=market.quantity(p.units.abs());
        // Model insurance/backstop absorption beyond the position's isolated collateral.
        // Never debit the unused wallet balance following a price gap.
        let bankruptcy=if p.units>0 {(p.average-self.collateral/q)/(Decimal::ONE-rate)}
            else {(p.average+self.collateral/q)/(Decimal::ONE+rate)};
        let price=if p.units>0 {mark.max(bankruptcy)} else {mark.min(bankruptcy)};
        ensure!(price>Decimal::ZERO,"invalid simulated liquidation price");
        let fee=(q*price*rate).min((self.collateral+p.unrealized_for(price,market)).max(Decimal::ZERO));
        let id=format!("paper-liquidation-{:?}-{}",venue,self.events.len());
        Ok(Fill{id:id.clone(),order_id:id,venue,side:if p.units>0{Side::Sell}else{Side::Buy},
            units:p.units.abs(),price,fee,time_ms:now})
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmergencyOrder {
    pub request: OrderRequest,
    pub terminal: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Protection {
    pub triggered_ms: u64,
    pub affected: Vec<Venue>,
    pub orders: [Option<EmergencyOrder>;2],
    pub attempt: u64,
    pub completed_ms: Option<u64>,
    pub warning: String,
}

fn ingest(s:&mut Snapshot, fills:&[Fill], reducing:bool, request:Option<&OrderRequest>)->Result<()> {
    let mut next=s.clone();
    for f in fills {
        let key=format!("{:?}:{}",f.venue,f.id);
        if let Some(old)=next.fills.get(&key) {ensure!(old==f,"changed duplicate liquidation/exit fill");continue;}
        ensure!(f.time_ms>=s.created_ms && f.price>Decimal::ZERO && f.fee>=Decimal::ZERO,"invalid protection fill");
        if let Some(r)=request {ensure!(f.order_id==r.id && f.venue==r.venue && f.side==r.side,"unowned protection fill");}
        if reducing {
            let p=&next.positions[f.venue.index()];
            ensure!(p.units.signum()!=f.side.sign() && f.units>0 && f.units<=p.units.abs(),"protection fill exceeds owned exposure");
        }
        next.record_fill(f,request.and_then(|r|r.arrival_mid))?;
    }
    *s=next;Ok(())
}

async fn resolve_existing(s:&mut Snapshot,workers:&[AccountWorker;2])->Result<bool> {
    // A crash can leave acknowledged paper fills only in the remote journal. Query the
    // original IDs; never dispatch an unsent hedge after a liquidation notification.
    let Some(op)=s.pending.clone() else {return Ok(true);};
    for r in [&op.first,&op.hedge,&op.repair,&op.unwind_hedge,&op.align_close].into_iter().flatten() {
        let result=workers[r.venue.index()].lookup(r.clone()).await?;
        ingest(s,&result.fills,r.reduce_only,Some(r))?;
        if !result.terminal {return Ok(false);}
    }
    s.pending=None;Ok(true)
}

fn request(s:&Snapshot,b:&Book,venue:Venue,now:u64)->Result<OrderRequest> {
    b.validate(now,s.config.book_max_age_ms)?;
    let p=&s.positions[venue.index()];ensure!(p.units!=0,"no remaining protective position");
    let side=if p.units>0{Side::Sell}else{Side::Buy};
    let levels=if side==Side::Buy{&b.asks}else{&b.bids};
    let limit=levels[0].price*(Decimal::ONE+Decimal::from(side.sign())*s.config.execution_slippage_bps/Decimal::from(10_000));
    let available:i64=levels.iter().take_while(|l|if side==Side::Buy{l.price<=limit}else{l.price>=limit}).map(|l|l.units).sum();
    let step=s.config.market.venue_step(venue);
    let qty=available.min(p.units.abs())/step*step;
    ensure!(qty>0,"protective close waiting for executable depth");
    let guard=s.liquidation_protection.as_ref().context("missing liquidation latch")?;
    Ok(OrderRequest{id:format!("{}-liq-exit-{}-{:?}",s.instance_id,guard.attempt,venue),venue,side,units:qty,limit,
        arrival_mid:b.mid(),reduce_only:true,created_ms:now,expires_ms:now+s.config.operation_timeout_ms,
        signed_expires_ms:None})
}

/// Called before ordinary signals, including when the ordinary strategy is halted.
/// Returns true for a latched incident: ordinary logic must never run afterwards.
pub async fn protect(s:&mut Snapshot,store:&mut Store,workers:&[AccountWorker;2],books:&[Book;2],now:u64)->Result<bool> {
    ensure!(s.config.mode==Mode::Paper,"protection scope violation");
    if s.liquidation_protection.as_ref().is_some_and(|p|p.completed_ms.is_some()) {
        ensure!(s.positions.iter().all(|p|p.units==0) && s.pending.is_none(),"completed liquidation guard has residual exposure");
        s.paused=true;s.stop_requested=true;s.status=Status::Stopped;return Ok(true);
    }
    let (l,e)=tokio::join!(workers[0].liquidations(),workers[1].liquidations());
    let events=[l?,e?];
    if events.iter().all(Vec::is_empty) && s.liquidation_protection.is_none() {return Ok(false);}
    let newly_latched=s.liquidation_protection.is_none();
    if newly_latched {
        s.liquidation_protection=Some(Protection{triggered_ms:now,affected:vec![],orders:[None,None],attempt:0,completed_ms:None,warning:String::new()});
    }
    s.paused=true;s.stop_requested=true;s.stop_after_close=true;s.recovery_after_ms=None;
    s.status=Status::RecoveringExposure;
    if newly_latched {store.commit(s,now,"liquidation_protection_latched")?;}
    let outcome=advance_protection(s,store,workers,books,&events,now).await;
    match outcome {
        Ok(())=>s.liquidation_protection.as_mut().unwrap().warning.clear(),
        Err(err)=>{
            if format!("{err:#}").contains("persistence") {return Err(err);}
            s.liquidation_protection.as_mut().unwrap().warning=err.to_string();
            s.reason=format!("liquidation protection waiting: {err}");
        }
    }
    store.commit(s,now,"sample")?;
    Ok(true)
}

async fn advance_protection(s:&mut Snapshot,store:&mut Store,workers:&[AccountWorker;2],books:&[Book;2],events:&[Vec<Fill>;2],now:u64)->Result<()> {
    ensure!(resolve_existing(s,workers).await?,"prior order outcome unknown; querying original ID");
    // Recover emergency fills before applying newer liquidation events on that venue.
    for i in 0..2 {
        if let Some(old)=s.liquidation_protection.as_ref().unwrap().orders[i].clone() {
            if !old.terminal {
                let result=workers[i].lookup(old.request.clone()).await?;
                ingest(s,&result.fills,true,Some(&old.request))?;
                s.liquidation_protection.as_mut().unwrap().orders[i].as_mut().unwrap().terminal=result.terminal;
                ensure!(result.terminal,"protective order outcome unknown; no duplicate submit");
            }
        }
    }
    for (i,fills) in events.iter().enumerate() {
        ensure!(fills.iter().all(|f|f.venue.index()==i && f.time_ms<=now && f.id.starts_with("paper-liquidation-")),"invalid paper liquidation event");
        ingest(s,fills,true,None)?;
        if !fills.is_empty() {
            let venue=if i==0{Venue::Lighter}else{Venue::Entropy};
            let guard=s.liquidation_protection.as_mut().unwrap();
            if !guard.affected.contains(&venue){guard.affected.push(venue);}
        }
    }
    store.commit(s,now,"liquidation_fills_reconciled")?;
    let (l,e)=tokio::join!(workers[0].reconcile_account(),workers[1].reconcile_account());
    let accounts=[l?,e?];
    for (i,a) in accounts.iter().enumerate() {
        ensure!(a.venue.index()==i && a.authenticated && a.account.as_str()==if i==0{s.config.lighter_account.as_str()}else{s.config.entropy_account.as_str()}
            && a.observed_ms<=now && now-a.observed_ms<=s.config.account_max_age_ms && a.open_orders==0
            && a.position_units==s.positions[i].units,"protective exit needs fresh matching owned account");
    }
    for venue in [Venue::Lighter,Venue::Entropy] {
        let i=venue.index();
        if s.positions[i].units==0 {continue;}
        s.liquidation_protection.as_mut().unwrap().attempt+=1;
        let r=request(s,&books[i],venue,now)?;
        s.liquidation_protection.as_mut().unwrap().orders[i]=Some(EmergencyOrder{request:r.clone(),terminal:false});
        store.commit(s,now,"liquidation_exit_dispatch")?;
        let result=workers[i].submit(r.clone()).await?;
        ingest(s,&result.fills,true,Some(&r))?;
        s.liquidation_protection.as_mut().unwrap().orders[i].as_mut().unwrap().terminal=result.terminal;
        store.commit(s,now,"liquidation_exit_result")?;
    }
    let terminal=s.liquidation_protection.as_ref().unwrap().orders.iter().flatten().all(|x|x.terminal);
    if terminal && s.positions.iter().all(|p|p.units==0) {
        // Worker account evidence, not merely an order acknowledgement, confirms flatness.
        let (l,e)=tokio::join!(workers[0].reconcile_account(),workers[1].reconcile_account());
        ensure!([l?,e?].iter().all(|a|a.authenticated && a.position_units==0 && a.open_orders==0),"flat confirmation pending");
        s.closed_groups+=s.lots.len() as u64;s.lots.clear();s.previous_group_exit.clear();s.anchor=None;
        s.close_requested=false;s.exit_batch_active=false;s.status=Status::Stopped;
        s.reason="liquidation protection completed; both venues flat; manual review before restart".into();
        let guard=s.liquidation_protection.as_mut().unwrap();guard.completed_ms.get_or_insert(now);
    } else {s.reason="liquidation protection: closing remaining exposure; ordinary trading disabled".into();}
    Ok(())
}
