use super::*;
use rust_decimal::Decimal;
use std::{str::FromStr, sync::{Arc,RwLock,atomic::AtomicU64}};

fn d(s:&str)->Decimal { Decimal::from_str(s).unwrap() }
const DAY:u64=86_400_000;
const NOW:u64=20*DAY;

pub(super) fn seed(market:MarketPair,scale:u32,reverse:bool,age:u64)->Snapshot {
    let mut c=crate::profiles::paper_config(market).unwrap();c.set_initial_capital(scale).unwrap();
    c.fee_lighter=Decimal::ZERO;c.fee_entropy=Decimal::ZERO;c.execution_slippage_bps=Decimal::ZERO;
    let mut s=Snapshot::new(c).unwrap();
    s.direction=if reverse {Direction::LighterLong}else{Direction::LighterShort};
    s.status=Status::Running;s.paused=true;s.stop_requested=false;s.funding_synced_ms=NOW;
    let n=s.config.units(d("0.1")*Decimal::from(scale)).unwrap();
    let opened=NOW-age;
    for v in [Venue::Lighter,Venue::Entropy] {
        let side=s.direction.open_side(v);
        s.record_fill(&Fill{id:format!("open-{v:?}"),order_id:"lot-a".into(),venue:v,side,units:n,
            price:if side==Side::Sell {d("150")}else{d("100")},fee:Decimal::ZERO,time_ms:opened-2+v.index() as u64},None).unwrap();
    }
    s.lots.push(Lot{id:"lot-a".into(),level:0,units:n,opened_ms:opened,entry_spread:d("50"),entry_net_spread:Some(d("50"))});
    s.opened_groups=1;s.anchor=Some(d("50"));s
}
pub(super) fn books(s:&Snapshot,spread:&str,now:u64)->[Book;2] {
    std::array::from_fn(|i| {
        let px=d("100")+if i==s.direction.short(){d(spread)}else{Decimal::ZERO};
        Book{bids:vec![Level{price:px,units:1_000_000}],asks:vec![Level{price:px,units:1_000_000}],received_ms:now,connected:true}
    })
}
pub(super) fn accounts(s:&Snapshot,now:u64)->[AccountEvidence;2] {
    [Venue::Lighter,Venue::Entropy].map(|v|AccountEvidence{venue:v,
        account:if v==Venue::Lighter{s.config.lighter_account.clone()}else{s.config.entropy_account.clone()},
        observed_ms:now,position_units:s.positions[v.index()].units,free_margin:d("5000"),equity:d("5000"),
        leverage:3,isolated:true,open_orders:0,authenticated:true,liquidation_price:None,entry_margin:None})
}
fn funding(s:&mut Snapshot,id:&str,v:Venue,time:u64,amount:&str) {
    accounting::record_funding(s,Funding{id:id.into(),venue:v,time_ms:time,amount:d(amount)}).unwrap();
}
fn eligible(s:&Snapshot,spread:&str,now:u64)->bool {
    strategy::group_exit_eligible(s,&s.lots[0],&books(s,spread,now),s.lots[0].units,now).unwrap()
}

#[test]
fn linear_target_boundaries_floor_and_clock_rollback() {
    let mut s=seed(MarketPair::Openai,1,false,0);let l=&mut s.lots[0];
    for (age,want) in [(0,"25"),(DAY*7/2,"15"),(DAY*7,"5"),(DAY*30,"5")] {
        assert_eq!(strategy::group_profit_target(&s.config,l,NOW+age),d(want));
    }
    assert_eq!(strategy::group_profit_target(&s.config,l,NOW-1),d("25"));
    assert!(strategy::group_profit_target(&s.config,l,NOW+7*DAY-1)>d("5"));
    for entry in ["7","10"] {l.entry_spread=d(entry);
        for age in [0,DAY,7*DAY,30*DAY] {assert_eq!(strategy::group_profit_target(&s.config,l,NOW+age),d("5"));}}
}

