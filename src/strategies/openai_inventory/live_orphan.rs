//! Live-only fail-closed response when a venue reports that one owned leg vanished.
//! The missing leg's execution price/PnL remains unknown until exchange history is reconciled.
use super::{store::Store, venue::AccountWorker, *};
use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmergencyAttempt {
    pub request: OrderRequest,
    pub result: Option<OrderResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub missing: Venue,
    pub detected_ms: u64,
    pub last_check_ms: u64,
    pub attempts: Vec<EmergencyAttempt>,
    pub completed_ms: Option<u64>,
    pub warning: String,
}

fn fresh(a: &[AccountEvidence; 2], s: &Snapshot, now: u64) -> bool {
    a.iter().enumerate().all(|(i, x)| {
        x.venue.index() == i
            && x.account.as_str() == if i == 0 { s.config.lighter_account.as_str() } else { s.config.entropy_account.as_str() }
            && x.authenticated
            && x.open_orders == 0
            && x.isolated
            && x.leverage == s.config.leverage
            && x.observed_ms <= now
            && now - x.observed_ms <= s.config.account_max_age_ms
    })
}

/// Only an exactly missing leg and an unchanged opposite leg can trigger an automatic close.
/// Other account mismatches require manual investigation rather than a guessed order.
pub fn candidate(s: &Snapshot, a: &[AccountEvidence; 2], now: u64) -> Option<Venue> {
    if s.config.mode != Mode::Live || s.pending.is_some() || s.lots.is_empty() || !fresh(a, s, now) {
        return None;
    }
    for missing in [Venue::Lighter, Venue::Entropy] {
        let i = missing.index();
        let other = 1 - i;
        if s.positions[i].units != 0
            && a[i].position_units == 0
            && s.positions[other].units != 0
            && a[other].position_units == s.positions[other].units
        {
            return Some(missing);
        }
    }
    None
}

fn request(s: &Snapshot, a: &AccountEvidence, book: &Book, attempt: usize, now: u64) -> Result<OrderRequest> {
    let venue = a.venue;
    let owned = s.positions[venue.index()].units;
    ensure!(owned != 0 && a.position_units != 0
        && a.position_units.signum() == owned.signum()
        && a.position_units.abs() <= owned.abs(), "unexpected survivor position; no emergency order");
    book.validate(now, s.config.book_max_age_ms)?;
    let side = if a.position_units > 0 { Side::Sell } else { Side::Buy };
    let levels = if side == Side::Buy { &book.asks } else { &book.bids };
    let best = levels[0].price;
    let limit = best * (Decimal::ONE
        + Decimal::from(side.sign()) * s.config.execution_slippage_bps / Decimal::from(10_000));
    let available: i64 = levels.iter()
        .take_while(|level| if side == Side::Buy { level.price <= limit } else { level.price >= limit })
        .map(|level| level.units)
        .sum();
    let step = s.config.market.venue_step(venue);
    let units = available.min(a.position_units.abs()) / step * step;
    ensure!(units > 0, "no protected depth for emergency reduction");
    Ok(OrderRequest {
        id: format!("openai-{}-v2-orphan-{attempt}", s.instance_id),
        venue,
        side,
        units,
        limit,
        arrival_mid: book.mid(),
        reduce_only: true,
        created_ms: now,
        expires_ms: now + s.config.operation_timeout_ms,
        signed_expires_ms: (venue == Venue::Lighter).then_some(now + 599_000),
    })
}

fn save_result(attempt: &mut EmergencyAttempt, result: OrderResult) -> Result<()> {
    ensure!(result.fills.iter().all(|fill| fill.order_id == attempt.request.id
        && fill.venue == attempt.request.venue && fill.side == attempt.request.side),
        "emergency fill ownership mismatch");
    ensure!(result.fills.iter().map(|fill| fill.units).sum::<i64>() <= attempt.request.units,
        "emergency fill exceeds request");
    attempt.result = Some(result);
    Ok(())
}

fn warning(s: &mut Snapshot, store: &mut Store, now: u64, message: String) -> Result<()> {
    let incident = s.live_orphan.as_mut().unwrap();
    if incident.warning != message {
        incident.warning = message.clone();
        s.reason = format!("live orphan protection waiting: {message}");
        store.commit(s, now, "live_orphan_waiting")?;
    }
    Ok(())
}

