// Multiple logical groups share one protected reduce-only operation per venue.
fn confirmed_batch(now: u64, reverse: bool) -> (Snapshot, Operation) {
    let (mut s, a) = group_fixture(now, reverse);
    s.config.decision_ms = Some(1000);
    for lot in &s.lots { s.previous_group_exit.insert(lot.id.clone(), now - 1000); }
    let op = strategy::evaluate(&mut s, &bidir_books(now, 5, reverse), &a, now).unwrap().unwrap();
    assert_eq!(op.close_allocations.len(), 2);
    assert_eq!(op.requested_units, 2000);
    (s, op)
}

#[test]
fn batch_exit_confirms_each_group_and_reserves_all_together() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, mut a) = group_fixture(now, reverse);
        s.config.decision_ms = Some(1000);
        assert!(strategy::evaluate(&mut s, &bidir_books(now, 5, reverse), &a, now).unwrap().is_none());
        // A profitable lot without a previous qualifying observation cannot join.
        s.previous_group_exit.remove("group-0");
        for x in &mut a { x.observed_ms = now + 1000; }
        let one = strategy::evaluate(&mut s, &bidir_books(now + 1000, 5, reverse), &a, now + 1000).unwrap().unwrap();
        assert_eq!(one.close_allocations.len(), 1);
        assert_eq!(one.close_lot_id.as_deref(), Some("group-1"));
        let (mut s, op) = confirmed_batch(now, reverse);
        s.pending = Some(op);
        assert!(strategy::normal_exit_eligible(&s, &bidir_books(now, 5, reverse), now).unwrap());
    }
}

#[test]
fn batch_exit_combined_depth_and_unsent_repricing_are_protected() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, op) = confirmed_batch(now, reverse);
        let mut thin = bidir_books(now, 5, reverse);
        // Each 1000-unit lot fits, but both together exceed protected liquidity.
        let side = s.direction.side(Venue::Lighter, Action::Close);
        let levels = if side == Side::Buy { &mut thin[0].asks } else { &mut thin[0].bids };
        levels[0].units = 1500;
        levels.push(Level { price: levels[0].price + Decimal::from(side.sign()) * d(5), units: 100000 });
        s.pending = Some(op.clone());
        assert!(!strategy::normal_exit_eligible(&s, &thin, now).unwrap());
        assert!(strategy::refresh_unsent_close_batch(&mut s, &thin, now).unwrap());
        assert_eq!(s.pending.as_ref().unwrap().requested_units, 1000);
        assert!(strategy::normal_exit_eligible(&s, &thin, now).unwrap());
        // A rebound removes only the unprofitable lot, retaining the good lot.
        s.pending = Some(op.clone());
        assert!(strategy::refresh_unsent_close_batch(&mut s, &bidir_books(now, 12, reverse), now).unwrap());
        assert_eq!(s.pending.as_ref().unwrap().close_lot_id.as_deref(), Some("group-1"));
        s.pending = Some(op);
        assert!(strategy::refresh_unsent_close_batch(&mut s, &bidir_books(now, 30, reverse), now).unwrap());
        assert!(!strategy::normal_exit_eligible(&s, &bidir_books(now, 30, reverse), now).unwrap());
    }
}

#[test]
fn batch_exit_partial_fill_is_allocated_once_and_invalid_plan_is_atomic() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, mut op) = confirmed_batch(now, reverse);
        let id = op.id.clone();
        op.first_filled = 1500; op.hedge_filled = 1500;
        op.first_terminal = true; op.hedge_terminal = true;
        for p in &mut s.positions { p.units = p.units.signum() * 500; }
        s.pending = Some(op.clone());
        let restored: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        s = restored;
        s.finish_operation(now).unwrap();
        assert_eq!(s.closed_groups, 1);
        assert_eq!(s.lots.len(), 1);
        assert_eq!(s.lots[0].id, "group-1"); assert_eq!(s.lots[0].units, 500);
        assert_eq!(s.closed_lot_allocations[&id], vec![
            CloseAllocation { lot_id: "group-0".into(), units: 1000 },
            CloseAllocation { lot_id: "group-1".into(), units: 500 }]);
        assert!(s.finish_operation(now).is_err());
        assert_eq!(s.lots[0].units, 500);
        let (mut bad, _) = confirmed_batch(now, reverse);
        for p in &mut bad.positions { p.units = p.units.signum() * 500; }
        op.close_allocations[1].lot_id = "missing".into();
        bad.pending = Some(op);
        let before = serde_json::to_string(&bad).unwrap();
        assert!(bad.finish_operation(now).is_err());
        assert_eq!(serde_json::to_string(&bad).unwrap(), before);
    }
}

#[test]
fn batch_exit_legacy_selected_lot_and_fifo_deserialize_without_new_fields() {
    let now = 4_000_000;
    for single in [false, true] {
        let (mut s, mut op) = confirmed_batch(now, false);
        op.close_allocations.clear();
        op.close_lot_id = single.then(|| "group-1".into());
        op.requested_units = 1000; op.first_filled = 1000; op.hedge_filled = 1000;
        op.first_terminal = true; op.hedge_terminal = true;
        for p in &mut s.positions { p.units = p.units.signum() * 1000; }
        s.pending = Some(op);
        let mut json = serde_json::to_value(&s).unwrap();
        json.as_object_mut().unwrap().remove("closed_lot_allocations");
        json["pending"].as_object_mut().unwrap().remove("close_allocations");
        let mut legacy: Snapshot = serde_json::from_value(json).unwrap();
        legacy.finish_operation(now).unwrap();
        assert_eq!(legacy.closed_groups, 1);
        assert_eq!(legacy.lots[0].id, if single { "group-0" } else { "group-1" });
    }
}