#[test]
fn independent_lot_age_survives_restart_and_partial_close() {
    let mut s=seed(MarketPair::Anth,10,true,7*DAY);
    let mut newer=s.lots[0].clone();newer.id="newer".into();newer.opened_ms=NOW;s.lots.push(newer);
    s.last_open_completed=Some((NOW,d("50")));
    s.lots[0].units/=2;
    let restored:Snapshot=serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(strategy::group_profit_target(&restored.config,&restored.lots[0],NOW),d("5"));
    assert_eq!(strategy::group_profit_target(&restored.config,&restored.lots[1],NOW),d("25"));
    assert_eq!(restored.lots[0].opened_ms,NOW-7*DAY);
}

#[test]
fn age_and_full_costs_gate_both_markets_directions_and_capital_tiers() {
    for market in [MarketPair::Openai,MarketPair::Anth] {for scale in [1,10] {for reverse in [false,true] {
        let young=seed(market,scale,reverse,0);assert!(!eligible(&young,"40",NOW));
        let mut s=seed(market,scale,reverse,7*DAY);
        assert!(eligible(&s,"45",NOW));assert!(!eligible(&s,"45.1",NOW));
        s.config.execution_slippage_bps=d("1");assert!(!eligible(&s,"45",NOW));
        assert!(eligible(&s,"44",NOW));
        s.config.execution_slippage_bps=Decimal::ZERO;
        // Opening fees/repair costs are already in entry_net_spread.
        s.lots[0].entry_net_spread=Some(d("46"));
        s.config.fee_lighter=d("0.01");s.config.fee_entropy=d("0.01");
        assert!(!eligible(&s,"45",NOW));
        s.config.fee_lighter=Decimal::ZERO;s.config.fee_entropy=Decimal::ZERO;
        s.lots[0].entry_net_spread=Some(d("50"));
        let expense=-(d("0.5")*Decimal::from(scale));
        funding(&mut s,"cost",Venue::Entropy,NOW-DAY,&expense.to_string());
        assert!(!eligible(&s,"45",NOW),"exact zero after funding is not a profit");
        let income=d("0.1")*Decimal::from(scale);
        funding(&mut s,"income",Venue::Lighter,NOW-DAY,&income.to_string());
        assert!(eligible(&s,"45",NOW));
        let f=accounting::ProfitFunding::new(&s);
        assert_eq!(f.slice(&s.lots[0],s.lots[0].units/2).unwrap(),(expense+income)/Decimal::TWO);
    }}}
}

#[test]
fn missing_or_ambiguous_funding_blocks_only_normal_profit_decision() {
    let mut s=seed(MarketPair::Openai,1,false,7*DAY);
    funding(&mut s,"cost",Venue::Entropy,NOW-DAY,"-0.01");
    s.funding_records.clear();
    assert!(strategy::group_exit_eligible(&s,&s.lots[0],&books(&s,"44",NOW),s.lots[0].units,NOW).is_err());
    // Explicit risk close does not consult lot age or attributed funding.
    s.close_requested=true;
    let b=books(&s,"60",NOW);let a=accounts(&s,NOW);
    assert_eq!(strategy::evaluate(&mut s,&b,&a,NOW).unwrap().unwrap().action,Action::Close);
    let mut s=seed(MarketPair::Openai,1,false,7*DAY);
    let t=s.fills.values().find(|f|f.venue==Venue::Entropy).unwrap().time_ms;
    funding(&mut s,"ambiguous",Venue::Entropy,t,"-0.01");
    assert!(accounting::ProfitFunding::new(&s).slice(&s.lots[0],s.lots[0].units).is_err());
    s.config.max_loss_usdc=d("1");let b=books(&s,"80",NOW);let a=accounts(&s,NOW);
    assert_eq!(strategy::evaluate(&mut s,&b,&a,NOW).unwrap().unwrap().action,Action::Close);
    assert!(s.loss_stop.is_some());
}

