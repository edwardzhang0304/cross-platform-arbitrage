use super::*;
use std::sync::atomic::{AtomicUsize,Ordering};

struct ReadOnlyBackend {
    account:AccountEvidence,
    funding_calls:Arc<AtomicUsize>,
    account_calls:Arc<AtomicUsize>,
    funding_fails:bool,
    account_fails:bool,
    withdraw_during_funding:bool,
}
impl VenueBackend for ReadOnlyBackend {
    fn submit(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult> {panic!("activation must never submit")}
    fn lookup(&mut self,_:OrderRequest)->venue::BoxFuture<'_,OrderResult> {panic!("activation must never lookup orders")}
    fn account(&mut self)->venue::BoxFuture<'_,AccountEvidence> {
        self.account_calls.fetch_add(1,Ordering::SeqCst);
        let mut a=self.account.clone();a.observed_ms=crate::domain::now_ms();let fail=self.account_fails;
        Box::pin(async move {ensure!(!fail,"synthetic account failure");Ok(a)})
    }
    fn funding(&mut self,_:u64,_:u64)->venue::BoxFuture<'_,Vec<Funding>> {
        self.funding_calls.fetch_add(1,Ordering::SeqCst);
        if self.withdraw_during_funding {self.account.equity=Decimal::ZERO;self.account.free_margin=Decimal::ZERO;}
        let fail=self.funding_fails;
        Box::pin(async move {ensure!(!fail,"synthetic funding failure");Ok(vec![])})
    }
}

#[tokio::test]
async fn activation_refresh_distinguishes_empty_account_read_failure_and_funding_failure() {
    for case in ["withdrawn","deposited","funding_failed","account_failed","withdraw_during_funding"] {
        let mut config=InventoryConfig::default();config.market=MarketPair::Anth;
        let (mut db,mut s)=Store::offline_replay(&config).unwrap();
        s.status=Status::Stopped;s.stop_requested=true;s.paused=true;
        s.funding_synced_ms=crate::domain::now_ms()-120_000;
        let before=serde_json::to_value(&s).unwrap();
        let funding_calls=Arc::new(AtomicUsize::new(0));let account_calls=Arc::new(AtomicUsize::new(0));
        let old=[Venue::Lighter,Venue::Entropy].map(|venue| AccountEvidence {
            venue,account:if venue==Venue::Lighter {config.lighter_account.clone()}else{config.entropy_account.clone()},
            equity:Decimal::from(200),free_margin:Decimal::from(200),observed_ms:s.funding_synced_ms,
            position_units:0,authenticated:true,leverage:3,isolated:true,open_orders:0,
            entry_margin:None,liquidation_price:None,
        });
        let workers=old.clone().map(|mut a| {
            let entropy=a.venue==Venue::Entropy;
            if entropy && case=="withdrawn" {a.equity=Decimal::ZERO;a.free_margin=Decimal::ZERO;}
            AccountWorker::spawn(a.venue,Mode::Paper,true,Box::new(ReadOnlyBackend {
                account:a,funding_calls:funding_calls.clone(),account_calls:account_calls.clone(),
                funding_fails:entropy && case=="funding_failed",account_fails:entropy && case=="account_failed",
                withdraw_during_funding:entropy && case=="withdraw_during_funding",
            })).unwrap()
        });
        let mut accounts=Some(old);
        let result=refresh_activation_evidence(&mut s,&mut db,&workers,&mut accounts).await;
        match case {
            "deposited"=>{result.unwrap();assert!(s.funding_synced_ms>before["funding_synced_ms"].as_u64().unwrap());
                assert_eq!(funding_calls.load(Ordering::SeqCst),2);assert_eq!(account_calls.load(Ordering::SeqCst),4);},
            "withdrawn"|"withdraw_during_funding"=>{
                assert!(result.unwrap_err().to_string().contains("Entropy 账户资金不足"));
                assert_eq!(accounts.as_ref().unwrap()[1].equity,Decimal::ZERO);
                if case=="withdrawn" {assert_eq!(funding_calls.load(Ordering::SeqCst),0);assert_eq!(serde_json::to_value(&s).unwrap(),before);}
            },
            _=>{assert!(result.is_err());assert_eq!(serde_json::to_value(&s).unwrap(),before);}
        }
        assert_eq!(s.status,Status::Stopped);assert!(s.paused && s.stop_requested);
        assert!(s.pending.is_none());assert!(s.positions.iter().all(|p|p.units==0));
    }
}

#[test]
fn unfunded_held_pairs_can_still_resume_exits_but_stale_account_evidence_cannot() {
    let now=crate::domain::now_ms();let mut s=Snapshot::new(InventoryConfig::default()).unwrap();
    let mut a=[Venue::Lighter,Venue::Entropy].map(|venue| AccountEvidence {
        venue,account:if venue==Venue::Lighter {s.config.lighter_account.clone()}else{s.config.entropy_account.clone()},
        observed_ms:now,position_units:0,equity:Decimal::ZERO,free_margin:Decimal::ZERO,
        authenticated:true,leverage:3,isolated:true,open_orders:0,entry_margin:None,liquidation_price:None,
    });
    assert!(validate_activation_accounts(&s,&a,now).unwrap_err().to_string().contains("账户资金不足"));
    s.positions[0].units=-700;s.positions[1].units=700;
    validate_activation_accounts(&s,&a,now).unwrap();
    a[0].observed_ms=now-3001;assert!(validate_activation_accounts(&s,&a,now).is_err());
}
