use super::*;

fn account(c:&InventoryConfig, at:u64)->AccountEvidence {
    AccountEvidence { venue:Venue::Entropy,account:c.entropy_account.clone(),observed_ms:at,
        position_units:700,free_margin:Decimal::from(100),equity:Decimal::from(100),
        leverage:c.leverage,isolated:true,open_orders:0,authenticated:true,liquidation_price:None }
}

#[test]
fn preflight_reuses_only_bound_fresh_rest_evidence_once_without_restamping() {
    for market in [MarketPair::Openai,MarketPair::Anth] {
        let mut c=InventoryConfig::default();c.market=market;
        let mut cache=Some(account(&c,1000));
        let a=take_entropy_preflight(&mut cache,&c,1001).unwrap();
        assert_eq!(a.observed_ms,1000);
        assert!(take_entropy_preflight(&mut cache,&c,1002).is_none());
        for kind in 0..5 {
            let mut a=account(&c,1000);
            match kind {0=>a.observed_ms=1002,1=>a.observed_ms=0,
                2=>a.venue=Venue::Lighter,3=>a.account="wrong profile".into(),_=>a.authenticated=false}
            let now=if kind==1 {c.account_max_age_ms+1} else {1001};
            let mut cache=Some(a);
            assert!(take_entropy_preflight(&mut cache,&c,now).is_none());
            assert!(cache.is_none());
        }
    }
}

#[test]
fn reused_evidence_still_enforces_reduce_only_actual_position_and_open_orders() {
    let c=InventoryConfig::default();let now=crate::domain::now_ms();
    let mut r=OrderRequest {id:"synthetic-repair".into(),venue:Venue::Entropy,
        side:Side::Sell,units:700,limit:Decimal::from(2000),arrival_mid:None,reduce_only:true,
        created_ms:now,expires_ms:now+5000,signed_expires_ms:None};
    let a=take_entropy_preflight(&mut Some(account(&c,now)),&c,now).unwrap();
    final_risk(&c,&a,&r).unwrap();
    r.units=701;assert!(final_risk(&c,&a,&r).is_err());
    r.units=700;r.side=Side::Buy;assert!(final_risk(&c,&a,&r).is_err());
    r.side=Side::Sell;let mut a=a;a.open_orders=1;assert!(final_risk(&c,&a,&r).is_err());
}
