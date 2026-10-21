use super::*;
fn d(s:&str)->Decimal{s.parse().unwrap()}
fn evidence()->EntryMarginEvidence {
    EntryMarginEvidence{reported_available:Some(d("28")),collateral:Some(d("0.1")),
        allocated_margin:Some(d("92")),unrealized_pnl:Some(d("18")),position_value:Some(d("246")),
        initial_margin_fraction:Some(d("0.3333")),mark_price:Some(d("1500")),isolated_only:true}
}
#[test]
fn valuation_available_and_isolated_profit_cannot_pay_for_a_new_entry() {
    let mut e=evidence();
    assert_eq!(e.spendable(),Some(d("0.1")));
    for side in [Side::Buy,Side::Sell] {
        assert!(e.check(d("0.01"),d("1500"),side,Decimal::ZERO,d("10")).is_err());
    }
    e.collateral=Some(d("20"));
    assert!(e.check(d("0.01"),d("1500"),Side::Buy,Decimal::ZERO,d("10")).is_ok());
    e.reported_available=Some(d("12"));
    assert!(e.check(d("0.01"),d("1500"),Side::Buy,Decimal::ZERO,d("10")).is_err());
}
#[test]
fn adverse_execution_fee_and_reserve_are_included_in_both_directions() {
    for (side,limit) in [(Side::Buy,d("1501")),(Side::Sell,d("1499"))] {
        let mut e=evidence();let fee=d("0.001");let reserve=d("10");
        let need=d("0.01")*(d("1500")*d("0.3333")+d("1")+limit*fee)+reserve;
        e.collateral=Some(need);
        e.check(d("0.01"),limit,side,fee,reserve).unwrap();
        e.collateral=Some(need-d("0.000001"));
        assert!(e.check(d("0.01"),limit,side,fee,reserve).is_err());
    }
}
#[test]
fn missing_margin_mixed_cross_risk_and_underfunded_isolated_position_fail_closed() {
    for case in 0..8 {
        let mut e=evidence();e.collateral=Some(d("100"));e.reported_available=Some(d("100"));
        match case {0=>e.collateral=None,1=>e.initial_margin_fraction=None,2=>e.position_value=None,
            3=>e.allocated_margin=None,4=>e.unrealized_pnl=None,5=>e.mark_price=None,
            6=>e.isolated_only=false,_=>e.unrealized_pnl=Some(d("-20"))}
        assert!(e.check(d("0.01"),d("1500"),Side::Buy,Decimal::ZERO,d("10")).is_err(),"{case}");
    }
    let mut flat=evidence();flat.collateral=Some(d("100"));flat.position_value=Some(Decimal::ZERO);
    flat.allocated_margin=Some(Decimal::ZERO);flat.unrealized_pnl=Some(Decimal::ZERO);flat.mark_price=None;
    flat.check(d("0.01"),d("1500"),Side::Buy,Decimal::ZERO,d("10")).unwrap();
}
#[test]
fn entry_protection_is_profile_local_and_old_snapshots_remain_compatible() {
    let mut a=Snapshot::new(InventoryConfig::default()).unwrap();let b=a.clone();
    let req=OrderRequest{id:"synthetic-margin-order".into(),venue:Venue::Lighter,side:Side::Sell,
        units:100,limit:d("1500"),arrival_mid:None,reduce_only:false,created_ms:1000,
        expires_ms:6000,signed_expires_ms:None};
    latch(&mut a,&req,EntryRejection::MarginNotAllowed);
    let restored:Snapshot=serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
    assert!(restored.entry_margin_block.is_some() && restored.paused);
    assert!(service::start_continuous(&mut a).is_err());
    assert!(b.entry_margin_block.is_none());
    let mut legacy=serde_json::to_value(&a).unwrap();legacy.as_object_mut().unwrap().remove("entry_margin_block");
    assert!(serde_json::from_value::<Snapshot>(legacy).unwrap().entry_margin_block.is_none());
}
