fn bidir_books(now: u64, spread: i64, reverse: bool) -> [Book; 2] {
    let mut b = books(now, spread);
    if reverse {
        for book in &mut b {
            let bids = book.asks.iter().map(|l| Level { price: d(216)-l.price, units:l.units }).collect();
            let asks = book.bids.iter().map(|l| Level { price: d(216)-l.price, units:l.units }).collect();
            book.bids=bids; book.asks=asks;
        }
    }
    b
}
fn mirror_fixture(s: &mut Snapshot, a: &mut [AccountEvidence; 2]) {
    s.config.direction_policy=DirectionPolicy::Both;
    s.direction=Direction::LighterShort;
    for (_,m) in &mut s.samples { *m = -*m; }
    for (p,a) in s.positions.iter_mut().zip(a) {
        p.units = -p.units; p.average=d(216)-p.average; a.position_units=p.units;
    }
}

#[test]
fn reverse_signal_needs_two_samples_current_depth_and_directional_mean() {
    let now=4_000_000;
    let mut s=warmed(now);
    for (_,m) in &mut s.samples { *m = -*m; }
    s.config.direction_policy=DirectionPolicy::Both;
    let b=bidir_books(now,16,true);
    assert!(strategy::evaluate(&mut s,&b,&accounts(now),now).unwrap().is_none());
    let next=now+15000;
    let mut thin=bidir_books(next,16,true);
    thin[1].asks[0].units=10;
    thin[1].asks.push(Level{price:d(120),units:100000});
    assert!(strategy::evaluate(&mut s,&thin,&accounts(next),next).unwrap().is_none());
    let op=strategy::evaluate(&mut s,&bidir_books(next+15000,16,true),&accounts(next+15000),next+15000).unwrap().unwrap();
    assert_eq!(s.direction,Direction::LighterShort);
    assert_eq!(op.first_venue,Venue::Entropy);
    assert!(op.min_entry_spread.unwrap()>=d(14));
    let mut legacy=warmed(now);
    legacy.previous_reverse_signal=Some((now-15000,d(16),d(10)));
    assert!(strategy::evaluate(&mut legacy,&b,&accounts(now),now).unwrap().is_none());
}

#[test]
fn reverse_continuity_never_lowers_its_own_threshold() {
    let now=4_000_000;
    let mut s=warmed(now);s.samples.clear();s.mean_initialized=true;
    s.continuity_mean=Some((now-1000,d(-10)));
    s.samples.push_back((now-1000,d(-5)));
    assert_eq!(strategy::reference_mean_for(&s,now,Direction::LighterShort),Some(d(10)));
    s.samples[0].1=d(-15);
    assert_eq!(strategy::reference_mean_for(&s,now,Direction::LighterShort),Some(d(15)));
}

#[test]
fn negative_mean_cannot_allow_negative_protected_entry_spread() {
    let now=4_000_000;
    for reverse in [false,true] {
        let mut s=warmed(now);
        s.config.direction_policy=DirectionPolicy::Both;
        for (_,m) in &mut s.samples { *m=if reverse{d(10)}else{d(-10)}; }
        if reverse {s.previous_reverse_signal=Some((now-15000,d(1),d(-10)));}
        else {s.previous_signal=Some((now-15000,d(1),d(-10)));}
        // Both venue quotes are equal. The two protected limits would create a
        // negative spread even though zero passes the top-of-book threshold.
        assert!(strategy::evaluate(&mut s,&bidir_books(now,0,reverse),&accounts(now),now).unwrap().is_none());
        let next=now+15000;
        let op=strategy::evaluate(&mut s,&bidir_books(next,1,reverse),&accounts(next),next).unwrap().unwrap();
        assert_eq!(op.min_entry_spread,Some(Decimal::ZERO));
    }
}

