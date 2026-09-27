use super::*;
use super::liquidation::{IsolationSpec,Mark,PaperIsolation};
use super::venue::{AccountWorker,PaperBackend,VenueBackend,BoxFuture};
use rust_decimal::Decimal;
use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};

fn spec()->IsolationSpec {IsolationSpec{maintenance_rates:[Decimal::new(12,2),Decimal::ONE/Decimal::from(12)],
    liquidation_fee_rates:[Decimal::new(1,2),Decimal::new(9,5)],mark_max_age_ms:5000}}
fn book(price:i64,now:u64)->Book {Book{bids:vec![Level{price:price.into(),units:100000}],asks:vec![Level{price:price.into(),units:100000}],received_ms:now,connected:true}}
fn marks(books:&[Book;2])->[Mark;2] {books.clone().map(|b|Mark{price:b.mid(),received_ms:b.received_ms,connected:b.connected})}

struct UncertainOnce {inner:PaperBackend, uncertain:bool}
impl VenueBackend for UncertainOnce {
    fn submit(&mut self,r:OrderRequest)->BoxFuture<'_,OrderResult> {Box::pin(async move {
        let result=self.inner.submit(r).await?;
        if self.uncertain {self.uncertain=false;anyhow::bail!("simulated lost acknowledgement after durable fill");}
        Ok(result)
    })}
    fn lookup(&mut self,r:OrderRequest)->BoxFuture<'_,OrderResult>{self.inner.lookup(r)}
    fn account(&mut self)->BoxFuture<'_,AccountEvidence>{self.inner.account()}
    fn liquidations(&mut self)->BoxFuture<'_,Vec<Fill>>{self.inner.liquidations()}
}
struct Fixture {
    state:Snapshot,store:store::Store,workers:[AccountWorker;2],books:Arc<RwLock<[Book;2]>>,
    marks:Arc<RwLock<[Mark;2]>>,clock:Arc<AtomicU64>,now:u64,path:std::path::PathBuf,
}
impl Fixture {
    async fn new(reverse:bool,uncertain:Option<Venue>)->Self {
        Self::with_policy(reverse,uncertain,ExitPolicy::PerGroup).await
    }
    async fn with_policy(reverse:bool,uncertain:Option<Venue>,policy:ExitPolicy)->Self {
        let now=crate::domain::now_ms();
        let mut config=InventoryConfig::default();config.exit_policy=policy;
        config.direction_policy=DirectionPolicy::Both;config.shared_exit_conditions=true;config.max_loss_usdc=30.into();
        let mut state=Snapshot::new(config.clone()).unwrap();state.direction=if reverse{Direction::LighterShort}else{Direction::LighterLong};state.status=Status::Running;
        let path=std::env::temp_dir().join(format!("paper-liquidation-{}",state.instance_id));std::fs::create_dir_all(&path).unwrap();
        let (store,_)=store::Store::open(&path.join("state.sqlite"),&config).unwrap();
        let books=Arc::new(RwLock::new([book(100,now),book(100,now)]));
        let marks=Arc::new(RwLock::new(marks(&books.read().unwrap())));let clock=Arc::new(AtomicU64::new(now));
        let mut built=vec![];
        for venue in [Venue::Lighter,Venue::Entropy] {
            let mut backend=PaperBackend::durable(venue,config.clone(),Position::default(),books.clone(),&path.join(format!("{venue:?}.sqlite"))).unwrap()
                .with_logical_clock(clock.clone()).with_isolation(spec(),marks.clone()).unwrap();
            let req=OrderRequest{id:format!("entry-{venue:?}"),venue,side:state.direction.open_side(venue),units:1000,limit:if state.direction.open_side(venue)==Side::Buy{101.into()}else{99.into()},arrival_mid:Some(100.into()),reduce_only:false,created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
            for f in backend.submit(req).await.unwrap().fills {state.record_fill(&f,Some(100.into())).unwrap();}
            built.push(AccountWorker::spawn(venue,Mode::Paper,true,Box::new(UncertainOnce{inner:backend,uncertain:uncertain==Some(venue)})).unwrap());
        }
        state.lots.push(Lot{id:"group-0".into(),level:0,units:1000,opened_ms:now,entry_spread:0.into(),entry_net_spread:Some(0.into())});state.opened_groups=1;
        let workers=built.try_into().ok().unwrap();
        Self{state,store,workers,books,marks,clock,now,path}
    }
    fn frame(&mut self,affected:Venue,other_stale:bool,thin:bool) {
        self.now+=250;self.clock.store(self.now,Ordering::SeqCst);
        let mut books=[book(100,self.now),book(100,self.now)];
        let price=if self.state.direction.open_side(affected)==Side::Buy{50}else{160};
        books[affected.index()]=book(price,self.now);
        *self.marks.write().unwrap()=marks(&books);
        let other=1-affected.index();
        books[other].connected=!other_stale;
        if thin {books[other].bids[0].units=400;books[other].asks[0].units=400;}
        *self.books.write().unwrap()=books;
    }
    async fn step(&mut self)->bool {
        let books=self.books.read().unwrap().clone();
        liquidation::protect(&mut self.state,&mut self.store,&self.workers,&books,self.now).await.unwrap()
    }
}

#[tokio::test]
async fn liquidation_closes_other_venue_both_directions_and_stays_stopped() {
    for reverse in [false,true] {for venue in [Venue::Lighter,Venue::Entropy] {
        let mut f=Fixture::new(reverse,None).await;
        assert!(!f.step().await);
        f.frame(venue,false,false);assert!(f.step().await);
        assert!(f.state.positions.iter().all(|p|p.units==0));
        assert_eq!(f.state.status,Status::Stopped);assert_eq!(f.state.closed_groups,1);assert!(f.state.lots.is_empty());
        assert_eq!(f.state.liquidation_protection.as_ref().unwrap().affected,vec![venue]);
        let count=f.state.fills.len();assert_eq!(count,4);
        for _ in 0..3 {assert!(f.step().await);}
        assert_eq!(f.state.fills.len(),count);assert_eq!(f.state.closed_groups,1);
        assert!(f.state.paused&&f.state.stop_requested);
    }}
}

#[tokio::test]
async fn liquidation_waits_for_other_quote_then_closes_partial_depth_without_profit_gate() {
    let mut f=Fixture::new(false,None).await;f.frame(Venue::Entropy,true,false);assert!(f.step().await);
    assert_eq!(f.state.positions[1].units,0);assert_eq!(f.state.positions[0].units,1000);
    assert!(f.state.liquidation_protection.as_ref().unwrap().completed_ms.is_none());
    assert!(f.state.liquidation_protection.as_ref().unwrap().warning.contains("stale"));
    f.frame(Venue::Entropy,false,true);assert!(f.step().await);assert_eq!(f.state.positions[0].units,600);
    f.frame(Venue::Entropy,false,true);assert!(f.step().await);assert_eq!(f.state.positions[0].units,200);
    f.frame(Venue::Entropy,false,true);assert!(f.step().await);assert_eq!(f.state.positions[0].units,0);
    assert_eq!(f.state.closed_groups,1);assert_eq!(f.state.status,Status::Stopped);
    assert!(f.state.positions.iter().map(|p|p.realized-p.fees).sum::<Decimal>()<Decimal::ZERO);
}

#[tokio::test]
async fn lost_acknowledgement_is_looked_up_without_resubmission() {
    let mut f=Fixture::new(false,Some(Venue::Lighter)).await;f.frame(Venue::Entropy,false,false);f.step().await;
    assert_eq!(f.state.positions[0].units,1000);assert_eq!(f.state.liquidation_protection.as_ref().unwrap().attempt,1);
    let request=f.state.liquidation_protection.as_ref().unwrap().orders[0].as_ref().unwrap().request.id.clone();
    f.frame(Venue::Entropy,false,false);f.step().await;
    assert_eq!(f.state.positions[0].units,0);assert_eq!(f.state.liquidation_protection.as_ref().unwrap().attempt,1);
    assert_eq!(f.state.fills.values().filter(|fill|fill.order_id==request).count(),1);
}

#[tokio::test]
async fn restart_resumes_durable_liquidation_and_retains_costs() {
    let mut f=Fixture::new(false,None).await;f.frame(Venue::Entropy,true,false);f.step().await;
    let config=f.state.config.clone();let path=f.path.clone();let old_id=f.state.instance_id.clone();
    let books=f.books.clone();let mark=f.marks.clone();let clock=f.clock.clone();let now=f.now;
    let fee=f.state.cumulative_fees();drop(f);tokio::task::yield_now().await;
    let (mut store,mut s)=store::Store::open(&path.join("state.sqlite"),&config).unwrap();
    assert_eq!(s.instance_id,old_id);assert_eq!(s.cumulative_fees(),fee);
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|AccountWorker::spawn(v,Mode::Paper,true,
        Box::new(PaperBackend::durable(v,config.clone(),s.positions[v.index()].clone(),books.clone(),&path.join(format!("{v:?}.sqlite"))).unwrap()
            .with_logical_clock(clock.clone()).with_isolation(spec(),mark.clone()).unwrap())).unwrap());
    books.write().unwrap()[0].connected=true;
    let b=books.read().unwrap().clone();
    assert!(liquidation::protect(&mut s,&mut store,&workers,&b,now).await.unwrap());
    assert!(s.positions.iter().all(|p|p.units==0));assert_eq!(s.fills.len(),4);assert_eq!(s.closed_groups,1);
}

