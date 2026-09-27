use super::*;
use rust_decimal::Decimal;
fn d(v: i64) -> Decimal {
    Decimal::from(v)
}
fn books(now: u64, spread: i64) -> [Book; 2] {
    [100, 100 + spread].map(|p| Book {
        bids: vec![Level {
            price: d(p),
            units: 100000,
        }],
        asks: vec![Level {
            price: d(p),
            units: 100000,
        }],
        received_ms: now,
        connected: true,
    })
}
fn accounts(now: u64) -> [AccountEvidence; 2] {
    [Venue::Lighter, Venue::Entropy].map(|venue| AccountEvidence {
        venue,
        account: "o1".into(),
        observed_ms: now,
        position_units: 0,
        free_margin: d(100),
        equity: d(100),
        leverage: 3,
        isolated: true,
        open_orders: 0,
        authenticated: true,
        liquidation_price: None,
    })
}
fn warmed(now: u64) -> Snapshot {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.status = Status::Running;
    s.funding_synced_ms = now;
    for i in 1..=240 {
        s.samples.push_back((now - i * 15000, d(10)));
    }
    s.samples.make_contiguous().sort_by_key(|x| x.0);
    s
}
#[test]
fn precision_is_exact_and_common_size_rounds_down() {
    assert_eq!(common_units(d(15), d(101)).unwrap(), 1480);
    assert!(units(Decimal::new(1, 5)).is_err());
    assert_eq!(quantity(1480), Decimal::new(148, 3));
}

#[test]
fn execution_cost_uses_arrival_mid_and_reports_legacy_coverage() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    let buy = Fill {
        id: "buy-fill".into(),
        order_id: "buy-order".into(),
        venue: Venue::Lighter,
        side: Side::Buy,
        units: 1000,
        price: d(101),
        fee: Decimal::new(1, 2),
        time_ms: 2000,
    };
    let sell = Fill {
        id: "sell-fill".into(),
        order_id: "sell-order".into(),
        venue: Venue::Entropy,
        side: Side::Sell,
        units: 500,
        price: d(109),
        fee: Decimal::new(2, 2),
        time_ms: 3000,
    };
    let legacy = Fill {
        id: "legacy-fill".into(),
        order_id: "legacy-order".into(),
        venue: Venue::Lighter,
        side: Side::Sell,
        units: 100,
        price: d(100),
        fee: Decimal::ZERO,
        time_ms: 1000,
    };

    assert!(s.record_fill(&legacy, None).unwrap());
    assert!(s.record_fill(&buy, Some(d(100))).unwrap());
    assert!(s.record_fill(&sell, Some(d(110))).unwrap());
    assert!(!s.record_fill(&buy, Some(d(1))).unwrap());
    assert_eq!(s.execution_cost, Decimal::new(15, 2));
    assert_eq!(s.cumulative_fees(), Decimal::new(3, 2));
    assert_eq!(s.execution_cost_started_ms, Some(2000));
    assert_eq!(s.execution_cost_tracked_fills, 2);
    assert_eq!(s.untracked_execution_fills(), 1);
}

#[test]
fn legacy_request_and_snapshot_default_execution_cost_fields() {
    let request: OrderRequest = serde_json::from_value(serde_json::json!({
        "id":"legacy", "venue":"lighter", "side":"buy", "units":100,
        "limit":"101", "reduce_only":false, "created_ms":1, "expires_ms":2,
        "signed_expires_ms":null
    }))
    .unwrap();
    assert_eq!(request.arrival_mid, None);

    let mut value =
        serde_json::to_value(Snapshot::new(InventoryConfig::default()).unwrap()).unwrap();
    for key in [
        "execution_cost",
        "execution_cost_started_ms",
        "execution_cost_tracked_fills",
    ] {
        value.as_object_mut().unwrap().remove(key);
    }
    let restored: Snapshot = serde_json::from_value(value).unwrap();
    assert_eq!(restored.execution_cost, Decimal::ZERO);
    assert_eq!(restored.execution_cost_started_ms, None);
    assert_eq!(restored.execution_cost_tracked_fills, 0);
}

fn loss_fixture(now: u64) -> (Snapshot, [AccountEvidence; 2]) {
    let mut s = warmed(now);
    s.config.fee_entropy = Decimal::ZERO;
    s.config.execution_slippage_bps = Decimal::ZERO;
    s.positions[0] = Position {
        units: 10_000,
        average: d(100),
        ..Default::default()
    };
    s.positions[1] = Position {
        units: -10_000,
        average: d(116),
        ..Default::default()
    };
    s.lots.push(Lot {
        entry_net_spread: None,
        id: "loss-owned".into(),
        level: 0,
        units: 10_000,
        opened_ms: now - 1000,
        entry_spread: d(16),
    });
    let mut a = accounts(now);
    a[0].position_units = 10_000;
    a[1].position_units = -10_000;
    (s, a)
}

#[test]
fn loss_stop_sums_both_legs_fees_and_funding_and_latches_at_boundary() {
    let now = 4_000_000;
    let (mut s, _) = loss_fixture(now);
    s.positions[0].realized = d(2);
    s.positions[0].fees = d(1);
    s.positions[1].funding = d(-1);
    // Open spread 16 -> exit spread 25 produces -9; carry sums to zero.
    assert!(!strategy::enforce_loss_limit(&mut s, &books(now, 25), now).unwrap());
    assert!(s.loss_stop.is_none());
    assert_eq!(s.total_pnl(&books(now, 26)).unwrap(), d(-10));
    assert!(strategy::enforce_loss_limit(&mut s, &books(now, 26), now).unwrap());
    assert_eq!(s.loss_stop.as_ref().unwrap().net_pnl, d(-10));
    assert!(s.paused && s.close_requested && s.stop_after_close);
    // A price rebound must not clear the stop.
    strategy::enforce_loss_limit(&mut s, &books(now, 8), now).unwrap();
    assert!(s.loss_stop.is_some() && s.close_requested);
    s.positions[0].units = 0;
    s.positions[1].units = 0;
    s.lots.clear();
    strategy::enforce_loss_limit(&mut s, &books(now, 8), now).unwrap();
    assert_eq!(s.status, Status::Stopped);
    assert!(s.stop_requested && s.loss_stop.is_some());
}

#[test]
fn loss_exit_preempts_warmup_sampling_and_unavailable_funding() {
    let now = 4_000_000;
    let (mut s, a) = loss_fixture(now);
    s.config.mode = Mode::Live;
    s.samples.clear();
    s.last_sample_ms = now; // no regular sample is due
    s.status = Status::Warming;
    s.funding_synced_ms = 0;
    let op = strategy::evaluate(&mut s, &books(now, 27), &a, now)
        .unwrap()
        .unwrap();
    assert_eq!(op.action, Action::Close);
    assert!(s.loss_stop.is_some() && s.stop_after_close);
    assert!(strategy::risk_check(&s, &books(now, 16), &a, now, 1000, Action::Open).is_err());
}

#[test]
fn loss_limit_uses_cumulative_pnl_not_one_leg_or_peak_drawdown() {
    let now = 4_000_000;
    let (mut s, _) = loss_fixture(now);
    s.positions[0].realized = d(-20);
    s.positions[1].realized = d(15);
    s.peak_pnl = d(100);
    s.round_start_pnl = d(20);
    assert_eq!(s.total_pnl(&books(now, 16)).unwrap(), d(-5));
    assert!(!strategy::enforce_loss_limit(&mut s, &books(now, 16), now).unwrap());
}