#[test]
fn direction_change_is_flat_fresh_bound_paper_only_and_preserves_history() {
    let now=4_000_000;
    let mut s=warmed(now);s.paused=true;
    let id=s.instance_id.clone();let samples=s.samples.clone();
    for case in 0..8 {
        let mut bad=s.clone();let mut a=accounts(now);
        match case {
            0=>bad.paused=false,
            1=>bad.positions[0].units=10,
            2=>a[0].observed_ms=now-4000,
            3=>a[1].open_orders=1,
            4=>a[1].authenticated=false,
            5=>a[1].account="different".into(),
            6=>a[0].venue=Venue::Entropy,
            _=>bad.config.mode=Mode::Live,
        }
        let before=serde_json::to_string(&bad).unwrap();
        assert!(service::set_direction_policy(&mut bad,DirectionPolicy::Both,Some(&a),now).is_err());
        assert_eq!(serde_json::to_string(&bad).unwrap(),before);
    }
    service::set_direction_policy(&mut s,DirectionPolicy::Both,Some(&accounts(now)),now).unwrap();
    assert_eq!(s.samples,samples);assert_eq!(s.instance_id,id);assert!(s.paused);
    assert_eq!(s.config.direction_policy,DirectionPolicy::Both);
    let path=std::env::temp_dir().join(format!("direction-policy-{}.sqlite",s.instance_id));
    let (mut db,_)=store::Store::open(&path,&s.config).unwrap();
    db.commit(&s,now,"test-policy").unwrap();drop(db);
    let (_,restored)=store::Store::open(&path,&s.config).unwrap();
    assert_eq!(restored.config.direction_policy,DirectionPolicy::Both);
    assert_eq!(restored.samples,samples);
    let mut old=serde_json::to_value(&s).unwrap();
    for field in ["direction","previous_reverse_signal","reverse_first_armed","fill_opening"] {old.as_object_mut().unwrap().remove(field);}
    old["config"].as_object_mut().unwrap().remove("direction_policy");
    let old:Snapshot=serde_json::from_value(old).unwrap();
    assert_eq!(old.direction,Direction::LighterLong);assert_eq!(old.config.direction_policy,DirectionPolicy::LighterLongOnly);
}

#[tokio::test]
async fn both_directions_complete_open_close_cross_zero_and_record_fill_purpose() {
    use std::sync::{Arc,RwLock};
    for reverse in [false,true] {
        let now=crate::domain::now_ms();let mut s=warmed(now);
        s.config.direction_policy=DirectionPolicy::Both;
        s.config.execution_slippage_bps=Decimal::ZERO;
        if reverse {for (_,m) in &mut s.samples{*m = -*m;}s.previous_reverse_signal=Some((now-15000,d(16),d(10)));}
        else {s.previous_signal=Some((now-15000,d(16),d(10)));}
        let b=bidir_books(now,16,reverse);
        s.pending=strategy::evaluate(&mut s,&b,&accounts(now),now).unwrap();
        let qty=s.pending.as_ref().unwrap().requested_units;
        let shared=Arc::new(RwLock::new(b.clone()));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,Box::new(venue::PaperBackend::new(v,s.config.clone(),Position::default(),shared.clone()))).unwrap());
        let path=std::env::temp_dir().join(format!("bidir-cycle-{}.sqlite",s.instance_id));
        let (mut db,_)=store::Store::open(&path,&s.config).unwrap();
        for _ in 0..8 {if s.pending.is_none(){break;} execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
        assert!(s.pending.is_none());assert_eq!(s.lots.len(),1);
        let sign=if reverse{-1}else{1};assert_eq!(s.positions[0].units,sign*qty);assert_eq!(s.positions[1].units,-sign*qty);
        assert!(s.fill_opening.values().all(|v|*v));
        let mut a=accounts(now);for i in 0..2{a[i].position_units=s.positions[i].units;}
        s.assert_reconciled(&a).unwrap();
        // A flip in the price ordering cannot change the direction of existing inventory.
        let locked=s.direction;
        let close_now=crate::domain::now_ms();let close=bidir_books(close_now,-2,reverse);
        *shared.write().unwrap()=close.clone();
        s.last_sample_ms=0;s.previous_exit=Some((close_now-15000,true));
        s.pending=strategy::evaluate(&mut s,&close,&a,close_now).unwrap();
        assert_eq!(s.direction,locked);assert_eq!(s.pending.as_ref().unwrap().action,Action::Close);
        for _ in 0..8 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&close,close_now).await.unwrap();}
        assert!(s.pending.is_none());assert!(s.lots.is_empty());assert_eq!(s.positions[0].units,0);assert_eq!(s.positions[1].units,0);
        let fees=s.cumulative_fees();
        assert_eq!(s.total_pnl(&close).unwrap(),quantity(qty)*d(18)-fees);
        assert_eq!(s.fill_opening.values().filter(|x|**x).count(),2);
        assert_eq!(s.fill_opening.values().filter(|x|!**x).count(),2);
        assert_eq!(s.opened_groups,1);assert_eq!(s.closed_groups,1);
        let serialized=serde_json::to_string(&s).unwrap();let restored:Snapshot=serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.fill_opening,s.fill_opening);assert_eq!(restored.direction,locked);
    }
}

