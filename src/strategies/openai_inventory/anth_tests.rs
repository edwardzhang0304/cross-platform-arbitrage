#[test]
fn anth_units_and_profit_do_not_inherit_openai_scale() {
    let m=MarketPair::Anth;
    assert_eq!(m.units(Decimal::new(637,5)).unwrap(),637);
    assert!(m.units(Decimal::new(6371,6)).is_err());
    assert_eq!(m.common_units(d(15),d(2100)).unwrap(),700);
    assert_eq!(m.quantity(700),Decimal::new(7,3));
    assert_eq!(MarketPair::Openai.common_units(d(15),d(2100)).unwrap(),70);
    assert_eq!(m.protected_price(Venue::Lighter,Decimal::new(210021,2),true).unwrap(),Decimal::new(21002,1));
    assert_eq!(m.protected_price(Venue::Lighter,Decimal::new(210021,2),false).unwrap(),Decimal::new(21003,1));
    let mut p=Position::default();
    let mut f=Fill{id:"one".into(),order_id:"one".into(),venue:Venue::Lighter,side:Side::Buy,units:637,price:d(2100),fee:Decimal::ZERO,time_ms:1};
    p.apply_for(&f,m).unwrap();f.side=Side::Sell;f.price=d(2110);p.apply_for(&f,m).unwrap();
    assert_eq!(p.realized,Decimal::new(637,4));assert_eq!(p.units,0);
}

#[tokio::test]
async fn anth_realistic_virtual_ioc_preserves_five_digit_fill_and_journal_identity() {
    use venue::VenueBackend;
    use std::sync::{Arc,RwLock};
    let now=crate::domain::now_ms();let c=crate::profiles::paper_config(MarketPair::Anth).unwrap();
    let b=Book{bids:vec![Level{price:d(2100),units:637}],asks:vec![Level{price:d(2100),units:637}],received_ms:now,connected:true};
    let books=Arc::new(RwLock::new([b.clone(),b]));
    let folder=std::env::temp_dir().join(format!("anth-journal-{}",uuid::Uuid::new_v4()));std::fs::create_dir_all(&folder).unwrap();
    let path=folder.join("remote.sqlite");
    let mut backend=venue::PaperBackend::durable(Venue::Lighter,c.clone(),Position::default(),books.clone(),&path).unwrap().with_orderbook_matching();
    let r=OrderRequest{id:"partial-637".into(),venue:Venue::Lighter,side:Side::Buy,units:1000,limit:d(2101),arrival_mid:Some(d(2100)),reduce_only:false,created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
    let result=backend.submit(r.clone()).await.unwrap();
    assert!(result.terminal);assert_eq!(result.fills[0].units,637);assert_eq!(result.fills[0].price,d(2100));
    assert_eq!(backend.account().await.unwrap().position_units,637);
    drop(backend);
    let mut restored=venue::PaperBackend::durable(Venue::Lighter,c.clone(),Position::default(),books.clone(),&path).unwrap().with_orderbook_matching();
    assert_eq!(restored.lookup(r.clone()).await.unwrap().fills[0].units,637);
    assert!(restored.submit(r).await.is_err());drop(restored);
    assert!(venue::PaperBackend::durable(Venue::Entropy,c.clone(),Position::default(),books.clone(),&path).is_err());
    let mut wrong=c.clone();wrong.market=MarketPair::Openai;
    assert!(venue::PaperBackend::durable(Venue::Lighter,wrong,Position::default(),books.clone(),&path).is_err());
    let (db,_)=store::Store::open(&folder.join("engine.sqlite"),&c).unwrap();drop(db);
    assert!(store::Store::open(&folder.join("engine.sqlite"),&crate::profiles::paper_config(MarketPair::Openai).unwrap()).is_err());
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn anth_unknown_entry_restart_repairs_partial_hedge_without_losing_tail() {
    use std::sync::{Arc,Mutex};
    for fraction in [1,2,3] { for reverse in [false,true] {
        let now=crate::domain::now_ms();let mut s=warmed(now);s.config.market=MarketPair::Anth;
        s.config.direction_policy=DirectionPolicy::Both;
        if reverse {for (_,x) in &mut s.samples {*x = -*x;}s.previous_reverse_signal=Some((now-15000,d(16),d(10)));}
        else {s.previous_signal=Some((now-15000,d(16),d(10)));}
        let b=bidir_books(now,16,reverse);
        s.pending=strategy::evaluate(&mut s,&b,&accounts(now),now).unwrap();
        s.pending.as_mut().unwrap().requested_units=20000;
        let path=std::env::temp_dir().join(format!("anth-entry-{}.sqlite",s.instance_id));let cfg=s.config.clone();
        let (mut db,_)=store::Store::open(&path,&cfg).unwrap();
        let remotes=[Arc::new(Mutex::new(Remote::default())),Arc::new(Mutex::new(Remote::default()))];
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,Box::new(FaultVenue{
            prices:if reverse{[d(116),d(100)]}else{[d(100),d(116)]},venue:v,remote:remotes[v.index()].clone(),unknown_once:true,fraction:if v==Venue::Lighter{fraction}else{1}
        })).unwrap());
        execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();drop(db);
        let (mut db,mut s)=store::Store::open(&path,&cfg).unwrap();
        let stale=[Book::default(),Book::default()];
        execution::advance(&mut s,&mut db,&workers,&stale,now).await.unwrap();
        assert_eq!(remotes[1].lock().unwrap().queries,1);
        for step in 0..20 {if s.pending.is_none(){break;} execution::advance(&mut s,&mut db,&workers,&b,now+step).await.unwrap();}
        assert!(s.pending.is_none(),"fraction {fraction}: {}",s.reason);
        let expected=if fraction==3{0}else{20000/fraction};let sign=if reverse{-1}else{1};
        assert_eq!(s.positions[0].units,sign*expected);assert_eq!(s.positions[1].units,-sign*expected);
        assert_eq!(s.paired_units(),expected);
        for i in 0..2 {assert_eq!(remotes[i].lock().unwrap().position.units,s.positions[i].units);}
        if fraction==3 {assert!(s.fills.values().any(|f|f.units%100!=0));}
    }}
}