#[test]
fn loss_stop_survives_restart_and_legacy_config_gets_ten_usdc_cap() {
    let now = 4_000_000;
    let (mut s, _) = loss_fixture(now);
    let path = std::env::temp_dir().join(format!("inventory-loss-{}.sqlite", s.instance_id));
    let cfg = s.config.clone();
    let (mut db, _) = store::Store::open(&path, &cfg).unwrap();
    strategy::enforce_loss_limit(&mut s, &books(now, 26), now).unwrap();
    db.commit(&s, now, "loss_limit_latched").unwrap();
    drop(db);
    let (_, loaded) = store::Store::open(&path, &cfg).unwrap();
    assert_eq!(loaded.loss_stop.unwrap().net_pnl, d(-10));
    assert!(loaded.paused && loaded.stop_after_close && loaded.close_requested);
    let mut legacy =
        serde_json::to_value(Snapshot::new(InventoryConfig::default()).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("loss_stop");
    legacy["config"]
        .as_object_mut()
        .unwrap()
        .remove("max_loss_usdc");
    let migrated: Snapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(migrated.config.max_loss_usdc, d(10));
    assert!(migrated.loss_stop.is_none());
    let mut bad = cfg;
    bad.max_loss_usdc = d(31);
    assert!(bad.validate().is_err());
    bad.max_loss_usdc = d(0);
    assert!(bad.validate().is_err());
}
#[test]
fn instance_ids_do_not_reuse_order_namespace() {
    let a = Snapshot::new(InventoryConfig::default()).unwrap();
    let b = Snapshot::new(InventoryConfig::default()).unwrap();
    assert_ne!(a.instance_id, b.instance_id);
}
#[test]
fn config_blocks_wrong_account_and_excess_exposure() {
    let mut c = InventoryConfig::default();
    c.max_groups = 21;
    assert!(c.validate().is_err());
    c.max_groups = 20;
    c.entropy_address = "0x1".into();
    assert!(c.validate().is_err());
}
#[test]
fn no_entry_before_two_consecutive_samples() {
    let now = 4_000_000;
    let mut s = warmed(now);
    assert!(
        strategy::evaluate(&mut s, &books(now, 16), &accounts(now), now)
            .unwrap()
            .is_none()
    );
    let t = now + 15000;
    let op = strategy::evaluate(&mut s, &books(t, 16), &accounts(t), t)
        .unwrap()
        .unwrap();
    assert_eq!(op.action, Action::Open);
    assert_eq!(op.requested_units % 10, 0);
    assert!(quantity(op.requested_units) * d(108) <= d(15));
}

#[test]
fn jittered_real_samples_complete_warmup_but_long_gaps_do_not() {
    let now = 4_000_000;
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.status = Status::Warming;
    // Actual overnight cadence was ~16.52 seconds, only 218 samples per hour.
    for i in (1..=217).rev() {
        s.samples.push_back((now - i * 16_520, d(10)));
    }
    assert!(strategy::sampling_progress(&s, now).ready);
    strategy::evaluate(&mut s, &books(now, 10), &accounts(now), now).unwrap();
    assert_eq!(s.status, Status::Running);
    s.samples
        .retain(|(t, _)| *t < now - 3_000_000 || *t > now - 900_000);
    s.continuity_mean = None; // Test raw coverage without an established fallback.
    assert!(!strategy::sampling_progress(&s, now).ready);
    let mut short = Snapshot::new(InventoryConfig::default()).unwrap();
    for i in (1..=60).rev() {
        short.samples.push_back((now - i * 15_000, d(10)));
    }
    assert!(!strategy::sampling_progress(&short, now).ready);
    // Dense bursts do not manufacture elapsed coverage; stale history expires.
    for i in 1..=300 {
        short.samples.push_back((now - 301 + i, d(10)));
    }
    assert!(!strategy::sampling_progress(&short, now).ready);
    assert!(!strategy::sampling_progress(&s, now + 3_600_000).ready);
}

#[test]
fn sampling_uses_fixed_slots_without_catchup_or_stale_quotes() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.status = Status::Warming;
    for t in [4_005_000, 4_021_500, 4_035_000] {
        strategy::evaluate(&mut s, &books(t, 10), &accounts(t), t).unwrap();
    }
    assert_eq!(s.samples.len(), 3); // 1.5s delay is recovered at the next slot.
    let t = 4_035_250;
    strategy::evaluate(&mut s, &books(t, 10), &accounts(t), t).unwrap();
    assert_eq!(s.samples.len(), 3);
    let t = 4_065_000;
    assert!(strategy::evaluate(&mut s, &books(t - 1501, 10), &accounts(t), t).is_err());
    assert_eq!(s.samples.len(), 3);
    strategy::evaluate(&mut s, &books(t, 10), &accounts(t), t).unwrap();
    assert_eq!(s.samples.len(), 4); // No invented sample for the missed slot.
}

#[test]
fn initialized_mean_survives_maintenance_without_fabricating_history() {
    let now = crate::domain::now_ms();
    let mut s = warmed(now);
    s.last_sample_ms = now - 15_000;
    assert!(!s.mean_initialized);
    let before = s.samples.clone();
    let path = std::env::temp_dir().join(format!("mean-resume-{}.sqlite", s.instance_id));
    {
        let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
        db.commit(&s, now, "before_maintenance").unwrap();
    }
    let (_, restored) = store::Store::open(&path, &s.config).unwrap();
    assert!(restored.mean_initialized);
    assert_eq!(restored.samples, before);
    assert!(strategy::sampling_progress(&restored, now + 5 * 60_000).ready);
    let continuity = strategy::sampling_progress(&restored, now + 35 * 60_000);
    assert!(continuity.ready && continuity.continuity_active);
    assert!(continuity.covered_ms < continuity.required_ms);
    assert!(!strategy::sampling_progress(&restored, now + 61 * 60_000).ready);
    let mut initial = restored.clone();
    initial.mean_initialized = false;
    assert!(!strategy::sampling_progress(&initial, now + 5 * 60_000).ready);
}

#[test]
fn entry_tuning_is_flat_only_durable_and_preserves_loss_accounting() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.paused = true;
    s.positions[0].realized = d(-2);
    let id = s.instance_id.clone();
    assert!(service::set_entry_offset(&mut s, d(1), Some(&accounts(now - 3001)), now).is_err());
    service::set_entry_offset(&mut s, d(1), Some(&accounts(now)), now).unwrap();
    assert_eq!(s.config.entry_offset, d(1));
    assert_eq!(s.positions[0].realized, d(-2));
    assert_eq!(s.config.max_loss_usdc, d(10));
    assert_eq!(s.instance_id, id);
    assert!(s.previous_signal.is_none());
    let path = std::env::temp_dir().join(format!("inventory-tuning-{}.sqlite", s.instance_id));
    {
        let (mut db, _) = store::Store::open(&path, &InventoryConfig::default()).unwrap();
        db.command(&s, "tune", "set_entry_offset", now).unwrap();
    }
    {
        let (_, restored) = store::Store::open(&path, &s.config).unwrap();
        assert_eq!(restored.config.entry_offset, d(1));
        assert_eq!(restored.positions[0].realized, d(-2));
    }
    s.paused = false;
    assert!(service::set_entry_offset(&mut s, d(2), Some(&accounts(now)), now).is_err());
    s.paused = true;
    s.positions[0].units = 100;
    assert!(service::set_entry_offset(&mut s, d(2), Some(&accounts(now)), now).is_err());
    s.positions[0].units = 0;
    s.loss_stop = Some(LossStop {
        at_ms: now,
        net_pnl: d(-10),
    });
    assert!(service::set_entry_offset(&mut s, d(2), Some(&accounts(now)), now).is_err());
}

#[test]
fn loss_limit_tuning_is_stopped_flat_only_and_accepts_thirty() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.paused = true;
    s.stop_requested = true;
    s.status = Status::Stopped;
    service::set_max_loss(&mut s, d(30), Some(&accounts(now)), now).unwrap();
    assert_eq!(s.config.max_loss_usdc, d(30));
    assert!(s.reason.contains("30 USDC"));

    let mut stale = s.clone();
    assert!(service::set_max_loss(&mut stale, d(20), Some(&accounts(now - 3001)), now).is_err());
    let mut running = s.clone();
    running.paused = false;
    running.stop_requested = false;
    assert!(service::set_max_loss(&mut running, d(20), Some(&accounts(now)), now).is_err());
    let mut exposed = s.clone();
    exposed.positions[0].units = 1;
    assert!(service::set_max_loss(&mut exposed, d(20), Some(&accounts(now)), now).is_err());
    assert!(service::set_max_loss(&mut s, d(31), Some(&accounts(now)), now).is_err());
}