#[tokio::test]
async fn aged_profit_exit_rechecks_funding_then_executes_and_preserves_other_history() {
    for market in [MarketPair::Openai,MarketPair::Anth] {for reverse in [false,true] {
        let base=crate::domain::now_ms();let mut s=seed(market,1,reverse,8*DAY);
        let delta=base-NOW;s.lots[0].opened_ms+=delta;for f in s.fills.values_mut(){f.time_ms+=delta;}
        s.funding_synced_ms=base;let b=books(&s,"44",base);let a=accounts(&s,base);
        funding(&mut s,"paid",Venue::Entropy,base-DAY,"-0.01");
        assert!(strategy::evaluate(&mut s,&b,&a,base).unwrap().is_none());
        let now=base+1000;let b=books(&s,"44",now);let a=accounts(&s,now);
        s.pending=strategy::evaluate(&mut s,&b,&a,now).unwrap();
        assert!(s.pending.is_some());assert!(strategy::normal_exit_eligible(&s,&b,now).unwrap());
        let reserved=serde_json::to_string(&s).unwrap();
        let mut late:Snapshot=serde_json::from_str(&reserved).unwrap();
        funding(&mut late,"late-cost",Venue::Entropy,base-DAY/2,"-1");
        assert!(!strategy::normal_exit_eligible(&late,&b,now).unwrap());
        let mut stale=s.clone();stale.funding_synced_ms=now-90_001;
        assert!(strategy::normal_exit_eligible(&stale,&b,now).is_err());
        let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(now));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,
            Box::new(venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone()).with_logical_clock(clock.clone()))).unwrap());
        let (mut late_db,_)=store::Store::offline_replay(&late.config).unwrap();
        execution::advance(&mut late,&mut late_db,&workers,&b,now).await.unwrap();
        assert!(late.pending.is_none());
        for (i,w) in workers.iter().enumerate() {
            assert_eq!(w.account().await.unwrap().position_units,s.positions[i].units,"late costs must prevent both submissions");
        }
        let (mut db,_)=store::Store::offline_replay(&s.config).unwrap();
        execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();
        assert!(s.pending.as_ref().is_some_and(|p|p.first.is_some()));
        // Once a real first request exists, later funding cannot interrupt the
        // opposite reducing leg. The final ledger still includes that expense.
        funding(&mut s,"post-submit-cost",Venue::Entropy,base-DAY/2,"-1");
        for _ in 0..10 {if s.pending.is_none(){break;}execution::advance(&mut s,&mut db,&workers,&b,now).await.unwrap();}
        assert!(s.pending.is_none(), "{market:?}/{reverse}: {} {:?}",s.reason,s.pending);assert!(s.positions.iter().all(|p|p.units==0));
        assert!(s.lots.is_empty());assert_eq!(s.closed_groups,1);
        let report=accounting::AccountingCache::default().report(&s,&b,now);
        assert!(report.closed_net_profit.unwrap()<Decimal::ZERO,"late settlement is counted without stranding the hedge");
    }}
}

#[tokio::test]
async fn emergency_exit_ignores_age_negative_profit_and_missing_funding_history() {
    let now=crate::domain::now_ms();let mut s=seed(MarketPair::Openai,1,false,0);
    s.lots[0].opened_ms=now;s.positions[1].funding=d("-0.1");
    s.funding_ids.insert("Entropy:missing".into());s.status=Status::NeedsAttention;
    let b=Arc::new(RwLock::new(books(&s,"60",now)));
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn(v,Mode::Paper,true,
        Box::new(venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),b.clone()))).unwrap());
    let(mut db,_)=store::Store::offline_replay(&s.config).unwrap();
    emergency_exit::latch(&mut s,now).unwrap();
    for _ in 0..4 {emergency_exit::advance(&mut s,&mut db,&workers,&b).await.unwrap();if !emergency_exit::active(&s){break;}}
    assert!(s.positions.iter().all(|p|p.units==0));
    assert!(s.emergency_exit.as_ref().unwrap().flat_confirmed_ms.is_some());
}
