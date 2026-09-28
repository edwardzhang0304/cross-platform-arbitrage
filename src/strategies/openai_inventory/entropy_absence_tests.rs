use super::*;

fn fixture(market: MarketPair, reverse: bool) -> (InventoryConfig, OrderRequest,
    venue::LookupEvidence, hyperliquid::ClearinghouseState, hyperliquid::UserFill) {
    let config = InventoryConfig { market, ..InventoryConfig::default() };
    let qty = if market == MarketPair::Anth { 700 } else { 70 };
    let first_side = if reverse { Side::Sell } else { Side::Buy };
    let r = OrderRequest { id:"synthetic-incident-v2-repair".into(), venue:Venue::Entropy,
        side:first_side.opposite(), units:qty, limit:Decimal::from(2100), arrival_mid:None,
        reduce_only:true, created_ms:1_000_000, expires_ms:1_005_000, signed_expires_ms:None };
    let remote: hyperliquid::UserFill = serde_json::from_value(json!({
        "coin":market.entropy_symbol(), "side":if reverse {"A"} else {"B"},
        "sz":config.quantity(qty).to_string(), "px":"2100", "fee":"0.001",
        "time":999324, "oid":12345, "hash":"synthetic-first-fill", "tid":42,
        "dir":"Open Long", "closedPnl":"0", "crossed":true
    })).unwrap();
    let known = Fill { id:hyperliquid::user_fill_identity(&remote),
        order_id:"synthetic-incident-v2-first".into(), venue:Venue::Entropy,
        side:first_side, units:qty, price:Decimal::from(2100), fee:Decimal::new(1,3), time_ms:remote.time };
    let evidence = venue::LookupEvidence { request_id:r.id.clone(), market,
        position_units:8 * qty * first_side.sign(), known_fills:vec![known] };
    let chain = serde_json::from_value(json!({"time":15_400_000,
        "marginSummary":{"accountValue":"100","totalNtlPos":"100","totalRawUsd":"100","totalMarginUsed":"10"},
        "assetPositions":[{"position":{"coin":market.entropy_symbol(),
            "szi":config.quantity(evidence.position_units).to_string()}}]
    })).unwrap();
    (config,r,evidence,chain,remote)
}

#[test]
fn expired_repair_ignores_only_exact_other_order_fills_in_both_markets_and_directions() {
    for market in [MarketPair::Openai,MarketPair::Anth] { for reverse in [false,true] {
        let (c,r,e,mut chain,f) = fixture(market,reverse);
        for hours in [4,24] {
            let now=r.created_ms+hours*3_600_000; chain.time=Some(now);
            let result=entropy_absence_result(&c,&r,Some(&e),&chain,&[],&[f.clone()],now).unwrap();
            assert!(result.terminal && result.fills.is_empty());
            assert!(result.reason.contains("complete known-fill history"));
        }
    }}
}

#[test]
fn expired_unknown_request_requires_fresh_complete_matching_evidence() {
    for case in 0..21 {
        let (c,mut r,mut e,mut chain,mut f)=fixture(MarketPair::Anth,false);
        let now=chain.time.unwrap();
        let mut rows=vec![];
        let mut orders=vec![];
        match case {
            0=>chain.time=None,
            1=>chain.time=Some(r.expires_ms+30_000),
            2=>chain.time=Some(now-15_001),
            3=>chain.time=Some(now+15_001),
            4=>chain.asset_positions[0].position.szi="0.049".into(),
            5=>chain.asset_positions.push(chain.asset_positions[0].clone()),
            6=>e.request_id="wrong-request".into(),
            7=>e.market=MarketPair::Openai,
            8=>e.known_fills[0].order_id=r.id.clone(),
            9=>e.known_fills[0].venue=Venue::Lighter,
            10=>e.known_fills.clear(),
            11=>f.oid+=1,
            12=>f.px="2099".into(),
            13=>f.sz="0.006".into(),
            14=>f.side="A".into(),
            15=>f.fee="0.002".into(),
            16=>e.known_fills.push(e.known_fills[0].clone()),
            17=>rows.push(f.clone()),
            18=>orders.push(serde_json::from_value(json!({"coin":"io:ANTH","limitPx":"2100",
                "oid":999,"side":"A","sz":"0.007","timestamp":now})).unwrap()),
            19=>r.venue=Venue::Lighter,
            _=>e.known_fills[0].order_id.clear(),
        }
        rows.push(f);
        assert!(entropy_absence_result(&c,&r,Some(&e),&chain,&orders,&rows,now).is_err(),"case {case}");
    }
}