#[test]
fn ordinary_start_clears_exhausted_bounded_entry_budget() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.entry_attempts_remaining = Some(0);
    s.paused = true;
    s.stop_requested = true;
    s.status = Status::Stopped;
    service::start_continuous(&mut s).unwrap();
    assert_eq!(s.entry_attempts_remaining, None);
    assert!(!s.paused && !s.stop_requested);
    assert_eq!(s.status, Status::Recovering);

    s.pending = Some(Operation {
        close_allocations: Vec::new(),
        close_lot_id: None,
        id: "pending".into(),
        action: Action::Open,
        level: 0,
        requested_units: 1,
        created_ms: 1,
        first: None,
        hedge: None,
        repair: None,
        repair_attempt: 0,
        repair_retry_after_ms: None,
        align_close: None,
        first_terminal: false,
        hedge_terminal: false,
        repair_terminal: false,
        align_close_terminal: false,
        first_filled: 0,
        hedge_filled: 0,
        repair_filled: 0,
        align_close_filled: 0,
        first_value: Decimal::ZERO,
        hedge_value: Decimal::ZERO,
        failed: false,
        first_venue: Venue::Entropy,
        min_entry_spread: None,
        unwind_hedge: None,
        unwind_hedge_terminal: false,
        unwind_hedge_filled: 0,
        quote_wait_started_ms: None,
    });
    assert!(service::start_continuous(&mut s).is_err());
}
#[test]
fn stale_and_unowned_state_block_risk() {
    let now = 4_000_000;
    let s = warmed(now);
    let b = books(now, 16);
    let a = accounts(now);
    assert!(strategy::risk_check(&s, &b, &a, now, 1400, Action::Open).is_ok());
    let mut bad = a.clone();
    bad[1].position_units = 10;
    assert!(strategy::risk_check(&s, &b, &bad, now, 1400, Action::Open).is_err());
    let mut bad = a.clone();
    bad[0].free_margin = d(10);
    assert!(strategy::risk_check(&s, &b, &bad, now, 1400, Action::Open).is_err());
    assert!(strategy::risk_check(&s, &b, &a, now + 3001, 1400, Action::Open).is_err());
}
#[test]
fn insufficient_depth_and_disconnection_block() {
    let now = 100;
    let mut b = books(now, 10)[0].clone();
    assert!(b.vwap(Side::Buy, 100001).is_err());
    b.connected = false;
    assert!(b.validate(now, 1500).is_err());
}
#[test]
fn five_level_book_requires_full_quantity_for_both_hedge_directions() {
    let now=100;
    let mut pair=books(now,10);
    pair[1].bids=(0..5).map(|i|Level {price:Decimal::new(11000-i,2), units:10}).collect();
    pair[1].asks=(0..5).map(|i|Level {price:Decimal::new(11001+i,2), units:10}).collect();
    assert!(pair[1].validate(now,1500).is_ok());
    for side in [Side::Buy,Side::Sell] {
        assert!(pair[1].vwap(side,50).is_ok());
        assert!(pair[1].vwap(side,60).unwrap_err().to_string().contains("insufficient depth"));
    }
    for direction in [Direction::LighterLong,Direction::LighterShort] {
        assert!(direction.entry(&pair,50).is_ok());
        assert!(direction.entry(&pair,60).is_err());
        assert!(direction.exit(&pair,60).is_err());
    }
}
#[test]
fn fills_are_deduplicated_and_realized_pnl_accounts_fees() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    let mut f = Fill {
        id: "a".into(),
        order_id: "o".into(),
        venue: Venue::Lighter,
        side: Side::Buy,
        units: 1000,
        price: d(100),
        fee: Decimal::new(1, 2),
        time_ms: 1,
    };
    assert!(s.record_fill(&f, None).unwrap());
    assert!(!s.record_fill(&f, None).unwrap());
    f.id = "b".into();
    f.side = Side::Sell;
    f.price = d(110);
    s.record_fill(&f, None).unwrap();
    assert_eq!(s.positions[0].units, 0);
    assert_eq!(s.positions[0].realized, d(1));
    assert_eq!(s.positions[0].fees, Decimal::new(2, 2));
}
#[test]
fn durable_mode_and_single_database_owner() {
    let path = std::env::temp_dir().join(format!(
        "inventory-test-{}.sqlite",
        Snapshot::new(InventoryConfig::default())
            .unwrap()
            .instance_id
    ));
    let c = InventoryConfig::default();
    let (mut db, mut s) = store::Store::open(&path, &c).unwrap();
    s.sequence = 11;
    db.commit(&s, 1, "test").unwrap();
    assert!(store::Store::open(&path, &c).is_err());
    drop(db);
    let (db, loaded) = store::Store::open(&path, &c).unwrap();
    assert_eq!(loaded.sequence, 11);
    assert_eq!(loaded.instance_id, s.instance_id);
    drop(db);
    let mut live = c;
    live.mode = Mode::Live;
    assert!(store::Store::open(&path, &live).is_err());
    let _ = std::fs::remove_file(path);
}
#[test]
fn command_id_cannot_be_reused_for_another_action() {
    let path = std::env::temp_dir().join(format!(
        "inventory-command-{}.sqlite",
        Snapshot::new(InventoryConfig::default())
            .unwrap()
            .instance_id
    ));
    let (mut db, s) = store::Store::open(&path, &InventoryConfig::default()).unwrap();
    db.command(&s, "one", "pause", 1).unwrap();
    assert!(db.command_seen("one", "pause").unwrap());
    assert!(db.command_seen("one", "start").is_err());
}
#[tokio::test]
async fn process_dry_run_is_hard_worker_gate() {
    let c = InventoryConfig::default();
    let b = venue::PaperBackend::new(
        Venue::Lighter,
        c,
        Position::default(),
        std::sync::Arc::new(std::sync::RwLock::new(books(1, 10))),
    );
    assert!(venue::AccountWorker::spawn(Venue::Lighter, Mode::Live, true, Box::new(b)).is_err());
}

struct FaultVenue {
    prices: [Decimal; 2],
    venue: Venue,
    remote: std::sync::Arc<std::sync::Mutex<Remote>>,
    unknown_once: bool,
    fraction: i64,
}
#[derive(Default)]
struct Remote {
    position: Position,
    orders: std::collections::BTreeMap<String, OrderResult>,
    submissions: usize,
    queries: usize,
}
impl venue::VenueBackend for FaultVenue {
    fn submit(&mut self, r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        Box::pin(async move {
            let mut remote = self.remote.lock().unwrap();
            remote.submissions += 1;
            anyhow::ensure!(!remote.orders.contains_key(&r.id), "duplicate remote order");
            let qty = if r.reduce_only {
                r.units
            } else {
                r.units / self.fraction
            };
            let f = Fill {
                id: format!("{}:trade", r.id),
                order_id: r.id.clone(),
                venue: r.venue,
                side: r.side,
                units: qty,
                price: self.prices[self.venue.index()],
                fee: Decimal::ZERO,
                time_ms: r.created_ms,
            };
            remote.position.apply(&f)?;
            let result = OrderResult {
                exchange_created_ms: None,
                terminal: true,
                fills: vec![f],
                reason: "fixture exchange execution".into(),
            };
            remote.orders.insert(r.id, result.clone());
            if self.unknown_once {
                self.unknown_once = false;
                anyhow::bail!("fixture disconnect after acceptance");
            }
            Ok(result)
        })
    }
    fn lookup(&mut self, r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        Box::pin(async move {
            let mut remote = self.remote.lock().unwrap();
            remote.queries += 1;
            Ok(remote.orders.get(&r.id).unwrap().clone())
        })
    }
    fn account(&mut self) -> venue::BoxFuture<'_, AccountEvidence> {
        Box::pin(async move {
            let mut a = accounts(crate::domain::now_ms())[self.venue.index()].clone();
            a.position_units = self.remote.lock().unwrap().position.units;
            Ok(a)
        })
    }
}
#[tokio::test]
async fn unknown_acceptance_restart_queries_original_id_then_partial_hedge_repairs() {
    use std::sync::{Arc, Mutex};
    let now = crate::domain::now_ms();
    let mut s = warmed(now);
    s.previous_signal = Some((now - 15000, d(16), d(10)));
    let b = books(now, 16);
    s.pending = strategy::evaluate(&mut s, &b, &accounts(now), now).unwrap();
    // Retain regression coverage for pre-upgrade, Lighter-first operations.
    s.pending.as_mut().unwrap().first_venue = Venue::Lighter;
    let cfg = s.config.clone();
    let path = std::env::temp_dir().join(format!("inventory-fault-{}.sqlite", s.instance_id));
    let (mut db, _) = store::Store::open(&path, &cfg).unwrap();
    db.commit(&s, now, "reserved").unwrap();
    let l = Arc::new(Mutex::new(Remote::default()));
    let e = Arc::new(Mutex::new(Remote::default()));
    let workers = [
        venue::AccountWorker::spawn(
            Venue::Lighter,
            Mode::Paper,
            true,
            Box::new(FaultVenue {
                    prices: [d(100), d(116)],
                venue: Venue::Lighter,
                remote: l.clone(),
                unknown_once: true,
                fraction: 1,
            }),
        )
        .unwrap(),
        venue::AccountWorker::spawn(
            Venue::Entropy,
            Mode::Paper,
            true,
            Box::new(FaultVenue {
                    prices: [d(100), d(116)],
                venue: Venue::Entropy,
                remote: e.clone(),
                unknown_once: false,
                fraction: 2,
            }),
        )
        .unwrap(),
    ];
    execution::advance(&mut s, &mut db, &workers, &b, now)
        .await
        .unwrap();
    assert_eq!(s.status, Status::RecoveringExposure);
    assert!(s.pending.as_ref().unwrap().first.is_some());
    drop(db);
    let (mut db, mut s) = store::Store::open(&path, &cfg).unwrap();
    for _ in 0..8 {
        execution::advance(&mut s, &mut db, &workers, &b, now)
            .await
            .unwrap();
        if s.pending.is_none() {
            break;
        }
    }
    assert!(s.pending.is_none());
    assert_eq!(s.status, Status::NeedsAttention);
    assert_eq!(l.lock().unwrap().submissions, 2); // one entry, one owned rollback, never a duplicate entry
    assert_eq!(l.lock().unwrap().queries, 1);
    assert_eq!(e.lock().unwrap().submissions, 1);
    assert_eq!(s.positions[0].units, -s.positions[1].units);
    assert_eq!(s.paired_units(), s.positions[0].units);
}
#[tokio::test]
async fn loss_stop_cancels_unsent_entry_but_reconciles_sent_entry_then_flattens() {
    use std::sync::{Arc, Mutex};
    for (sent, first_venue) in [false, true]
        .into_iter()
        .flat_map(|sent| [Venue::Lighter, Venue::Entropy].map(|v| (sent, v)))
    {
        let now = crate::domain::now_ms();
        let b = books(now, 16);
        let mut s = warmed(now);
        let mut a = accounts(now);
        strategy::evaluate(&mut s, &b, &a, now).unwrap();
        let t = now + 15000;
        a.iter_mut().for_each(|a| a.observed_ms = t);
        let b = books(t, 16);
        s.pending = strategy::evaluate(&mut s, &b, &a, t).unwrap();
        s.pending.as_mut().unwrap().first_venue = first_venue;
        let path = std::env::temp_dir().join(format!("loss-inflight-{}.sqlite", s.instance_id));
        let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
        db.commit(&s, t, "reserved").unwrap();
        let l = Arc::new(Mutex::new(Remote::default()));
        let e = Arc::new(Mutex::new(Remote::default()));
        let workers = [Venue::Lighter, Venue::Entropy].map(|venue| {
            venue::AccountWorker::spawn(
                venue,
                Mode::Paper,
                true,
                Box::new(FaultVenue {
                    prices: [d(100), d(116)],
                    venue,
                    remote: if venue == Venue::Lighter {
                        l.clone()
                    } else {
                        e.clone()
                    },
                    unknown_once: venue == first_venue,
                    fraction: 1,
                }),
            )
            .unwrap()
        });
        if sent {
            execution::advance(&mut s, &mut db, &workers, &b, t)
                .await
                .unwrap();
            assert!(s.pending.as_ref().unwrap().first.is_some());
        }
        // A settled funding debit reaches the cap while entry is reserved/unknown.
        s.positions[0].funding = d(-11);
        execution::advance(&mut s, &mut db, &workers, &b, t)
            .await
            .unwrap();
        assert!(s.loss_stop.is_some());
        if !sent {
            assert!(s.pending.is_none());
            assert_eq!(l.lock().unwrap().submissions, 0);
            assert_eq!(e.lock().unwrap().submissions, 0);
            assert_eq!(s.status, Status::Stopped);
            continue;
        }
        for _ in 0..5 {
            execution::advance(&mut s, &mut db, &workers, &b, t)
                .await
                .unwrap();
            if s.pending.is_none() {
                break;
            }
        }
        assert_eq!(l.lock().unwrap().submissions, 1);
        assert_eq!(
            l.lock().unwrap().queries,
            usize::from(first_venue == Venue::Lighter)
        );
        assert_eq!(
            e.lock().unwrap().queries,
            usize::from(first_venue == Venue::Entropy)
        );
        assert_eq!(e.lock().unwrap().submissions, 1);
        assert_eq!(s.status, Status::Closing);
        a[0].position_units = s.positions[0].units;
        a[1].position_units = s.positions[1].units;
        s.pending = strategy::evaluate(&mut s, &b, &a, t).unwrap();
        assert_eq!(s.pending.as_ref().unwrap().action, Action::Close);
        for _ in 0..5 {
            execution::advance(&mut s, &mut db, &workers, &b, t)
                .await
                .unwrap();
            if s.pending.is_none() {
                break;
            }
        }
        assert_eq!(s.status, Status::Stopped);
        assert_eq!(s.positions[0].units, 0);
        assert_eq!(s.positions[1].units, 0);
        assert_eq!(l.lock().unwrap().position.units, 0);
        assert_eq!(e.lock().unwrap().position.units, 0);
        assert!(s.loss_stop.is_some() && s.lots.is_empty());
    }
}