// A venue may accept only part of a merged IOC and disconnect before replying.
struct PartialBatchVenue(FaultVenue);
impl venue::VenueBackend for PartialBatchVenue {
    fn submit(&mut self, mut r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        assert!(r.reduce_only);
        if r.id.ends_with("-first") { r.units = 1500; }
        self.0.submit(r)
    }
    fn lookup(&mut self, r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> { self.0.lookup(r) }
    fn account(&mut self) -> venue::BoxFuture<'_, AccountEvidence> { self.0.account() }
}

#[tokio::test]
async fn batch_exit_execution_and_restart_never_resubmit_or_overclose() {
    use std::sync::{Arc, Mutex};
    for reverse in [false, true] {
        for partial in [false, true] {
            let now = crate::domain::now_ms();
            let (mut s, op) = confirmed_batch(now, reverse);
            let id = op.id.clone(); s.pending = Some(op);
            let cfg = s.config.clone();
            let path = std::env::temp_dir().join(format!("batch-exit-{}.sqlite", s.instance_id));
            let (mut db, _) = store::Store::open(&path, &cfg).unwrap();
            db.commit(&s, now, "reserved").unwrap();
            let remotes = s.positions.each_ref().map(|p| Arc::new(Mutex::new(Remote { position: p.clone(), ..Default::default() })));
            let b = bidir_books(now, 5, reverse);
            let workers = [Venue::Lighter, Venue::Entropy].map(|v| {
                let backend = FaultVenue { prices: [b[0].mid().unwrap(), b[1].mid().unwrap()], venue: v,
                    remote: remotes[v.index()].clone(), unknown_once: v == Venue::Lighter, fraction: 1 };
                let backend: Box<dyn venue::VenueBackend> = if partial { Box::new(PartialBatchVenue(backend)) } else { Box::new(backend) };
                venue::AccountWorker::spawn(v, Mode::Paper, true, backend).unwrap()
            });
            execution::advance(&mut s, &mut db, &workers, &b, now).await.unwrap();
            assert!(s.pending.as_ref().unwrap().first.is_some());
            assert_eq!(s.status, Status::RecoveringExposure);
            // Repricing after submission is forbidden, even after a rebound.
            let reserved = serde_json::to_string(&s.pending).unwrap();
            assert!(!strategy::refresh_unsent_close_batch(&mut s, &bidir_books(now, 30, reverse), now).unwrap());
            assert_eq!(serde_json::to_string(&s.pending).unwrap(), reserved);
            drop(db);
            let (mut db, mut s) = store::Store::open(&path, &cfg).unwrap();
            for _ in 0..8 {
                if s.pending.is_none() { break; }
                execution::advance(&mut s, &mut db, &workers, &b, now).await.unwrap();
            }
            assert!(s.pending.is_none());
            assert_eq!(s.closed_groups, if partial { 1 } else { 2 });
            assert_eq!(s.paired_units(), if partial { 500 } else { 0 });
            assert_eq!(s.closed_lot_allocations[&id].iter().map(|a| a.units).sum::<i64>(), if partial { 1500 } else { 2000 });
            for (i, remote) in remotes.iter().enumerate() {
                let r = remote.lock().unwrap();
                assert_eq!(r.submissions, 1);
                assert_eq!(r.position.units, s.positions[i].units);
            }
            assert_eq!(remotes[0].lock().unwrap().queries, 1);
            let fills = s.fills.len();
            execution::advance(&mut s, &mut db, &workers, &b, now).await.unwrap();
            assert_eq!(s.fills.len(), fills);
        }
    }
}

#[tokio::test]
async fn batch_exit_synthetic_spike_closes_all_confirmed_groups_in_one_wave() {
    use std::sync::{Arc, RwLock, atomic::{AtomicU64, Ordering}};
    for reverse in [false, true] {
        let start = 4_000_000;
        let (mut s, op) = confirmed_batch(start, reverse);
        let expected = s.lots.len() as u64;
        s.pending = Some(op);
        let shared = Arc::new(RwLock::new(bidir_books(start, 5, reverse)));
        let clock = Arc::new(AtomicU64::new(start));
        let workers = [Venue::Lighter, Venue::Entropy].map(|v| {
            let backend = venue::PaperBackend::new(v, s.config.clone(), s.positions[v.index()].clone(), shared.clone());
            venue::AccountWorker::spawn_replay(v, backend, clock.clone()).unwrap()
        });
        let (mut db, _) = store::Store::offline_replay(&s.config).unwrap();
        db.commit(&s, start, "reserved").unwrap();
        for index in 0..8 {
            let now = start + index * 100;
            let books = bidir_books(now, 5, reverse);
            *shared.write().unwrap() = books.clone();
            clock.store(now, Ordering::SeqCst);
            execution::advance(&mut s, &mut db, &workers, &books, now).await.unwrap();
            if s.pending.is_none() { break; }
        }
        assert!(s.pending.is_none());
        assert_eq!(s.closed_groups, expected);
        assert!(s.lots.is_empty());
        assert!(s.positions.iter().all(|p| p.units == 0));
        assert_eq!(s.fills.len(), 2, "one merged fill per venue");
        assert_eq!(s.closed_lot_allocations.len(), 1);
    }
}