#[test]
fn held_reverse_inventory_does_not_add_an_opposite_grid() {
    let now=4_000_000;let(mut s,mut a)=loss_fixture(now);mirror_fixture(&mut s,&mut a);
    s.previous_signal=Some((now-15000,d(20),d(-10)));
    s.previous_reverse_signal=Some((now-15000,d(-20),d(10)));
    s.previous_exit=None;
    assert!(strategy::evaluate(&mut s,&books(now,20),&a,now).unwrap().is_none());
    assert_eq!(s.direction,Direction::LighterShort);assert_eq!(s.lots.len(),1);
    // Only after the previous round is flat may a new opposite direction be reserved.
    s.lots.clear();s.positions=Default::default();s.last_sample_ms=0;s.anchor=None;
    let proposal=strategy::evaluate(&mut s,&books(now,20),&accounts(now),now).unwrap().unwrap();
    assert_eq!(proposal.action,Action::Open);assert_eq!(s.direction,Direction::LighterLong);
}

#[tokio::test]
async fn reverse_failed_hedge_and_unfillable_repair_keep_residual_for_attention() {
    use std::sync::{Arc,Mutex};
    let now=crate::domain::now_ms();let mut s=warmed(now);
    s.config.direction_policy=DirectionPolicy::Both;
    for (_,m) in &mut s.samples{*m = -*m;}
    s.previous_reverse_signal=Some((now-15000,d(16),d(10)));
    let b=bidir_books(now,16,true);
    s.pending=strategy::evaluate(&mut s,&b,&accounts(now),now).unwrap();
    let qty=s.pending.as_ref().unwrap().requested_units;
    let remotes=[Arc::new(Mutex::new(Remote::default())),Arc::new(Mutex::new(Remote::default()))];
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,
        Box::new(FaultVenue{prices:[d(116),d(100)],venue:v,remote:remotes[v.index()].clone(),unknown_once:false,fraction:1})).unwrap());
    let path=std::env::temp_dir().join(format!("bidir-failed-repair-{}.sqlite",s.instance_id));
    let(mut db,_)=store::Store::open(&path,&s.config).unwrap();
    execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
    assert_eq!(s.positions[1].units,qty);
    let mut bad=b.clone();bad[0].bids[0].price=d(113);bad[1].bids[0].units=1;
    for _ in 0..5 {execution::advance(&mut s,&mut db,&workers,&bad,now).await.unwrap();if s.status==Status::NeedsAttention{break;}}
    assert_eq!(s.status,Status::NeedsAttention);assert!(s.pending.is_some());
    assert_eq!(s.positions[0].units,0);assert_eq!(s.positions[1].units,qty);
    assert_eq!(remotes[0].lock().unwrap().submissions,0);
    assert_eq!(remotes[1].lock().unwrap().submissions,1);
    drop(db);let(_,restored)=store::Store::open(&path,&s.config).unwrap();
    assert_eq!(restored.direction,Direction::LighterShort);
    assert_eq!(restored.positions[1].units,qty);assert!(restored.pending.is_some());
}