#[tokio::test]
async fn expired_unsent_entry_is_discarded_but_unknown_submission_is_queried() {
    use std::sync::{Arc, Mutex};
    for case in 0..4 {
        let now = crate::domain::now_ms();
        let mut s = warmed(now);
        strategy::evaluate(&mut s, &books(now, 16), &accounts(now), now).unwrap();
        let t = now + 15_000;
        s.pending = strategy::evaluate(&mut s, &books(t, 16), &accounts(t), t).unwrap();
        assert!(s.pending.is_some());
        let path = std::env::temp_dir().join(format!("unsent-{}.sqlite", s.instance_id));
        let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
        let remote = Arc::new(Mutex::new(Remote::default()));
        let workers = [Venue::Lighter, Venue::Entropy].map(|venue| {
            venue::AccountWorker::spawn(
                venue,
                Mode::Paper,
                true,
                Box::new(FaultVenue {
                    prices: [d(100), d(116)],
                    venue,
                    remote: remote.clone(),
                    unknown_once: true,
                    fraction: 1,
                }),
            )
            .unwrap()
        });
        if case == 3 {
            execution::advance(&mut s, &mut db, &workers, &books(t, 16), t)
                .await
                .unwrap();
        }
        let at = if case == 2 { t + 5_001 } else { t + 1_501 };
        let b = if case == 1 {
            books(at, 10)
        } else if case == 2 {
            books(at, 16)
        } else {
            books(t, 16)
        };
        execution::advance(&mut s, &mut db, &workers, &b, at)
            .await
            .unwrap();
        if case == 3 {
            assert!(s.pending.as_ref().unwrap().first.is_some());
            assert_eq!(remote.lock().unwrap().submissions, 1);
            assert_eq!(remote.lock().unwrap().queries, 1);
        } else {
            assert!(s.pending.is_none());
            assert!(s.previous_signal.is_none());
            assert_eq!(s.status, Status::Running);
            assert_eq!(remote.lock().unwrap().submissions, 0);
        }
    }
}