#[test]
fn omitted_fills_and_full_raw_pages_cannot_be_treated_as_absence() {
    let (c,r,e,chain,f)=fixture(MarketPair::Anth,false); let now=chain.time.unwrap();
    assert!(entropy_absence_result(&c,&r,None,&chain,&[],&[f.clone()],now).is_err());
    assert!(entropy_absence_result(&c,&r,Some(&e),&chain,&[],&[],now).is_err());
    let mut foreign=f.clone(); foreign.coin="BTC".into();
    let mut rows=vec![foreign;1999]; rows.push(f);
    assert!(entropy_absence_result(&c,&r,Some(&e),&chain,&[],&rows,now).unwrap_err()
        .to_string().contains("page is full"));
}

#[test]
fn unchanged_position_does_not_hide_unrecorded_round_trip_or_partial_fill() {
    let (c,r,e,chain,f)=fixture(MarketPair::Anth,false); let now=chain.time.unwrap();
    let mut unknown=f.clone(); unknown.oid=555; unknown.hash="unknown-fill".into();
    let mut reverse=unknown.clone(); reverse.oid=556; reverse.side="A".into();
    for unknown_rows in [vec![unknown.clone()],vec![unknown,reverse]] {
        let mut rows=vec![f.clone()]; rows.extend(unknown_rows);
        assert!(entropy_absence_result(&c,&r,Some(&e),&chain,&[],&rows,now).is_err());
    }
}

#[tokio::test]
async fn read_only_preflight_deadline_is_a_proven_non_submission() {
    let err=entropy_preflight(std::future::pending::<Result<()>>(),510).await.unwrap_err();
    assert!(err.to_string().contains("request not submitted"));
    let value=entropy_preflight(async {Ok(42)},1000).await.unwrap(); assert_eq!(value,42);
    assert!(entropy_preflight(async {Ok(42)},500).await.is_err());
    assert!(entropy_preflight::<()>(async {anyhow::bail!("invalid account")},1000).await
        .unwrap_err().to_string().contains("invalid account"));
}

struct AbsenceRecoveryBackend {
    config: InventoryConfig,
    first_fill: hyperliquid::UserFill,
    chain: hyperliquid::ClearinghouseState,
    sent: std::sync::Arc<std::sync::Mutex<Vec<OrderRequest>>>,
}
impl venue::VenueBackend for AbsenceRecoveryBackend {
    fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_,OrderResult> {
        Box::pin(async move {
            assert_eq!(r.venue,Venue::Entropy);
            assert!(r.reduce_only && r.id.ends_with("-repair-1"));
            self.sent.lock().unwrap().push(r.clone());
            Ok(OrderResult {exchange_created_ms:None,terminal:true,reason:"filled".into(),
                fills:vec![Fill {id:"synthetic-successful-repair".into(),order_id:r.id,
                    venue:r.venue,side:r.side,units:r.units,price:r.limit,fee:Decimal::ZERO,time_ms:r.created_ms}]})
        })
    }
    fn lookup(&mut self, _: OrderRequest) -> BoxFuture<'_,OrderResult> {
        Box::pin(async {anyhow::bail!("regression: durable evidence was not forwarded")})
    }
    fn lookup_reconciled(&mut self, r: OrderRequest, e: venue::LookupEvidence) -> BoxFuture<'_,OrderResult> {
        Box::pin(async move {
            entropy_absence_result(&self.config,&r,Some(&e),&self.chain,&[],
                &[self.first_fill.clone()],self.chain.time.unwrap())
        })
    }
    fn account(&mut self) -> BoxFuture<'_,AccountEvidence> {
        Box::pin(async {anyhow::bail!("synthetic fixture only")})
    }
}

