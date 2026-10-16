// Included in the shared strategy test module; all books/accounts are synthetic.
#[test]
fn capital_tiers_are_explicit_bounded_and_do_not_upgrade_existing_ledgers() {
    let old=crate::profiles::paper_config(MarketPair::Openai).unwrap();
    let bytes=serde_json::to_string(&old).unwrap();assert!(!bytes.contains("capital_multiplier"));
    let restored:InventoryConfig=serde_json::from_str(&bytes).unwrap();assert_eq!(restored.capital_multiplier,1);
    let mut large=old.clone();large.set_initial_capital(10).unwrap();large.validate().unwrap();
    assert_eq!((large.group_notional,large.max_notional_per_venue,large.max_loss_usdc),(d(150),d(3000),d(300)));
    assert_eq!((large.min_free_margin,large.close_slice_notional,large.leverage,large.max_groups),(d(100),d(150),3,20));
    assert_eq!(large.grid,old.grid);assert_eq!(large.accumulation,old.accumulation);
    assert_eq!(large.group_take_profit,old.group_take_profit);assert_eq!(large.execution_slippage_bps,old.execution_slippage_bps);
    assert_eq!(rules_upgrade::current_config(&old).unwrap(),old);
    assert_eq!(rules_upgrade::current_config(&large).unwrap(),large);
    let mut state=Snapshot::new(old.clone()).unwrap();
    assert!(rules_upgrade::migrate_snapshot(&mut state,&large).is_err());assert_eq!(state.config,old);
    for kind in ["implicit","multiplier","size","cap","loss","reserve","leverage","groups"] {
        let mut bad=large.clone();match kind {
            "implicit"=>bad.capital_multiplier=1,"multiplier"=>bad.capital_multiplier=100,
            "size"=>bad.group_notional=d(151),"cap"=>bad.max_notional_per_venue=d(3001),
            "loss"=>bad.max_loss_usdc=d(301),"reserve"=>bad.min_free_margin=d(99),
            "leverage"=>bad.leverage=4,_=>bad.max_groups=21,
        }
        assert!(bad.validate().is_err(),"{kind}");
    }
}

#[test]
fn large_account_checks_its_own_margin_cap_and_300u_loss_latch() {
    let now=4_000_000;let mut s=warmed(now);s.config.set_initial_capital(10).unwrap();
    let mut b=books(now,16);for book in &mut b {book.bids[0].units=1_000_000;book.asks[0].units=1_000_000;}
    let mut a=accounts(now);for x in &mut a {x.free_margin=d(1000);x.equity=d(1100);}
    let qty=s.config.common_units(d(150),d(108)).unwrap();assert_eq!(qty,13880);
    strategy::risk_check(&s,&b,&a,now,qty,Action::Open).unwrap();
    let mut low=a.clone();low[1].free_margin=d(149);
    assert!(strategy::risk_check(&s,&b,&low,now,qty,Action::Open).unwrap_err().to_string().contains("free margin"));
    let mut full=s.clone();full.positions[0].units=300000;let mut cap=a.clone();cap[0].position_units=300000;
    assert!(strategy::risk_check(&full,&b,&cap,now,qty,Action::Open).unwrap_err().to_string().contains("notional cap"));
    s.positions[0].realized=d(-299);assert!(!strategy::enforce_loss_limit(&mut s,&b,now).unwrap());
    s.positions[0].realized=d(-300);assert!(strategy::enforce_loss_limit(&mut s,&b,now).unwrap());assert!(s.loss_stop.is_some());
    s.positions[0].realized=d(0);strategy::enforce_loss_limit(&mut s,&b,now).unwrap();assert!(s.loss_stop.is_some());
}

#[tokio::test]
async fn large_account_opens_and_closes_exact_base_pairs_in_both_directions() {
    use std::sync::{Arc,RwLock,atomic::{AtomicU64,Ordering}};
    for reverse in [false,true] {
        let now=4_000_000;let mut s=warmed(now);s.config.set_initial_capital(10).unwrap();
        s.config.direction_policy=DirectionPolicy::Both;s.config.execution_slippage_bps=Decimal::ZERO;
        if reverse {for (_,m) in &mut s.samples{*m = -*m;}s.previous_reverse_signal=Some((now-15000,d(16),d(10)));}
        else {s.previous_signal=Some((now-15000,d(16),d(10)));}
        let evidence=|s:&Snapshot,t| {let mut a=accounts(t);for i in 0..2 {a[i].position_units=s.positions[i].units;a[i].free_margin=d(1000);a[i].equity=d(1100);}a};
        let b=bidir_books(now,16,reverse);let a=evidence(&s,now);
        s.pending=strategy::evaluate(&mut s,&b,&a,now).unwrap();let qty=s.pending.as_ref().unwrap().requested_units;
        assert_eq!(qty,s.config.common_units(d(150),(b[0].mid().unwrap()+b[1].mid().unwrap())/Decimal::TWO).unwrap());
        let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(now));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn_replay(v,
            venue::PaperBackend::new(v,s.config.clone(),Position::default(),shared.clone()),clock.clone()).unwrap());
        let (mut db,_)=store::Store::offline_replay(&s.config).unwrap();
        for _ in 0..8 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
        assert!(s.pending.is_none());assert_eq!(s.lots.len(),1);assert_eq!(s.positions[0].units.abs(),qty);assert_eq!(s.positions[0].units,-s.positions[1].units);
        let mut close=b.clone();
        for round in 1..=3 {
            if s.lots.is_empty(){break;}let at=now+round*15000;clock.store(at,Ordering::SeqCst);
            close=bidir_books(at,-2,reverse);*shared.write().unwrap()=close.clone();
            let a=evidence(&s,at);s.last_sample_ms=0;s.previous_exit=Some((at-15000,true));
            s.pending=strategy::evaluate(&mut s,&close,&a,at).unwrap();assert_eq!(s.pending.as_ref().unwrap().action,Action::Close);
            for _ in 0..8 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&close,at).await.unwrap();}
            assert!(s.pending.is_none());assert_eq!(s.positions[0].units,-s.positions[1].units);
        }
        assert!(s.lots.is_empty());assert!(s.positions.iter().all(|p|p.units==0));assert_eq!(s.closed_groups,1);
        assert_eq!(s.total_pnl(&close).unwrap(),quantity(qty)*d(18)-s.cumulative_fees());
        let restored:Snapshot=serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();assert_eq!(restored.config,s.config);
    }
}