#[tokio::test]
async fn anth_partial_close_finishes_five_digit_tail_before_entropy() {
    use std::sync::{Arc,Mutex};
    for reverse in [false,true] {
        let now=crate::domain::now_ms();let (mut s,mut a)=loss_fixture(now);
        s.config.market=MarketPair::Anth;if reverse{mirror_fixture(&mut s,&mut a);}
        s.close_requested=true;let b=bidir_books(now,16,reverse);
        s.pending=strategy::evaluate(&mut s,&b,&a,now).unwrap();
        let qty=s.pending.as_ref().unwrap().requested_units;
        let remotes=s.positions.clone().map(|position|Arc::new(Mutex::new(Remote{position,..Default::default()})));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,Box::new(PartialCloseVenue(FaultVenue{
            prices:if reverse{[d(116),d(100)]}else{[d(100),d(116)]},venue:v,remote:remotes[v.index()].clone(),unknown_once:false,fraction:1
        }))).unwrap());
        let path=std::env::temp_dir().join(format!("anth-close-{}.sqlite",s.instance_id));let cfg=s.config.clone();
        let (mut db,_)=store::Store::open(&path,&cfg).unwrap();
        execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
        assert_eq!(s.pending.as_ref().unwrap().first_filled,qty-5);
        assert_eq!(remotes[1].lock().unwrap().submissions,0);
        drop(db);let (mut db,mut s)=store::Store::open(&path,&cfg).unwrap();
        for step in 1..10 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&b,now+step).await.unwrap();}
        assert!(s.pending.is_none());assert_eq!(s.paired_units(),10000-qty);
        assert_eq!(s.positions[0].units,-s.positions[1].units);
        assert_eq!(remotes[0].lock().unwrap().submissions,2);assert_eq!(remotes[1].lock().unwrap().submissions,1);
        assert!(s.fills.values().any(|f|f.units==5));
    }
}

#[test]
fn anth_funding_uses_position_at_payment_time_and_correct_sign() {
    let mut orders=std::collections::BTreeMap::new();
    let fill=Fill{id:"f".into(),order_id:"o".into(),venue:Venue::Lighter,side:Side::Sell,units:637,price:d(2100),fee:Decimal::ZERO,time_ms:100};
    orders.insert("o".into(),OrderResult{exchange_created_ms: None, terminal:true,fills:vec![fill],reason:String::new()});
    assert_eq!(paper_funding::estimate(Venue::Lighter,MarketPair::Anth,&orders,100,d(1)).unwrap().amount,Decimal::ZERO);
    assert_eq!(paper_funding::estimate(Venue::Lighter,MarketPair::Anth,&orders,200,d(1)).unwrap().amount,Decimal::new(637,5));
    assert_eq!(paper_funding::estimate(Venue::Lighter,MarketPair::Anth,&orders,200,d(-1)).unwrap().amount,-Decimal::new(637,5));
    assert!(paper_funding::estimate(Venue::Entropy,MarketPair::Anth,&orders,200,d(1)).is_err());
}
