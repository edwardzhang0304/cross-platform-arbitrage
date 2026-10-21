use super::*;

#[test]
fn raw_isolated_collateral_is_distinct_from_valuation_available_on_rest_and_stream() {
    let position=json!({"position":"0.16","position_value":"240","allocated_margin":"90",
        "unrealized_pnl":"18","initial_margin_fraction":"33.33","margin_mode":1});
    let rest=json!({"positions":[position.clone()],"available_balance":"28","collateral":"0.1"});
    let stream=rest.clone();
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
