use super::*;
use super::venue::{AccountWorker, PaperBackend};
use std::sync::{Arc,RwLock};
use rust_decimal::Decimal;

fn books(now:u64)->[Book;2] {std::array::from_fn(|_|Book{bids:vec![Level{price:100.into(),units:1_000_000}],asks:vec![Level{price:101.into(),units:1_000_000}],received_ms:now,connected:true})}
#[tokio::test]
async fn manual_emergency_bypasses_strategy_halts_and_flattens_both_markets() {
 for market in [MarketPair::Openai,MarketPair::Anth] {for direction in [Direction::LighterLong,Direction::LighterShort] {
    let mut c=InventoryConfig::default();c.market=market;c.direction_policy=DirectionPolicy::Both;
    let (mut db,mut s)=store::Store::offline_replay(&c).unwrap();s.direction=direction;
    let now=crate::domain::now_ms();let book=Arc::new(RwLock::new(books(now)));
    let q=market.common_step()*10;
    for v in [Venue::Lighter,Venue::Entropy] {
        s.record_fill(&Fill{id:format!("entry-{v:?}"),order_id:"lot-1".into(),venue:v,side:direction.open_side(v),units:q,price:100.into(),fee:Decimal::ZERO,time_ms:now-10},None).unwrap();
    }
    s.lots.push(Lot{id:"lot-1".into(),level:0,units:q,opened_ms:now-10,entry_spread:0.into(),entry_net_spread:Some(0.into())});s.opened_groups=1;
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|AccountWorker::spawn(v,Mode::Paper,true,Box::new(PaperBackend::new(v,c.clone(),s.positions[v.index()].clone(),book.clone()).with_orderbook_matching())).unwrap());
    s.status=Status::NeedsAttention;s.reason="unresolved residual".into();s.loss_stop=Some(LossStop{at_ms:now,net_pnl:(-30).into()});
    emergency_exit::latch(&mut s,now).unwrap();let id=s.emergency_exit.as_ref().unwrap().id.clone();emergency_exit::latch(&mut s,now).unwrap();assert_eq!(s.emergency_exit.as_ref().unwrap().id,id);
    emergency_exit::advance(&mut s,&mut db,&workers,&book).await.unwrap();
    assert_eq!(s.emergency_exit.as_ref().unwrap().remaining_units,[Some(0),Some(0)],"post-order venue confirmation");
    emergency_exit::advance(&mut s,&mut db,&workers,&book).await.unwrap();
    assert!(!emergency_exit::active(&s));assert_eq!(s.status,Status::Stopped);assert!(s.positions.iter().all(|p|p.units==0));assert_eq!(s.closed_groups,1);
    for o in &s.emergency_exit.as_ref().unwrap().orders {assert!(o.request.reduce_only);let expected=if o.request.side==Side::Buy{Decimal::new(10605,2)}else{95.into()};assert_eq!(o.request.limit,market.protected_price(o.request.venue,expected,o.request.side==Side::Buy).unwrap());}
    let report=accounting::AccountingCache::default().report(&s,&book.read().unwrap(),crate::domain::now_ms());
    assert!(report.closed_net_profit.is_some(),"emergency closed fills remain attributable");
 }}
}