#[tokio::test]
async fn stale_marks_never_trigger_fake_liquidation() {
    let mut f=Fixture::new(false,None).await;f.frame(Venue::Entropy,false,false);
    f.marks.write().unwrap()[1].received_ms=0;
    assert!(!f.step().await);assert_eq!(f.state.positions[1].units,-1000);
    assert!(f.state.liquidation_protection.is_none());
}

#[test]
fn isolated_collateral_fees_and_gap_loss_do_not_consume_free_cash() {
    let mut iso=PaperIsolation::default();let mut p=Position::default();
    let entry=Fill{id:"x".into(),order_id:"x".into(),venue:Venue::Entropy,side:Side::Buy,units:1000,price:100.into(),fee:Decimal::new(1,2),time_ms:1};
    iso.apply(&p,&entry,3).unwrap();p.apply(&entry).unwrap();
    assert_eq!(iso.collateral,Decimal::from(10)/Decimal::from(3)-Decimal::new(1,2));
    let free_before=Decimal::from(100)-p.fees-iso.collateral;
    let fill=iso.forced_fill(&p,Venue::Entropy,1.into(),Decimal::new(1,2),2).unwrap();p.apply(&fill).unwrap();
    assert!((Decimal::from(100)+p.realized-p.fees-free_before).abs()<Decimal::new(1,20));
    assert!(iso.breached(&Position{units:1000,average:100.into(),..Default::default()},1.into(),Decimal::new(1,1)));
    let mut cfg=InventoryConfig::default();assert!(spec().validate(&cfg).is_ok());
    cfg.exit_policy=ExitPolicy::PerGroup;assert!(spec().validate(&cfg).is_ok());cfg.mode=Mode::Live;assert!(spec().validate(&cfg).is_err());
}