#[tokio::test]
async fn entropy_first_restart_partial_hedge_and_spread_budget() {
    use std::sync::{Arc, Mutex};
    for direction in [Direction::LighterLong, Direction::LighterShort] {
    let sign = direction.sign();
    // full fill, partial first, common partial hedge, non-common hedge, budget rejection
    for (first_fraction, hedge_fraction, budget_reject, expected) in [
        (1, 1, false, 2000),
        (2, 1, false, 1000),
        (1, 2, false, 1000),
        (1, 3, false, 0),
        (1, 1, true, 0),
    ] {
        let now = crate::domain::now_ms();
        let mut s = warmed(now);
        s.config.direction_policy = DirectionPolicy::Both;
        if sign == 1 { s.previous_signal = Some((now - 15000, d(16), d(10))); }
        else { for (_, value) in &mut s.samples { *value = -*value; }
            s.previous_reverse_signal = Some((now - 15000, d(16), d(10))); }
        let mut b = books(now, 16);
        if sign == -1 { b.swap(0, 1); }
        s.pending = strategy::evaluate(&mut s, &b, &accounts(now), now).unwrap();
        let op = s.pending.as_mut().unwrap();
        assert_eq!(op.first_venue, Venue::Entropy);
        op.requested_units = 2000;
        let mut repair_recovery = s.clone();
        let p = repair_recovery.pending.as_mut().unwrap();
        p.first_terminal = true;
        p.first_filled = 2000;
        p.hedge_terminal = true;
        p.repair = Some(OrderRequest {
            id: format!("{}-v2-repair", p.id),
            venue: Venue::Entropy,
            side: direction.open_side(Venue::Entropy).opposite(),
            units: 2000,
            limit: d(120),
            arrival_mid: None,
            reduce_only: true,
            created_ms: now - 10_000,
            expires_ms: now - 5_000,
            signed_expires_ms: None,
        });
        p.repair_terminal = true;
        repair_recovery.positions[1].units = -sign * 2000;
        let mut repair_accounts = accounts(now);
        repair_accounts[0].position_units = repair_recovery.positions[0].units;
        repair_accounts[1].position_units = repair_recovery.positions[1].units;
        execution::resume_terminal_repair(&mut repair_recovery, Some(&repair_accounts), now)
            .unwrap();
        let retried = repair_recovery.pending.as_ref().unwrap();
        assert!(!retried.repair_terminal);
        assert!(retried.repair.is_none());
        assert_eq!(retried.repair_attempt, 1);
        let op = s.pending.as_mut().unwrap();
        let mut legacy = serde_json::to_value(&op).unwrap();
        for k in [
            "first_venue",
            "min_entry_spread",
            "unwind_hedge",
            "unwind_hedge_filled",
            "unwind_hedge_terminal",
        ] {
            legacy.as_object_mut().unwrap().remove(k);
        }
        assert_eq!(
            serde_json::from_value::<Operation>(legacy)
                .unwrap()
                .first_venue,
            Venue::Lighter
        );
        let path = std::env::temp_dir().join(format!("entropy-first-{}.sqlite", s.instance_id));
        let cfg = s.config.clone();
        let (mut db, _) = store::Store::open(&path, &cfg).unwrap();
        let l = Arc::new(Mutex::new(Remote::default()));
        let e = Arc::new(Mutex::new(Remote::default()));
        let workers = [Venue::Lighter, Venue::Entropy].map(|venue| {
            venue::AccountWorker::spawn(
                venue,
                Mode::Paper,
                true,
                Box::new(FaultVenue {
                    prices: if sign == 1 { [d(100), d(116)] } else { [d(116), d(100)] },
                    venue,
                    remote: if venue == Venue::Lighter {
                        l.clone()
                    } else {
                        e.clone()
                    },
                    unknown_once: true,
                    fraction: if venue == Venue::Entropy {
                        first_fraction
                    } else {
                        hedge_fraction
                    },
                }),
            )
            .unwrap()
        });
        execution::advance(&mut s, &mut db, &workers, &b, now)
            .await
            .unwrap();
        assert_eq!(l.lock().unwrap().submissions, 0);
        assert_eq!(e.lock().unwrap().submissions, 1);
        assert_eq!(
            s.pending.as_ref().unwrap().first.as_ref().unwrap().venue,
            Venue::Entropy
        );
        drop(db);
        let (mut db, mut s) = store::Store::open(&path, &cfg).unwrap();
        assert_eq!(s.pending.as_ref().unwrap().first_venue, Venue::Entropy);
        // Unknown first order is queried even with no current book.
        let mut stale = b.clone();
        stale.iter_mut().for_each(|b| b.connected = false);
        execution::advance(&mut s, &mut db, &workers, &stale, now)
            .await
            .unwrap();
        assert_eq!(e.lock().unwrap().queries, 1);
        // A slow first-leg lookup must not consume the next phase's quote wait.
        let delayed = now + 6000;
        execution::advance(&mut s, &mut db, &workers, &stale, now)
            .await
            .unwrap();
        assert!(s.pending.as_ref().unwrap().hedge.is_none());
        assert!(!s.pending.as_ref().unwrap().hedge_terminal);
        s.pending.as_mut().unwrap().quote_wait_started_ms = None;
        execution::advance(&mut s, &mut db, &workers, &stale, delayed)
            .await
            .unwrap();
        assert!(!s.pending.as_ref().unwrap().hedge_terminal);
        let mut next_books = b.clone();
        if budget_reject {
            if sign == 1 { next_books[0].asks[0].price = d(103); }
            else { next_books[0].bids[0].price = d(113); }
        }
        execution::advance(&mut s, &mut db, &workers, &next_books, now)
            .await
            .unwrap();
        if s.pending
            .as_ref()
            .is_some_and(|p| p.hedge.is_some() && !p.hedge_terminal)
        {
            drop(db);
            let restored = store::Store::open(&path, &cfg).unwrap();
            db = restored.0;
            s = restored.1;
            assert_eq!(
                s.pending.as_ref().unwrap().hedge.as_ref().unwrap().venue,
                Venue::Lighter
            );
        }
        for _ in 0..12 {
            execution::advance(&mut s, &mut db, &workers, &next_books, now)
                .await
                .unwrap();
            if s.pending.is_none() {
                break;
            }
        }
        assert!(
            s.pending.is_none(),
            "pending operation after fixture recovery"
        );
        assert_eq!(s.positions[0].units, sign * expected);
        assert_eq!(s.positions[1].units, -sign * expected);
        assert_eq!(l.lock().unwrap().position.units, sign * expected);
        assert_eq!(e.lock().unwrap().position.units, -sign * expected);
        if expected > 0 {
            assert_eq!(s.lots[0].entry_spread, d(16));
            assert_eq!(s.lots[0].units, expected);
            let remote = l.lock().unwrap();
            let req = s
                .fills
                .values()
                .find(|f| f.venue == Venue::Lighter)
                .unwrap();
            assert_eq!(req.side, direction.open_side(Venue::Lighter));
            drop(remote);
        }
        if budget_reject {
            assert_eq!(l.lock().unwrap().submissions, 0);
        } else {
            assert_eq!(l.lock().unwrap().queries, 1);
        }
        if hedge_fraction == 3 {
            assert_eq!(l.lock().unwrap().submissions, 2);
            assert_eq!(e.lock().unwrap().submissions, 2);
        }
    }
}
}

#[test]
fn mature_mean_continuity_is_bounded_and_never_lowers_entry_reference() {
    let now = crate::domain::now_ms();
    let mut s = warmed(now);
    s.mean_initialized = true;
    s.samples.clear();
    s.samples.push_back((now, d(8)));
    s.continuity_mean = Some((now - 1000, d(10)));
    assert!(strategy::sampling_progress(&s, now).continuity_active);
    assert_eq!(strategy::reference_mean(&s, now), Some(d(10)));
    s.samples[0].1 = d(12);
    assert_eq!(strategy::reference_mean(&s, now), Some(d(12)));
    assert!(!strategy::sampling_progress(&s, now + s.config.mean_window_ms).ready);
    s.mean_initialized = false;
    assert!(!strategy::sampling_progress(&s, now).ready);
}

#[test]
fn aggregate_exit_requires_two_signals_and_freezes_new_entries() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.lots.push(Lot {
        entry_net_spread: None,
        id: "owned".into(),
        level: 0,
        units: 1500,
        opened_ms: now - 60000,
        entry_spread: d(16),
    });
    s.positions[0] = Position {
        units: 1500,
        average: d(100),
        ..Default::default()
    };
    s.positions[1] = Position {
        units: -1500,
        average: d(116),
        ..Default::default()
    };
    s.anchor = Some(d(16));
    let mut a = accounts(now);
    a[0].position_units = 1500;
    a[1].position_units = -1500;
    assert!(
        strategy::evaluate(&mut s, &books(now, 8), &a, now)
            .unwrap()
            .is_none()
    );
    let t = now + 15000;
    a.iter_mut().for_each(|a| a.observed_ms = t);
    assert_eq!(
        strategy::evaluate(&mut s, &books(t, 8), &a, t)
            .unwrap()
            .unwrap()
            .action,
        Action::Close
    );
    assert!(s.exit_batch_active);
    let t = t + 15000;
    a.iter_mut().for_each(|a| a.observed_ms = t);
    assert!(
        strategy::evaluate(&mut s, &books(t, 30), &a, t)
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn revoked_vault_lease_drops_backend_and_rejects_new_work() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let enabled = Arc::new(AtomicBool::new(true));
    let token = enabled.clone();
    let b = venue::PaperBackend::new(
        Venue::Lighter,
        InventoryConfig::default(),
        Position::default(),
        Arc::new(std::sync::RwLock::new(books(1, 10))),
    );
    let worker = venue::AccountWorker::spawn(
        Venue::Lighter,
        Mode::Paper,
        true,
        Box::new(venue::GuardedBackend {
            inner: Box::new(b),
            lease: Arc::new(move || token.load(Ordering::SeqCst)),
        }),
    )
    .unwrap();
    assert!(worker.account().await.is_ok());
    enabled.store(false, Ordering::SeqCst);
    assert!(worker.account().await.is_err());
}
#[test]
fn conflicting_duplicate_fill_is_rejected() {
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    let mut f = Fill {
        id: "id".into(),
        order_id: "x".into(),
        venue: Venue::Lighter,
        side: Side::Buy,
        units: 100,
        price: d(100),
        fee: Decimal::ZERO,
        time_ms: 1,
    };
    s.record_fill(&f, None).unwrap();
    f.units = 200;
    assert!(s.record_fill(&f, None).is_err());
    assert_eq!(s.positions[0].units, 100);
}
#[test]
fn all_account_risk_boundaries_fail_closed() {
    let now = 4000000;
    let s = warmed(now);
    let b = books(now, 16);
    let good = accounts(now);
    for case in 0..8 {
        let mut a = good.clone();
        match case {
            0 => a[0].authenticated = false,
            1 => a[1].isolated = false,
            2 => a[0].leverage = 6,
            3 => a[1].account = "o2".into(),
            4 => a[0].open_orders = 1,
            5 => a[0].observed_ms = now + 1,
            6 => a[1].observed_ms = now - 3001,
            _ => a[1].free_margin = Decimal::ZERO,
        };
        assert!(
            strategy::risk_check(&s, &b, &a, now, 1400, Action::Open).is_err(),
            "risk case {case}"
        );
    }
    assert!(strategy::risk_check(&s, &b, &good, now, 1401, Action::Open).is_err());
    assert!(strategy::risk_check(&s, &b, &good, now, 500, Action::Open).is_err());
    assert!(strategy::risk_check(&s, &b, &good, now, 500, Action::Close).is_err());
}
#[test]
fn emergency_exit_does_not_wait_for_mean_warmup() {
    let now = 4000000;
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.status = Status::Running;
    s.lots.push(Lot {
        entry_net_spread: None,
        id: "owned".into(),
        level: 0,
        units: 1500,
        opened_ms: now - 1000,
        entry_spread: d(16),
    });
    s.positions[0] = Position {
        units: 1500,
        average: d(100),
        ..Default::default()
    };
    s.positions[1] = Position {
        units: -1500,
        average: d(116),
        ..Default::default()
    };
    let mut a = accounts(now);
    a[0].position_units = 1500;
    a[1].position_units = -1500;
    a[0].liquidation_price = Some(d(97));
    let op = strategy::evaluate(&mut s, &books(now, 30), &a, now)
        .unwrap()
        .unwrap();
    assert_eq!(op.action, Action::Close);
    assert!(s.paused && s.stop_after_close);
}