use super::venue::{VenueBackend,BoxFuture};
use std::sync::Mutex;
#[derive(Default)]
struct Faults { account_down:bool, unknown:bool, lose_ack:bool, cap:Option<i64>, wrong_account:bool, stale:bool }
struct Backend {inner:PaperBackend,faults:Arc<Mutex<Faults>>,sent:Arc<Mutex<Vec<OrderRequest>>>,deferred:Option<OrderRequest>}
impl VenueBackend for Backend {
 fn submit(&mut self,mut r:OrderRequest)->BoxFuture<'_,OrderResult> {Box::pin(async move {
    self.sent.lock().unwrap().push(r.clone());
    if let Some(cap)=self.faults.lock().unwrap().cap {r.units=r.units.min(cap);}
    let result=self.inner.submit(r).await?;
    let lost={let mut f=self.faults.lock().unwrap();let lost=f.lose_ack;f.lose_ack=false;lost};
    if lost {anyhow::bail!("synthetic acknowledgement lost after fill");}
    Ok(result)
 })}
 fn lookup(&mut self,r:OrderRequest)->BoxFuture<'_,OrderResult>{Box::pin(async move {
    if self.faults.lock().unwrap().unknown {return Ok(OrderResult{exchange_created_ms:None,terminal:false,fills:vec![],reason:"synthetic unknown".into()});}
    if self.deferred.as_ref().is_some_and(|old|old.id==r.id) {let old=self.deferred.take().unwrap();self.inner.submit(old).await?;}
    self.inner.lookup(r).await
 })}
 fn account(&mut self)->BoxFuture<'_,AccountEvidence>{Box::pin(async move {
    if self.faults.lock().unwrap().account_down {anyhow::bail!("synthetic account unavailable");}
    let mut a=self.inner.account().await?;
    let f=self.faults.lock().unwrap();
    if f.wrong_account {a.account="another-account".into();}
    if f.stale {a.observed_ms=1;}
    Ok(a)
 })}
}
struct Fixture {s:Snapshot,db:Option<store::Store>,path:std::path::PathBuf,workers:[AccountWorker;2],books:Arc<RwLock<[Book;2]>>,faults:[Arc<Mutex<Faults>>;2],sent:[Arc<Mutex<Vec<OrderRequest>>>;2],q:i64}
impl Fixture {
 async fn new(market:MarketPair,prior:Option<Action>,known:bool)->Self {Self::with_deferred(market,prior,known,false).await}
 async fn with_deferred(market:MarketPair,prior:Option<Action>,known:bool,defer:bool)->Self {
    let mut c=InventoryConfig::default();c.market=market;c.direction_policy=DirectionPolicy::Both;
    let now=crate::domain::now_ms();let mut s=Snapshot::new(c.clone()).unwrap();s.direction=Direction::LighterShort;
    let q=market.common_step()*10;
    for venue in [Venue::Lighter,Venue::Entropy] {s.record_fill(&Fill{id:format!("seed-{venue:?}"),order_id:"old-lot".into(),venue,side:s.direction.open_side(venue),units:q,price:2100.into(),fee:0.into(),time_ms:now-20},None).unwrap();}
    s.lots.push(Lot{id:"old-lot".into(),level:0,units:q,opened_ms:now-20,entry_spread:0.into(),entry_net_spread:Some(0.into())});s.opened_groups=1;
    let prior_request=prior.map(|action| {let venue=if action==Action::Open {Venue::Entropy}else{Venue::Lighter};
      OrderRequest{id:"blocked-v2-first".into(),venue,side:Side::Buy,units:market.common_step()*7,limit:2102.into(),arrival_mid:Some(2100.into()),reduce_only:action==Action::Close,created_ms:now-10,expires_ms:now+5000,signed_expires_ms:None}});
    let mut realistic=books(now);for b in &mut realistic {b.bids[0].price=2100.into();b.asks[0].price=2101.into();}
    let book=Arc::new(RwLock::new(realistic));let faults=std::array::from_fn(|_|Arc::new(Mutex::new(Faults::default())));let sent=std::array::from_fn(|_|Arc::new(Mutex::new(vec![])));
    let mut built=vec![];
    for venue in [Venue::Lighter,Venue::Entropy] {
      let mut inner=PaperBackend::new(venue,c.clone(),s.positions[venue.index()].clone(),book.clone()).with_orderbook_matching();
      if let Some(r)=prior_request.as_ref().filter(|r|r.venue==venue && !defer) {
        let result=inner.submit(r.clone()).await.unwrap();
        assert_eq!(result.fills.iter().map(|f|f.units).sum::<i64>(),r.units,"fixture original must really fill");
        if known {for f in &result.fills{s.record_fill(f,r.arrival_mid).unwrap();}}
      }
      built.push(AccountWorker::spawn(venue,Mode::Paper,true,Box::new(Backend{inner,faults:faults[venue.index()].clone(),sent:sent[venue.index()].clone(),deferred:prior_request.as_ref().filter(|r|r.venue==venue && defer).cloned()})).unwrap());
    }
    if let Some(r)=&prior_request {
      s.pending=Some(serde_json::from_value(serde_json::json!({"id":"blocked","action":prior.unwrap(),"level":1,"requested_units":r.units,"created_ms":r.created_ms,"first":r,"hedge":null,"repair":null,"first_terminal":known,"hedge_terminal":true,"repair_terminal":false,"first_filled":if known{r.units}else{0},"hedge_filled":0,"repair_filled":0,"first_value":"0","hedge_value":"0","failed":true,"first_venue":r.venue})).unwrap());
    }
    s.status=Status::NeedsAttention;s.paused=true;s.stop_requested=true;
    let path=std::env::temp_dir().join(format!("cpa-emergency-test-{}",uuid::Uuid::new_v4()));std::fs::create_dir_all(&path).unwrap();
    let (mut db,_)=store::Store::open(&path.join("state.sqlite"),&c).unwrap();
    emergency_exit::latch(&mut s,now).unwrap();db.commit(&s,now,"emergency_latched").unwrap();
    Self{s,db:Some(db),path,workers:built.try_into().ok().unwrap(),books:book,faults,sent,q}
 }
 async fn step(&mut self) {
    let now=crate::domain::now_ms();for b in self.books.write().unwrap().iter_mut(){b.received_ms=now;}
    emergency_exit::advance(&mut self.s,self.db.as_mut().unwrap(),&self.workers,&self.books).await.unwrap();
 }
 fn reload(&mut self) {drop(self.db.take());let (db,s)=store::Store::open(&self.path.join("state.sqlite"),&self.s.config).unwrap();self.db=Some(db);self.s=s;}
 fn next_attempt(&mut self) {self.s.emergency_exit.as_mut().unwrap().next_attempt_ms=[0,0];}
 fn count(&self,i:usize)->usize {self.sent[i].lock().unwrap().len()}
}
impl Drop for Fixture {fn drop(&mut self){drop(self.db.take());let _=std::fs::remove_dir_all(&self.path);}}