#[tokio::test]
async fn variant_wires_protection_only_to_b_and_leaves_a_positions_unchanged() {
    let now=crate::domain::now_ms();let mut config=InventoryConfig::default();config.direction_policy=DirectionPolicy::Both;
    config.shared_exit_conditions=true;config.max_loss_usdc=30.into();
    let mut seed=Snapshot::new(config.clone()).unwrap();
    seed.samples=(1..=240).rev().map(|i|(now-i*15000,Decimal::from(10))).collect();
    let dir=std::env::temp_dir().join(format!("liquidation-ab-{}",seed.instance_id));
    let shared=Arc::new(RwLock::new([book(100,now),book(116,now)]));
    let mark=Arc::new(RwLock::new(marks(&shared.read().unwrap())));let clock=Arc::new(AtomicU64::new(now));
    let mut a=comparison::Variant::open(&dir.join("a.sqlite"),config.clone(),&seed,now,shared.clone(),clock.clone()).unwrap();
    config.exit_policy=ExitPolicy::PerGroup;
    let mut b=comparison::Variant::open_protected(&dir.join("b.sqlite"),config,&seed,now,shared.clone(),clock.clone(),Some((spec(),mark.clone()))).unwrap();
    for offset in [15000,30000,30250,30500,30750,31000,31250,31500] {
        let t=now+offset;let frame=[book(100,t),book(116,t)];
        *shared.write().unwrap()=frame.clone();*mark.write().unwrap()=marks(&frame);clock.store(t,Ordering::SeqCst);
        a.tick(&frame,t).await.unwrap();b.tick(&frame,t).await.unwrap();
    }
    assert_eq!(a.state.lots.len(),1);assert_eq!(b.state.lots.len(),1);
    assert_eq!(a.state.positions[0].units,b.state.positions[0].units);
    assert_eq!(a.state.cumulative_fees(),b.state.cumulative_fees());
    let a_units=a.state.positions[0].units;let t=now+31750;let frame=[book(100,t),book(160,t)];
    *shared.write().unwrap()=frame.clone();*mark.write().unwrap()=marks(&frame);clock.store(t,Ordering::SeqCst);
    a.tick(&frame,t).await.unwrap();b.tick(&frame,t).await.unwrap();
    assert_eq!(a.state.positions[0].units,a_units);assert!(a.state.liquidation_protection.is_none());
    assert!(b.state.positions.iter().all(|p|p.units==0));assert_eq!(b.state.status,Status::Stopped);
    assert!(b.summary(&frame,t)["liquidation_protection_enabled"].as_bool().unwrap());
    assert!(!a.summary(&frame,t)["liquidation_protection_enabled"].as_bool().unwrap());
}

#[tokio::test]
async fn round_paper_policy_uses_same_liquidation_and_paired_emergency_close() {
    for reverse in [false,true] {for venue in [Venue::Lighter,Venue::Entropy] {
        let mut f=Fixture::with_policy(reverse,None,ExitPolicy::Round).await;
        assert!(!f.step().await);
        f.frame(venue,false,false);assert!(f.step().await);
        assert!(f.state.positions.iter().all(|p|p.units==0));
        assert_eq!(f.state.status,Status::Stopped);
        assert_eq!(f.state.closed_groups,1);
        assert!(f.state.lots.is_empty());
        assert!(f.state.paused && f.state.stop_requested);
    }}
}