#[tokio::test]
async fn paper_exchange_history_survives_restart_independent_of_strategy_checkpoint() {
    use venue::VenueBackend;
    let now = crate::domain::now_ms();
    let cfg = InventoryConfig::default();
    let path = std::env::temp_dir().join(format!(
        "paper-remote-{}.sqlite",
        Snapshot::new(cfg.clone()).unwrap().instance_id
    ));
    let books = std::sync::Arc::new(std::sync::RwLock::new(books(now, 16)));
    let mut paper = venue::PaperBackend::durable(
        Venue::Lighter,
        cfg.clone(),
        Position::default(),
        books.clone(),
        &path,
    )
    .unwrap();
    let request = OrderRequest {
        id: "accepted-before-crash".into(),
        venue: Venue::Lighter,
        side: Side::Buy,
        units: 1400,
        limit: d(101),
        arrival_mid: None,
        reduce_only: false,
        created_ms: now,
        expires_ms: now + 5000,
        signed_expires_ms: None,
    };
    paper.submit(request.clone()).await.unwrap();
    drop(paper);
    let mut recovered =
        venue::PaperBackend::durable(Venue::Lighter, cfg, Position::default(), books, &path)
            .unwrap();
    let order = recovered.lookup(request.clone()).await.unwrap();
    assert!(order.terminal);
    assert_eq!(order.fills[0].units, 1400);
    assert_eq!(recovered.account().await.unwrap().position_units, 1400);
    assert!(recovered.submit(request).await.is_err());
}

#[tokio::test]
async fn unbound_paper_service_rejects_before_creating_a_ledger() {
    let cfg = InventoryConfig::default();
    let id = Snapshot::new(cfg.clone()).unwrap().instance_id;
    let path = std::env::temp_dir().join(format!("openai-retired-{id}.sqlite"));
    let result = InventoryService::launch(cfg, &path, true, None).await;
    assert!(result.is_err());
    assert!(result.is_err());
    assert!(!path.exists());
    assert!(!path.with_extension("lock").exists());
}

#[test]
fn rollback_resume_requires_fresh_complete_evidence_and_survives_restart() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.status = Status::NeedsAttention;
    s.recovery_after_ms = Some(now);
    s.consecutive_rollbacks = 1;
    s.funding_synced_ms = now;
    let b = books(now, 10);
    let a = accounts(now);
    let baseline: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    for case in 0..9 {
        let mut s = baseline.clone();
        let mut a = a.clone();
        let mut b = b.clone();
        match case {
            0 => s.recovery_after_ms = Some(now + 1),
            1 => s.paused = true,
            2 => s.consecutive_rollbacks = 3,
            3 => a[0].authenticated = false,
            4 => a[1].observed_ms = 0,
            5 => a[0].open_orders = 1,
            6 => b[0].connected = false,
            7 => {
                s.loss_stop = Some(LossStop {
                    at_ms: now,
                    net_pnl: d(-10),
                })
            }
            _ => s.funding_synced_ms = 0,
        }
        assert!(!service::resume_verified_rollback(&mut s, &b, &a, now).unwrap());
        assert_eq!(s.status, Status::NeedsAttention);
    }
    let mut mismatch = a.clone();
    mismatch[0].position_units = 90;
    assert!(service::resume_verified_rollback(&mut s, &b, &mismatch, now).is_err());
    assert!(service::resume_verified_rollback(&mut s, &b, &a, now).unwrap());
    assert_eq!(s.status, Status::Warming);
    assert!(s.recovery_after_ms.is_none());
    assert_eq!(s.consecutive_rollbacks, 1);
}

#[test]
fn terminal_rollback_arms_cooldown_but_third_failure_stays_stopped() {
    let mut s = warmed(4_000_000);
    let op: Operation = serde_json::from_value(serde_json::json!({
        "id":"rollback-test", "action":"open", "level":0,"requested_units":90,"created_ms":1,
        "first":null,"hedge":null,"repair":null,"first_terminal":true,"hedge_terminal":true,
        "repair_terminal":true,"first_filled":90,"hedge_filled":0,"repair_filled":90,
        "first_value":"9000","hedge_value":"0","failed":true
    }))
    .unwrap();
    for n in 1..=3 {
        s.pending = Some(op.clone());
        s.finish_operation(4_000_000).unwrap();
        assert_eq!(s.consecutive_rollbacks, n);
        assert_eq!(
            s.recovery_after_ms,
            if n < 3 { Some(4_030_000) } else { None }
        );
        assert!(s.pending.is_none());
        assert_eq!(s.status, Status::NeedsAttention);
    }
    s.pending = Some(op);
    s.pending.as_mut().unwrap().first_terminal = false;
    assert!(s.finish_operation(4_000_000).is_err());
    assert!(s.pending.is_some());
}

#[test]
fn risk_exit_is_not_blocked_by_completed_rollback_halt() {
    let now = 4_000_000;
    let (mut s, a) = loss_fixture(now);
    s.status = Status::NeedsAttention;
    s.reason = "execution recovered; review before resume".into();
    s.samples.clear();
    s.funding_synced_ms = 0;
    let op = strategy::evaluate(&mut s, &books(now, 27), &a, now)
        .unwrap()
        .unwrap();
    assert_eq!(op.action, Action::Close);
    assert!(s.loss_stop.is_some());
    let (mut blocked, a) = loss_fixture(now);
    blocked.status = Status::NeedsAttention;
    blocked.reason = "invalid authenticated fill evidence".into();
    assert!(
        strategy::evaluate(&mut blocked, &books(now, 27), &a, now)
            .unwrap()
            .is_none()
    );
    assert!(blocked.loss_stop.is_some());
    assert_eq!(blocked.reason, "invalid authenticated fill evidence");
}
#[test]
fn liquidation_exit_preempts_recovered_halt_without_mean() {
    let now = 4_000_000;
    let (mut s, mut a) = loss_fixture(now);
    s.status = Status::NeedsAttention;
    s.reason = "rollback completed; cooling down before verified resume".into();
    s.samples.clear();
    a[0].liquidation_price = Some(d(97));
    assert_eq!(
        strategy::evaluate(&mut s, &books(now, 16), &a, now)
            .unwrap()
            .unwrap()
            .action,
        Action::Close
    );
    assert!(s.loss_stop.is_none());
}
#[test]
fn fresh_explicit_account_verification_only_clears_position_halt() {
    let now = 4_000_000;
    let (mut s, a) = loss_fixture(now);
    s.status = Status::NeedsAttention;
    s.reason = "venue position differs from owned ledger".into();
    let baseline = s.clone();
    let mut bad = a.clone();
    bad[0].position_units = 0;
    assert!(service::clear_verified_position_halt(&mut s, &bad, now).is_err());
    bad = a.clone();
    bad[1].authenticated = false;
    assert!(service::clear_verified_position_halt(&mut s, &bad, now).is_err());
    bad = a.clone();
    bad[0].open_orders = 1;
    assert!(service::clear_verified_position_halt(&mut s, &bad, now).is_err());
    assert!(service::clear_verified_position_halt(&mut s, &a, now).unwrap());
    assert_eq!(s.status, Status::Warming);
    s = baseline;
    s.loss_stop = Some(LossStop {
        at_ms: now,
        net_pnl: d(-10),
    });
    strategy::enforce_loss_limit(&mut s, &books(now, 27), now).unwrap();
    assert!(service::clear_verified_position_halt(&mut s, &a, now).unwrap());
    assert_eq!(s.status, Status::Closing);
    s.status = Status::NeedsAttention;
    s.reason = "invalid authenticated fill evidence".into();
    assert!(!service::clear_verified_position_halt(&mut s, &a, now).unwrap());
}
#[test]
fn market_history_continues_during_halt_without_trading_or_faking_gaps() {
    let now = 4_000_000;
    let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
    s.status = Status::NeedsAttention;
    s.reason = "test halt".into();
    assert!(strategy::observe_while_halted(&mut s, &books(now, 16), now).unwrap());
    assert!(!strategy::observe_while_halted(&mut s, &books(now, 16), now).unwrap());
    assert!(strategy::observe_while_halted(&mut s, &books(now, 16), now + 15000).is_err());
    assert!(
        strategy::observe_while_halted(&mut s, &books(now + 120000, 20), now + 120000).unwrap()
    );
    assert_eq!(s.samples.len(), 2);
    assert_eq!(s.status, Status::NeedsAttention);
    assert!(s.previous_signal.is_none() && s.pending.is_none());
    assert_eq!(
        strategy::sampling_progress(&s, now + 120000).covered_ms,
        30000
    );
}