#[tokio::test]
async fn old_unknown_does_not_block_real_position_exit_and_is_not_resubmitted() {
 for market in [MarketPair::Openai,MarketPair::Anth] {for action in [Action::Open,Action::Close] {for known in [false,true] {
    let mut f=Fixture::new(market,Some(action),known).await;
    let first=if action==Action::Open{1}else{0};f.faults[first].lock().unwrap().unknown=true;
    f.step().await;assert_eq!([f.count(0),f.count(1)],[1,1],"both physical positions reduce before old lookup finishes");
    for w in &f.workers {assert_eq!(w.account().await.unwrap().position_units,0);}
    f.reload();f.step().await;
    if !known {assert!(emergency_exit::active(&f.s));assert!(f.s.pending.is_some());}
    f.faults[first].lock().unwrap().unknown=false;f.step().await;f.step().await;
    assert!(!emergency_exit::active(&f.s),"{:?}",f.s.emergency_exit);
    assert!(f.s.positions.iter().all(|p|p.units==0));assert_eq!(f.s.closed_groups,1);assert!(f.s.pending.is_none());
    assert_eq!([f.count(0),f.count(1)],[1,1]);
    assert!(accounting::AccountingCache::default().report(&f.s,&f.books.read().unwrap(),crate::domain::now_ms()).closed_net_profit.is_some());
 }}}
}
#[tokio::test]
async fn unknown_emergency_ack_is_lookup_only_after_restart() {
 let mut f=Fixture::new(MarketPair::Anth,None,false).await;
 f.faults[1].lock().unwrap().lose_ack=true;f.faults[1].lock().unwrap().unknown=true;
 f.step().await;f.reload();for _ in 0..3{f.step().await;}
 assert_eq!(f.count(1),1);assert!(emergency_exit::active(&f.s));
 f.faults[1].lock().unwrap().unknown=false;f.step().await;f.step().await;
 assert!(!emergency_exit::active(&f.s));assert_eq!(f.count(1),1);
}
#[tokio::test]
async fn partial_fills_retry_only_actual_remainder_even_after_restart() {
 let mut f=Fixture::new(MarketPair::Openai,None,false).await;
 let cap=f.q/2;for x in &f.faults{x.lock().unwrap().cap=Some(cap);}
 f.step().await;f.reload();f.step().await;assert_eq!(f.count(0),1,"retry cooldown");
 f.next_attempt();f.step().await;f.reload();f.step().await;
 assert!(!emergency_exit::active(&f.s));
 for sent in &f.sent {let s=sent.lock().unwrap();assert_eq!(s.iter().map(|r|r.units).collect::<Vec<_>>(),vec![f.q,cap]);assert!(s.iter().all(|r|r.reduce_only));}
}
#[tokio::test]
async fn one_venue_failure_does_not_block_other_and_untrusted_accounts_never_trade() {
 for case in 0..4 {
    let mut f=Fixture::new(MarketPair::Anth,None,false).await;
    {let mut fault=f.faults[0].lock().unwrap();match case{0=>fault.account_down=true,1=>fault.wrong_account=true,2=>fault.stale=true,_=>f.books.write().unwrap()[0].connected=false}}
    f.step().await;assert_eq!([f.count(0),f.count(1)],[0,1]);assert!(emergency_exit::active(&f.s));
    *f.faults[0].lock().unwrap()=Faults::default();f.books.write().unwrap()[0].connected=true;
    f.step().await;f.step().await;assert!(!emergency_exit::active(&f.s));
 }
}

