use super::*;

#[test]
fn raw_isolated_collateral_is_distinct_from_valuation_available_on_rest_and_stream() {
    let position=json!({"position":"0.16","position_value":"240","allocated_margin":"90",
        "unrealized_pnl":"18","initial_margin_fraction":"33.33","margin_mode":1});
    let rest=json!({"positions":[position.clone()],"available_balance":"28","collateral":"0.1"});
    // Independent wire fixtures: WS total includes the 90 U allocation;
    // copying the REST fixture here previously concealed the production bug.
    let frame=json!({"positions":{"42":position.clone()}});
    let stats=json!({"stats":{"available_balance":"28","collateral":"90.1","portfolio_value":"108.1"}});
    let stream=lighter_stream_account(&frame,&stats).unwrap();
    for a in [rest,stream] {
        let e=lighter_entry_margin(&a,Some(&position));
        assert_eq!(e.spendable(),Some(Decimal::new(1,1)));assert_eq!(e.mark_price,Some(1500.into()));
        assert_eq!(e.initial_margin_fraction,Some(Decimal::new(3333,4)));
        assert!(e.check(Decimal::new(1,2),1500.into(),Side::Sell,Decimal::ZERO,10.into()).is_err());
    }
    for raw in [json!("0.3333"),json!("33.33"),json!("3333")] {
        assert_eq!(lighter_margin_fraction(&raw).unwrap(),Decimal::new(3333,4));
    }
    let missing=json!({"positions":[position.clone()],"available_balance":"28"});
    assert!(lighter_entry_margin(&missing,Some(&position)).spendable().is_none());
    let cross=json!({"positions":[position.clone(),{"position":"1","margin_mode":0}],"available_balance":"28","collateral":"100"});
    assert!(lighter_entry_margin(&cross,Some(&position)).spendable().is_none());
    assert_eq!(lighter_entry_rejection("canceled-margin-not-allowed"),Some(entry_margin::EntryRejection::MarginNotAllowed));
    assert_eq!(lighter_entry_rejection("canceled-invalid-balance"),Some(entry_margin::EntryRejection::InvalidBalance));
    for status in ["canceled-too-much-slippage","filled","in-progress","unknown margin error"] {
        assert!(lighter_entry_rejection(status).is_none());
    }
}

#[test]
fn stream_cash_subtracts_every_isolated_allocation_including_flat_rows() {
    let p=json!({"position":"0.16","position_value":"240","allocated_margin":"90",
        "unrealized_pnl":"18","initial_margin_fraction":"33.33","margin_mode":1});
    let frame=json!({"positions":{"42":p.clone(),"43":{"position":"0.01","allocated_margin":"10","unrealized_pnl":"-2","margin_mode":1},
        "44":{"position":"0","allocated_margin":"5","unrealized_pnl":"0","margin_mode":1}}});
    let stats=json!({"stats":{"available_balance":"40","collateral":"125","portfolio_value":"141"}});
    let a=lighter_stream_account(&frame,&stats).unwrap();
    let e=lighter_entry_margin(&a,Some(&p));
    assert_eq!(e.spendable(),Some(20.into()));
    e.check(Decimal::new(1,2),1500.into(),Side::Sell,Decimal::ZERO,10.into()).unwrap();
    // A missing row allocation, a mixed cross position, a negative allocation,
    // or independently updated incompatible valuations must fail entries closed.
    for case in 0..4 {
        let mut f=frame.clone();let mut st=stats.clone();
        match case {0=>{f["positions"]["43"].as_object_mut().unwrap().remove("allocated_margin");},
            1=>f["positions"]["43"]["margin_mode"]=json!(0),
            2=>f["positions"]["43"]["allocated_margin"]=json!("-1"),
            _=>st["stats"]["portfolio_value"]=json!("145")};
        let a=lighter_stream_account(&f,&st).unwrap();
        assert!(lighter_entry_margin(&a,Some(&p)).spendable().is_none());
        assert_eq!(a["positions"].as_array().unwrap().len(),3); // exits retain position evidence
    }
}

#[test]
fn normalized_stream_cash_blocks_first_leg_admission_for_both_markets() {
    use std::str::FromStr;
    for market in [MarketPair::Openai,MarketPair::Anth] {
        let now=crate::domain::now_ms();let mut c=InventoryConfig::default();c.market=market;
        let mut s=Snapshot::new(c).unwrap();s.status=Status::Running;
        let p=json!({"position":"0","position_value":"0","allocated_margin":"90","unrealized_pnl":"0",
            "initial_margin_fraction":"33.33","margin_mode":1});
        let frame=json!({"positions":{"42":p.clone()}});
        let st=json!({"stats":{"available_balance":"28","collateral":"90.1","portfolio_value":"90.1"}});
        let a=lighter_stream_account(&frame,&st).unwrap();
        let cash=lighter_entry_margin(&a,Some(&p));
        let accounts=std::array::from_fn(|i|AccountEvidence{venue:if i==0 {Venue::Lighter}else{Venue::Entropy},
            account:if i==0 {s.config.lighter_account.clone()}else{s.config.entropy_account.clone()},observed_ms:now,
            position_units:0,free_margin:28.into(),equity:100.into(),leverage:3,isolated:true,open_orders:0,authenticated:true,
            entry_margin:if i==0 {Some(cash.clone())}else{None},liquidation_price:None});
        let book=Book{bids:vec![Level{price:Decimal::from_str("1500").unwrap(),units:100000}],
            asks:vec![Level{price:1501.into(),units:100000}],received_ms:now,connected:true};
        let units=s.config.market.units(Decimal::new(1,2)).unwrap();
        let err=strategy::risk_check(&s,&[book.clone(),book],&accounts,now,units,Action::Open).unwrap_err();
        assert!(err.to_string().contains("可划转保证金不足"),"{err}");
        assert!(s.pending.is_none());
    }
}

#[test]
fn missing_or_empty_entry_margin_never_blocks_reduce_only_venue_submission() {
    let now=crate::domain::now_ms();let c=InventoryConfig::default();
    let a=AccountEvidence{venue:Venue::Lighter,account:c.lighter_account.clone(),position_units:-100,
        observed_ms:now,free_margin:Decimal::ZERO,equity:10.into(),isolated:true,leverage:3,
        open_orders:0,authenticated:true,liquidation_price:None,entry_margin:Some(Default::default())};
    let r=OrderRequest{id:"synthetic-exit".into(),venue:Venue::Lighter,side:Side::Buy,units:100,
        limit:1500.into(),arrival_mid:None,reduce_only:true,created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
    final_risk(&c,&a,&r).unwrap();
    let mut opening=r.clone();opening.side=Side::Sell;opening.reduce_only=false;
    assert!(entry_funding(&c,&a,&opening).is_err());assert!(final_risk(&c,&a,&opening).is_err());
}