#[tokio::test]
async fn late_terminal_lookup_can_finish_without_reentering_timeout_halt() {
    let now = crate::domain::now_ms();
    let mut s = warmed(now);
    let b = books(now, 16);
    let req = OrderRequest {
        id: "original-late-first".into(),
        venue: Venue::Entropy,
        side: Side::Sell,
        units: 90,
        limit: d(116),
        arrival_mid: None,
        reduce_only: false,
        created_ms: now - 100000,
        expires_ms: now - 95000,
        signed_expires_ms: None,
    };
    s.pending=Some(serde_json::from_value(serde_json::json!({
        "id":"late-terminal", "action":"open", "level":0,"requested_units":90,"created_ms":now-100000,
        "first":req,"hedge":null,"repair":null,"first_terminal":false,"hedge_terminal":false,
        "repair_terminal":false,"first_filled":0,"hedge_filled":0,"repair_filled":0,
        "first_value":"0","hedge_value":"0","failed":false,"first_venue":"entropy"
    })).unwrap());
    s.status = Status::NeedsAttention;
    s.reason = "order unresolved past execution deadline; reconciliation required".into();
    let workers = [Venue::Lighter, Venue::Entropy].map(|v| {
        let mut backend = venue::PaperBackend::new(
            v,
            s.config.clone(),
            Position::default(),
            std::sync::Arc::new(std::sync::RwLock::new(b.clone())),
        );
        backend.orders.insert(
            "original-late-first".into(),
            OrderResult {
                exchange_created_ms: None,
                terminal: true,
                fills: vec![],
                reason: "verified expired absence".into(),
            },
        );
        venue::AccountWorker::spawn(v, Mode::Paper, true, Box::new(backend)).unwrap()
    });
    let path = std::env::temp_dir().join(format!("late-lookup-{}.sqlite", s.instance_id));
    let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
    execution::recheck_timed_out(&mut s, &mut db, &workers, now)
        .await
        .unwrap();
    assert_ne!(s.status, Status::NeedsAttention);
    assert!(s.pending.as_ref().unwrap().first_terminal);
    execution::advance(&mut s, &mut db, &workers, &b, now + 1)
        .await
        .unwrap();
    assert!(s.pending.is_none());
    assert_eq!(s.positions[0].units, 0);
    assert_eq!(s.positions[1].units, 0);
}

struct PartialCloseVenue(FaultVenue);
impl venue::VenueBackend for PartialCloseVenue {
    fn submit(&mut self, mut r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        assert!(r.reduce_only, "precision recovery must never add exposure");
        if r.venue == Venue::Lighter && r.id.ends_with("-first") {
            r.units -= 5;
        }
        self.0.submit(r)
    }
    fn lookup(&mut self, r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        self.0.lookup(r)
    }
    fn account(&mut self) -> venue::BoxFuture<'_, AccountEvidence> {
        self.0.account()
    }
}
#[tokio::test]
async fn partial_close_aligns_within_original_budget_before_entropy_hedge() {
    use std::sync::{Arc, Mutex};
    for reverse in [false, true] {
    let sign = if reverse { -1 } else { 1 };
    let market = |t, spread| bidir_books(t, spread, reverse);
    let now = crate::domain::now_ms();
    let (mut s, mut a) = loss_fixture(now);
        if reverse { mirror_fixture(&mut s, &mut a); }
    s.close_requested = true;
    let b = market(now, 16);
    s.pending = strategy::evaluate(&mut s, &b, &a, now).unwrap();
    let requested = s.pending.as_ref().unwrap().requested_units;
    let l = Arc::new(Mutex::new(Remote {
        position: s.positions[0].clone(),
        ..Default::default()
    }));
    let e = Arc::new(Mutex::new(Remote {
        position: s.positions[1].clone(),
        ..Default::default()
    }));
    let workers = [(Venue::Lighter, l.clone()), (Venue::Entropy, e.clone())].map(|(v, r)| {
        venue::AccountWorker::spawn(
            v,
            Mode::Paper,
            true,
            Box::new(PartialCloseVenue(FaultVenue {
                prices: if reverse { [d(116), d(100)] } else { [d(100), d(116)] },
                venue: v,
                remote: r,
                unknown_once: false,
                fraction: 1,
            })),
        )
        .unwrap()
    });
    let path = std::env::temp_dir().join(format!("close-align-{}.sqlite", s.instance_id));
    let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
    execution::advance(&mut s, &mut db, &workers, &market(now - 5000, 16), now)
        .await
        .unwrap();
    assert!(s.pending.as_ref().unwrap().first.is_none());
    assert_ne!(s.status, Status::NeedsAttention);
    assert_eq!(l.lock().unwrap().submissions, 0);
    for step in 0..8 {
        if s.pending.is_none() {
            break;
        }
        execution::advance(&mut s, &mut db, &workers, &b, now + step)
            .await
            .unwrap();
    }
    assert!(s.pending.is_none());
    assert_eq!(s.positions[0].units, sign * (10000 - requested));
    assert_eq!(s.positions[1].units, -sign * (10000 - requested));
    assert_eq!(s.paired_units(), 10000 - requested);
    assert_eq!(l.lock().unwrap().submissions, 2);
    assert_eq!(e.lock().unwrap().submissions, 1);
}
}

#[tokio::test]
async fn unsent_normal_exit_is_cancelled_when_profit_disappears() {
    let now = crate::domain::now_ms();
    let (mut s, a) = loss_fixture(now);
    s.previous_exit = Some((now - 15000, true));
    let b = books(now, 8);
    s.pending = strategy::evaluate(&mut s, &b, &a, now).unwrap();
    assert_eq!(s.pending.as_ref().unwrap().action, Action::Close);
    let workers = [Venue::Lighter, Venue::Entropy].map(|v| {
        venue::AccountWorker::spawn(
            v,
            Mode::Paper,
            true,
            Box::new(venue::PaperBackend::new(
                v,
                s.config.clone(),
                s.positions[v.index()].clone(),
                std::sync::Arc::new(std::sync::RwLock::new(b.clone())),
            )),
        )
        .unwrap()
    });
    let path = std::env::temp_dir().join(format!("profit-recheck-{}.sqlite", s.instance_id));
    let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
    execution::advance(&mut s, &mut db, &workers, &books(now, 25), now)
        .await
        .unwrap();
    assert!(s.pending.is_none());
    assert_eq!(s.paired_units(), 10000);
    assert_eq!(s.positions[0].units, 10000);
    assert_eq!(s.positions[1].units, -10000);
}