#[tokio::test]
async fn late_opening_fill_after_first_flat_read_is_closed_before_completion() {
 let mut f=Fixture::with_deferred(MarketPair::Anth,Some(Action::Open),false,true).await;
 f.faults[1].lock().unwrap().unknown=true;f.step().await;
 assert!(emergency_exit::active(&f.s));assert_eq!(f.workers[1].account().await.unwrap().position_units,0);
 f.faults[1].lock().unwrap().unknown=false;f.step().await;
 assert!(emergency_exit::active(&f.s),"late opening fill must not be declared complete");
 assert_eq!(f.workers[1].account().await.unwrap().position_units,MarketPair::Anth.common_step()*7);
 f.next_attempt();f.step().await;f.step().await;
 assert!(!emergency_exit::active(&f.s),"{:?}",f.s.emergency_exit);
 assert_eq!([f.count(0),f.count(1)],[1,2]);assert_eq!(f.s.positions[1].units,0);
 assert!(f.sent[1].lock().unwrap().iter().all(|r|r.reduce_only));
}

#[tokio::test]
async fn emergency_consumes_depth_beyond_normal_slippage_but_never_beyond_five_percent() {
 let mut f=Fixture::new(MarketPair::Anth,None,false).await;
 *f.books.write().unwrap()=books(crate::domain::now_ms());
 let half=f.q/2;
 f.books.write().unwrap()[0].asks=vec![Level{price:101.into(),units:half},Level{price:Decimal::new(1060,1),units:half}];
 f.books.write().unwrap()[1].bids=vec![Level{price:100.into(),units:half},Level{price:94.into(),units:half}];
 f.step().await;
 assert_eq!(f.workers[0].account().await.unwrap().position_units,0,"5 percent limit admits 106.0 after venue tick rounding");
 assert_eq!(f.workers[1].account().await.unwrap().position_units,half,"94 is outside the 95 sell limit");
 assert!(emergency_exit::active(&f.s));
 f.next_attempt();f.step().await;f.step().await;assert!(!emergency_exit::active(&f.s));
 assert_eq!(f.s.config.execution_slippage_bps,Decimal::ONE,"normal strategy stays 0.01 percent");
}

#[tokio::test]
async fn earlier_partial_repair_receipts_are_retained_in_emergency_close_accounting() {
 let mut f=Fixture::new(MarketPair::Anth,Some(Action::Close),true).await;
 let now=crate::domain::now_ms();
 let r=OrderRequest{id:"blocked-v2-repair-previous".into(),venue:Venue::Entropy,side:Side::Sell,units:200,limit:2099.into(),arrival_mid:Some(2100.into()),reduce_only:true,created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
 for fill in f.workers[1].submit(r).await.unwrap().fills {f.s.record_fill(&fill,Some(2100.into())).unwrap();}
 f.s.pending.as_mut().unwrap().repair_filled=200;
 f.step().await;f.step().await;
 assert!(!emergency_exit::active(&f.s),"{:?}",f.s.emergency_exit);
 assert_eq!(f.s.closed_groups,1);
 assert!(accounting::AccountingCache::default().report(&f.s,&f.books.read().unwrap(),crate::domain::now_ms()).closed_net_profit.is_some());
}