#[tokio::test]
async fn persisted_unknown_repair_recovers_once_and_preserves_seven_pairs_and_stop_flags() {
    use std::sync::{Arc,Mutex};
    for market in [MarketPair::Anth,MarketPair::Openai] { for reverse in [false,true] {
        let now=crate::domain::now_ms();
        let (mut c,mut repair,mut e,mut chain,mut remote)=fixture(market,reverse);
        c.direction_policy=DirectionPolicy::Both;
        let qty=repair.units;
        let shifted=now-4*3_600_000-repair.created_ms;
        repair.created_ms+=shifted; repair.expires_ms+=shifted;
        remote.time+=shifted; chain.time=Some(now);
        e.known_fills[0].time_ms=remote.time;
        e.known_fills[0].id=hyperliquid::user_fill_identity(&remote);
        let mut s=Snapshot::new(c.clone()).unwrap();
        s.direction=if reverse {Direction::LighterLong} else {Direction::LighterShort};
        s.paused=true;s.stop_requested=true;
        s.anchor=Some(Decimal::from(28));s.time_adds_used=3;
        s.last_open_completed=Some((repair.created_ms-3_600_000,Decimal::from(38)));
        for i in 0..7 {
            let id=format!("synthetic-old-pair-{i}");
            for v in [Venue::Lighter,Venue::Entropy] {
                s.record_fill(&Fill {id:format!("{id}-{v:?}"),order_id:format!("{id}-v2-first"),
                    venue:v,side:s.direction.open_side(v),units:qty,price:Decimal::from(2100),
                    fee:Decimal::ZERO,time_ms:repair.created_ms-600_000},None).unwrap();
            }
            s.lots.push(Lot {id,units:qty,level:i,opened_ms:repair.created_ms-600_000,
                entry_spread:Decimal::from(28+i as i64),entry_net_spread:Some(Decimal::from(28+i as i64))});
        }
        s.opened_groups=7;
        s.record_fill(&e.known_fills[0],None).unwrap();
        let mut first=repair.clone();first.id=e.known_fills[0].order_id.clone();
        first.side=first.side.opposite();first.reduce_only=false;first.created_ms-=2000;first.expires_ms-=2000;
        s.pending=Some(serde_json::from_value(json!({
            "id":"synthetic-incident","action":"open","level":7,"requested_units":qty,
            "created_ms":first.created_ms,"first":first,"hedge":null,"repair":repair,
            "first_terminal":true,"hedge_terminal":true,"repair_terminal":false,
            "first_filled":qty,"hedge_filled":0,"repair_filled":0,
            "first_value":(Decimal::from(qty)*Decimal::from(2100)).to_string(),
            "hedge_value":"0","failed":true,"first_venue":"entropy"
        })).unwrap());
        s.status=Status::NeedsAttention;
        s.reason="order unresolved past execution deadline; reconciliation required".into();
        let original_lots=serde_json::to_value(&s.lots).unwrap();
        let last_open=s.last_open_completed;
        let dir=std::env::temp_dir().join(format!("entropy-absence-recovery-{}",s.instance_id));
        let path=dir.join("ledger.sqlite");
        let (mut db,_)=store::Store::open(&path,&c).unwrap();
        db.commit(&s,now,"synthetic_rc7_unknown_repair").unwrap();drop(db);
        let (mut db,mut s)=store::Store::open(&path,&c).unwrap();
        let sent=Arc::new(Mutex::new(Vec::new()));
        let workers=[Venue::Lighter,Venue::Entropy].map(|v| venue::AccountWorker::spawn(v,
            Mode::Paper,true,Box::new(venue::GuardedBackend {lease:Arc::new(||true),
                inner:Box::new(AbsenceRecoveryBackend {config:c.clone(),first_fill:remote.clone(),
                    chain:chain.clone(),sent:sent.clone()})})).unwrap());
        // Loading rc.7 state only queries its original ID; it must not reissue it.
        let books=[2100,2100].map(|p|Book {bids:vec![Level{price:Decimal::from(p),units:100_000}],
            asks:vec![Level{price:Decimal::from(p),units:100_000}],received_ms:now,connected:true});
        if reverse {
            // The same proof must reach the adapter through background timeout polling.
            s.status=Status::NeedsAttention;
            s.reason="order unresolved past execution deadline; reconciliation required".into();
            execution::recheck_timed_out(&mut s,&mut db,&workers,now).await.unwrap();
        } else {
            execution::advance(&mut s,&mut db,&workers,&books,now).await.unwrap();
        }
        assert!(s.pending.as_ref().unwrap().repair_terminal);
        assert!(sent.lock().unwrap().is_empty());
        // A second crash after the absence proof must not lose that proof or the old pairs.
        drop(db);let (mut db,mut s)=store::Store::open(&path,&c).unwrap();
        execution::advance(&mut s,&mut db,&workers,&books,now).await.unwrap();
        let retry_at=s.pending.as_ref().unwrap().repair_retry_after_ms.unwrap();
        let accounts=[Venue::Lighter,Venue::Entropy].map(|v|AccountEvidence {venue:v,account:"fixture".into(),
            observed_ms:retry_at,position_units:s.positions[v.index()].units,free_margin:Decimal::from(100),
            equity:Decimal::from(100),leverage:3,isolated:true,open_orders:0,authenticated:true,liquidation_price:None});
        let books=books.map(|mut b|{b.received_ms=retry_at;b});
        assert!(execution::resume_protected_repair(&mut s,&accounts,&books,retry_at).unwrap());
        execution::advance(&mut s,&mut db,&workers,&books,retry_at).await.unwrap();
        execution::advance(&mut s,&mut db,&workers,&books,retry_at).await.unwrap();
        assert!(s.pending.is_none());
        assert_eq!(sent.lock().unwrap().len(),1);
        assert_eq!(sent.lock().unwrap()[0].units,qty);
        assert_eq!(serde_json::to_value(&s.lots).unwrap(),original_lots);
        assert_eq!(s.paired_units(),7*qty);
        assert_eq!(s.positions[0].units,-s.positions[1].units);
        assert_eq!(s.opened_groups,7);assert_eq!(s.closed_groups,0);
        assert!(s.paused && s.stop_requested);
        assert_eq!(s.time_adds_used,3);assert_eq!(s.last_open_completed,last_open);
        drop(db);std::fs::remove_dir_all(dir).unwrap();
    }}
}