struct ExpiredCloseHedge(FaultVenue);
impl venue::VenueBackend for ExpiredCloseHedge {
    fn submit(&mut self, r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        assert!(r.reduce_only);
        if r.venue == Venue::Entropy && r.id.ends_with("-hedge") {
            return Box::pin(async move {
                let mut remote = self.0.remote.lock().unwrap();
                assert!(!remote.orders.contains_key(&r.id));
                remote.submissions += 1;
                remote.orders.insert(
                    r.id,
                    OrderResult {
                        exchange_created_ms: None,
                        terminal: true,
                        fills: vec![],
                        reason: "expired; verified no execution".into(),
                    },
                );
                anyhow::bail!("fixture timeout after send")
            });
        }
        self.0.submit(r)
    }
    fn lookup(&mut self, r: OrderRequest) -> venue::BoxFuture<'_, OrderResult> {
        self.0.lookup(r)
    }
    fn account(&mut self) -> venue::BoxFuture<'_, AccountEvidence> {
        self.0.account()
    }
}
#[tokio::test]
async fn expired_close_hedge_repairs_then_resumes_or_continues_latched_loss_exit() {
    use std::sync::{Arc, Mutex};
    for reverse in [false, true] {
    let sign = if reverse { -1 } else { 1 };
    let market = |t, spread| bidir_books(t, spread, reverse);
    for stop in [false, true] {
        let now = crate::domain::now_ms();
        let (mut s, mut a) = loss_fixture(now);
        if reverse { mirror_fixture(&mut s, &mut a); }
        s.previous_exit = Some((now - 15000, true));
        s.funding_synced_ms = now;
        s.pending = strategy::evaluate(&mut s, &market(now, 8), &a, now).unwrap();
        let qty = s.pending.as_ref().unwrap().requested_units;
        let l = Arc::new(Mutex::new(Remote {
            position: s.positions[0].clone(),
            ..Default::default()
        }));
        let e = Arc::new(Mutex::new(Remote {
            position: s.positions[1].clone(),
            ..Default::default()
        }));
        let workers = [(Venue::Lighter, l.clone()), (Venue::Entropy, e.clone())].map(|(v, r)| {
            venue::AccountWorker::spawn(
                v,
                Mode::Paper,
                true,
                Box::new(ExpiredCloseHedge(FaultVenue {
                    prices: if reverse { [d(116), d(100)] } else { [d(100), d(116)] },
                    venue: v,
                    remote: r,
                    unknown_once: false,
                    fraction: 1,
                })),
            )
            .unwrap()
        });
        let path = std::env::temp_dir().join(format!("close-recovery-{}.sqlite", s.instance_id));
        let (mut db, _) = store::Store::open(&path, &s.config).unwrap();
        for step in 0..10 {
            if s.pending.is_none() {
                break;
            }
            let b = market(now + step, if stop && step > 0 { 27 } else { 8 });
            execution::advance(&mut s, &mut db, &workers, &b, now + step)
                .await
                .unwrap();
        }
        assert!(s.pending.is_none());
        assert_eq!(s.positions[0].units, sign * (10000 - qty));
        assert_eq!(s.positions[1].units, -sign * (10000 - qty));
        assert_eq!(l.lock().unwrap().submissions, 1);
        assert_eq!(e.lock().unwrap().submissions, 2);
        assert_eq!(e.lock().unwrap().queries, 1);
        let mut fresh = accounts(now + 30010);
        fresh[0].position_units = s.positions[0].units;
        fresh[1].position_units = s.positions[1].units;
        if stop {
            assert!(s.loss_stop.is_some());
            assert_eq!(s.status, Status::Closing);
            assert!(s.close_requested);
            assert!(
                !service::resume_verified_rollback(
                    &mut s,
                    &market(now + 30010, 27),
                    &fresh,
                    now + 30010
                )
                .unwrap()
            );
            assert_eq!(
                strategy::evaluate(&mut s, &market(now + 30010, 27), &fresh, now + 30010)
                    .unwrap()
                    .unwrap()
                    .action,
                Action::Close
            );
        } else {
            assert!(s.recovery_after_ms.is_some());
            assert_eq!(s.status, Status::NeedsAttention);
            let mut restored: Snapshot =
                serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert!(
                !service::resume_verified_rollback(&mut restored, &market(now, 8), &a, now).unwrap()
            );
            assert!(
                service::resume_verified_rollback(
                    &mut restored,
                    &market(now + 30010, 8),
                    &fresh,
                    now + 30010
                )
                .unwrap()
            );
            assert!(restored.recovery_after_ms.is_none());
            assert_eq!(restored.status, Status::Warming);
        }
    }
}
}

#[test]
fn bounded_entry_budget_survives_restart_and_failed_attempt() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.status = Status::Stopped;
    s.stop_requested = true;
    s.exit_batch_active = true;
    s.funding_synced_ms = now;
    let a = accounts(now);
    service::start_one_entry(&mut s, Some(&a), &books(now, 20), now).unwrap();
    assert!(!s.exit_batch_active);
    assert_eq!(s.entry_attempts_remaining, Some(1));
    s.status = Status::Running;
    assert!(
        strategy::evaluate(&mut s, &books(now, 20), &a, now)
            .unwrap()
            .is_none()
    );
    let t = now + 15000;
    let a = accounts(t);
    assert_eq!(
        strategy::evaluate(&mut s, &books(t, 20), &a, t)
            .unwrap()
            .unwrap()
            .action,
        Action::Open
    );
    assert_eq!(s.entry_attempts_remaining, Some(0));
    // An unfilled/failed attempt cannot buy a second group after restart.
    let mut restored: Snapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    let t = t + 15000;
    assert!(
        strategy::evaluate(&mut restored, &books(t, 20), &accounts(t), t)
            .unwrap()
            .is_none()
    );
    assert_eq!(restored.entry_attempts_remaining, Some(0));
}
#[test]
fn bounded_entry_rejects_stale_or_locked_without_mutation() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.status = Status::Stopped;
    s.stop_requested = true;
    s.exit_batch_active = true;
    s.funding_synced_ms = now;
    let mut a = accounts(now);
    a[0].observed_ms = 0;
    let original = serde_json::to_string(&s).unwrap();
    assert!(service::start_one_entry(&mut s, Some(&a), &books(now, 20), now).is_err());
    assert_eq!(serde_json::to_string(&s).unwrap(), original);
    s.loss_stop = Some(LossStop {
        at_ms: now,
        net_pnl: d(-10),
    });
    assert!(service::start_one_entry(&mut s, Some(&accounts(now)), &books(now, 20), now).is_err());
    assert!(s.exit_batch_active);
}
#[test]
fn old_ledger_defaults_to_unlimited_without_erasing_test_budget() {
    let s = warmed(4_000_000);
    let mut value = serde_json::to_value(&s).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("entry_attempts_remaining");
    let old: Snapshot = serde_json::from_value(value).unwrap();
    assert_eq!(old.entry_attempts_remaining, None);
}
#[test]
fn protected_depth_gate_retains_budget_until_both_legs_executable() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.entry_attempts_remaining = Some(1);
    let mut thin = books(now, 20);
    thin[1].bids = vec![
        Level {
            price: d(120),
            units: 10,
        },
        Level {
            price: d(119),
            units: 100000,
        },
    ];
    assert!(
        strategy::evaluate(&mut s, &thin, &accounts(now), now)
            .unwrap()
            .is_none()
    );
    let t = now + 15000;
    thin.iter_mut().for_each(|b| b.received_ms = t);
    assert!(
        strategy::evaluate(&mut s, &thin, &accounts(t), t)
            .unwrap()
            .is_none()
    );
    assert_eq!(s.entry_attempts_remaining, Some(1));
    assert_eq!(s.sequence, 0);
    assert!(s.reason.contains("protected execution price"));
    let t = t + 15000;
    assert!(
        strategy::evaluate(&mut s, &books(t, 20), &accounts(t), t)
            .unwrap()
            .is_some()
    );
    assert_eq!(s.entry_attempts_remaining, Some(0));
}
#[test]
fn protected_limits_reject_unprofitable_limit_pair_before_reservation() {
    let now = 4_000_000;
    let mut s = warmed(now);
    s.config.entry_offset = d(0);
    s.entry_attempts_remaining = Some(1);
    assert!(
        strategy::evaluate(&mut s, &books(now, 10), &accounts(now), now)
            .unwrap()
            .is_none()
    );
    let t = now + 15000;
    assert!(
        strategy::evaluate(&mut s, &books(t, 10), &accounts(t), t)
            .unwrap()
            .is_none()
    );
    assert_eq!(s.entry_attempts_remaining, Some(1));
    assert_eq!(s.sequence, 0);
    assert!(s.reason.contains("protected entry limits"));
}

include!("bidirectional_tests.rs");
include!("group_exit_tests.rs");
include!("decision_frequency_tests.rs");
include!("entry_confirmation_tests.rs");
include!("independent_entry_tests.rs");
include!("core_parity_tests.rs");
include!("rules_upgrade_tests.rs");

#[test]
fn five_minute_mean_excludes_old_prices_and_requires_real_coverage() {
    let now=4_005_000;let mut s=warmed(now);s.config.mean_window_ms=300_000;
    s.config.decision_ms=Some(1000);s.config.entry_confirmation_ms=Some(5000);
    s.config.validate().unwrap();
    for (t,m) in &mut s.samples {if *t<now-300_000 {*m=d(9999);}}
    assert_eq!(strategy::reference_mean_for(&s,now,Direction::LighterLong),Some(d(10)));
    assert_eq!(strategy::reference_mean_for(&s,now,Direction::LighterShort),Some(d(-10)));
    assert_eq!(strategy::sampling_progress(&s,now).required_ms,285_000);
    assert!(strategy::sampling_progress(&s,now).ready);
    s.samples.retain(|(t,_)|*t>=now-120_000);
    assert!(!strategy::sampling_progress(&s,now).ready);
    assert_eq!(s.config.confirmation_window_ms(),30000);
}

include!("batch_exit_tests.rs");

include!("entry_mean_tests.rs");

include!("anth_tests.rs");