/// Returns true while the incident owns the strategy: ordinary entry/exit logic must not run.
pub async fn protect(s: &mut Snapshot, store: &mut Store, workers: &[AccountWorker; 2],
    books: &[Book; 2], observed: &[AccountEvidence; 2], now: u64) -> Result<bool> {
    if s.config.mode != Mode::Live { return Ok(false); }
    ensure!(s.live_orphan.is_none() || s.pending.is_none(),
        "emergency incident overlaps a paired operation; manual reconciliation required");
    if s.pending.is_some() { return Ok(false); }
    if s.live_orphan.is_none() {
        let Some(missing) = candidate(s, observed, now) else { return Ok(false); };
        let (l, e) = tokio::join!(workers[0].reconcile_account(), workers[1].reconcile_account());
        let confirmed = [l?, e?];
        if candidate(s, &confirmed, crate::domain::now_ms()) != Some(missing) { return Ok(false); }
        s.live_orphan = Some(Incident { missing, detected_ms: now, last_check_ms: 0,
            attempts: vec![], completed_ms: None, warning: String::new() });
        s.paused = true;
        s.stop_requested = true;
        s.resume_after_recovery = false;
        s.status = Status::NeedsAttention;
        s.reason = format!("live {:?} leg missing; reducing opposite venue", missing);
        store.commit(s, now, "live_orphan_latched")?;
    }
    let incident = s.live_orphan.as_ref().unwrap();
    if incident.completed_ms.is_some() || now.saturating_sub(incident.last_check_ms) < 1_000 {
        return Ok(true);
    }
    s.live_orphan.as_mut().unwrap().last_check_ms = now;
    let (l, e) = tokio::join!(workers[0].reconcile_account(), workers[1].reconcile_account());
    let accounts = match (l,e) {
        (Ok(l),Ok(e)) => [l,e],
        (l,e) => {
            warning(s, store, now, format!("account recheck failed: {} {}",
                l.err().map_or_else(String::new, |x| x.to_string()),
                e.err().map_or_else(String::new, |x| x.to_string())))?;
            return Ok(true);
        }
    };
    if !fresh(&accounts, s, crate::domain::now_ms()) {
        warning(s, store, now, "account evidence stale or untrusted".into())?;
        return Ok(true);
    }
    let missing = s.live_orphan.as_ref().unwrap().missing;
    let other = 1 - missing.index();
    ensure!(accounts[missing.index()].position_units == 0,
        "missing leg reappeared; emergency halted for manual reconciliation");
    if let Some(last) = s.live_orphan.as_mut().unwrap().attempts.last_mut() {
        if !last.result.as_ref().is_some_and(|r| r.terminal) {
            let result = match workers[last.request.venue.index()].lookup(last.request.clone()).await {
                Ok(result) => result,
                Err(error) => {
                    warning(s, store, now, format!("original emergency order lookup failed: {error}"))?;
                    return Ok(true);
                }
            };
            save_result(last, result)?;
            store.commit(s, now, "live_orphan_lookup")?;
            if !s.live_orphan.as_ref().unwrap().attempts.last().unwrap().result.as_ref().unwrap().terminal {
                return Ok(true);
            }
        }
    }
    if accounts[other].position_units == 0 {
        let incident = s.live_orphan.as_mut().unwrap();
        incident.completed_ms = Some(now);
        s.status = Status::NeedsAttention;
        s.reason = "both venues flat after emergency reduction; liquidation PnL requires manual reconciliation".into();
        store.commit(s, now, "live_orphan_flat_confirmed")?;
        return Ok(true);
    }
    if s.live_orphan.as_ref().unwrap().attempts.len() >= 3 {
        warning(s, store, now, "three protected reductions exhausted; manual close required".into())?;
        return Ok(true);
    }
    let survivor = accounts[other].venue;
    let attempt_no = s.live_orphan.as_ref().unwrap().attempts.len() + 1;
    let order = match request(s, &accounts[other], &books[other], attempt_no, crate::domain::now_ms()) {
        Ok(order) => order,
        Err(error) => {
            warning(s, store, now, format!("protected reduction unavailable: {error}"))?;
            return Ok(true);
        }
    };
    s.live_orphan.as_mut().unwrap().attempts.push(EmergencyAttempt { request: order.clone(), result: None });
    store.commit(s, now, "live_orphan_order_reserved")?;
    match workers[survivor.index()].submit(order).await {
        Ok(result) => {
            save_result(s.live_orphan.as_mut().unwrap().attempts.last_mut().unwrap(), result)?;
            store.commit(s, now, "live_orphan_order_result")?;
        }
        Err(error) => {
            warning(s, store, now, format!("submission uncertain; checking original order: {error}"))?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::venue::{BoxFuture, VenueBackend};
    use std::sync::{Arc, Mutex};

    struct MockLive {
        evidence: Arc<Mutex<AccountEvidence>>,
        submitted: Arc<Mutex<Vec<OrderRequest>>>,
        uncertain_once: bool,
    }
    impl VenueBackend for MockLive {
        fn account(&mut self) -> BoxFuture<'_, AccountEvidence> {
            let evidence = self.evidence.clone();
            Box::pin(async move {
                let mut a = evidence.lock().unwrap().clone();
                a.observed_ms = crate::domain::now_ms();
                Ok(a)
            })
        }
        fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
            let evidence = self.evidence.clone();
            let submitted = self.submitted.clone();
            let uncertain = std::mem::take(&mut self.uncertain_once);
            Box::pin(async move {
                ensure!(r.reduce_only, "emergency test must reduce only");
                let mut a = evidence.lock().unwrap();
                ensure!(a.position_units.signum() != r.side.sign() && a.position_units.abs() >= r.units,
                    "emergency test would increase exposure");
                a.position_units += r.side.sign() * r.units;
                submitted.lock().unwrap().push(r);
                if uncertain { anyhow::bail!("simulated lost acknowledgement after fill"); }
                Ok(OrderResult { terminal: true, fills: vec![], reason: "mock terminal".into() })
            })
        }
        fn lookup(&mut self, _r: OrderRequest) -> BoxFuture<'_, OrderResult> {
            Box::pin(async { Ok(OrderResult { terminal: true, fills: vec![], reason: "mock terminal".into() }) })
        }
    }
    #[test]
    fn only_fresh_confirmed_missing_leg_is_a_live_emergency() {
        let mut c = InventoryConfig::default();
        c.mode = Mode::Live;
        c.entropy_address = "0x1111111111111111111111111111111111111111".into();
        c.lighter_account_index = Some(1);
        let mut s = Snapshot::new(c).unwrap();
        s.lots.push(Lot { id: "one".into(), units: 100, level: 0,
            opened_ms: 0, entry_spread: Decimal::ONE,
            entry_net_spread: Some(Decimal::ONE) });
        s.positions[0].units = 100;
        s.positions[1].units = -100;
        let now = crate::domain::now_ms();
        let mut a = [Venue::Lighter, Venue::Entropy].map(|venue| AccountEvidence {
            venue, account: if venue == Venue::Lighter { s.config.lighter_account.clone() } else { s.config.entropy_account.clone() }, observed_ms: now,
            position_units: s.positions[venue.index()].units, free_margin: Decimal::from(100),
            equity: Decimal::from(100), leverage: 3, isolated: true,
            open_orders: 0, authenticated: true, liquidation_price: None,
        });
        assert_eq!(candidate(&s, &a, now), None);
        a[1].position_units = 0;
        assert_eq!(candidate(&s, &a, now), Some(Venue::Entropy));
        a[1].observed_ms = now - 3_001;
        assert_eq!(candidate(&s, &a, now), None);
        a[1].observed_ms = now;
        a[0].position_units = 90;
        assert_eq!(candidate(&s, &a, now), None);
    }

    #[test]
    fn emergency_order_is_reduce_only_and_capped_by_protected_depth() {
        let mut c = InventoryConfig::default();
        c.mode = Mode::Live;
        c.entropy_address = "0x1111111111111111111111111111111111111111".into();
        c.lighter_account_index = Some(1);
        let now = crate::domain::now_ms();
        for (venue, owned, side) in [(Venue::Lighter, 1000, Side::Sell),
            (Venue::Entropy, -1000, Side::Buy)] {
            let mut s = Snapshot::new(c.clone()).unwrap();
            s.positions[venue.index()].units = owned;
            let a = AccountEvidence { venue,
                account: if venue == Venue::Lighter { c.lighter_account.clone() } else { c.entropy_account.clone() },
                observed_ms: now, position_units: owned, free_margin: Decimal::from(100),
                equity: Decimal::from(100), leverage: 3, isolated: true,
                open_orders: 0, authenticated: true, liquidation_price: None };
            let book = Book { bids: vec![Level { price: Decimal::from(1600), units: 400 }],
                asks: vec![Level { price: Decimal::from(1601), units: 400 }],
                received_ms: now, connected: true };
            let r = request(&s, &a, &book, 1, now).unwrap();
            assert_eq!(r.venue, venue);
            assert_eq!(r.side, side);
            assert_eq!(r.units, 400);
            assert!(r.reduce_only);
            assert!(if side == Side::Buy { r.limit >= book.asks[0].price }
                else { r.limit <= book.bids[0].price });
        }
    }

    #[cfg(feature="openai-inventory-live")]
    #[tokio::test]
    async fn live_missing_leg_closes_survivor_and_restart_remains_locked() {
      for uncertain in [false, true] {
        let mut c = InventoryConfig::default();
        c.mode = Mode::Live;
        c.entropy_address = "0x1111111111111111111111111111111111111111".into();
        c.lighter_account_index = Some(1);
        let mut s = Snapshot::new(c.clone()).unwrap();
        s.status = Status::Running;
        s.positions[0].units = 100;
        s.positions[1].units = -100;
        s.lots.push(Lot { id: "one".into(), units: 100, level: 0, opened_ms: 0,
            entry_spread: Decimal::ONE, entry_net_spread: Some(Decimal::ONE) });
        let path = std::env::temp_dir().join(format!("live-orphan-{}.sqlite", s.instance_id));
        let (mut store, _) = Store::open(&path, &c).unwrap();
        store.commit(&s, crate::domain::now_ms(), "test_owned_inventory").unwrap();
        let now = crate::domain::now_ms();
        let evidence = [Venue::Lighter, Venue::Entropy].map(|venue| Arc::new(Mutex::new(AccountEvidence {
            venue, account: if venue == Venue::Lighter { c.lighter_account.clone() } else { c.entropy_account.clone() },
            observed_ms: now, position_units: if venue == Venue::Lighter { 100 } else { 0 },
            free_margin: Decimal::from(100), equity: Decimal::from(100), leverage: 3,
            isolated: true, open_orders: 0, authenticated: true, liquidation_price: None,
        })));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let workers = [Venue::Lighter, Venue::Entropy].map(|venue| AccountWorker::spawn(
            venue, Mode::Live, false, Box::new(MockLive {
                evidence: evidence[venue.index()].clone(), submitted: submitted.clone(),
                uncertain_once: uncertain && venue == Venue::Lighter,
            })
        ).unwrap());
        let book = Book { bids: vec![Level { price: Decimal::from(1600), units: 1000 }],
            asks: vec![Level { price: Decimal::from(1600), units: 1000 }],
            received_ms: now, connected: true };
        let books = [book.clone(), book];
        let observed = [evidence[0].lock().unwrap().clone(), evidence[1].lock().unwrap().clone()];
        assert!(protect(&mut s, &mut store, &workers, &books, &observed, now).await.unwrap());
        let orders = submitted.lock().unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].venue, Venue::Lighter);
        assert_eq!(orders[0].side, Side::Sell);
        assert_eq!(orders[0].units, 100);
        assert!(orders[0].reduce_only);
        drop(orders);
        assert_eq!(evidence[0].lock().unwrap().position_units, 0);
        drop(store);
        let (mut restored_store, restored) = Store::open(&path, &c).unwrap();
        s = restored;
        assert!(protect(&mut s, &mut restored_store, &workers, &books, &observed, now + 1_000).await.unwrap());
        assert!(s.live_orphan.as_ref().unwrap().completed_ms.is_some());
        assert_eq!(s.status, Status::NeedsAttention);
        assert_eq!(submitted.lock().unwrap().len(), 1, "restart must not resubmit");
        drop(restored_store);
        let (_, restored) = Store::open(&path, &c).unwrap();
        assert_eq!(restored.status, Status::NeedsAttention);
        assert!(restored.live_orphan.is_some());
        assert!(restored.paused && restored.stop_requested);
      }
    }
}
