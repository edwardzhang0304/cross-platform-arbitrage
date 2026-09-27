fn seed_entry_quotes(s: &mut Snapshot, now: u64, spread: i64, reverse: bool) {
    s.entry_mean.points = (1..=s.config.mean_window_ms/1000).rev().map(|i| {
        let t = now - i*1000;
        charts::QuotePoint::from_books(&bidir_books(t, spread, reverse), t, 1500)
    }).collect();
    s.entry_mean.initialized = true;
}

fn wide_entry_books(now: u64) -> [Book; 2] {
    let mut b = books(now, 8);
    b[0].asks[0].price += d(2);
    b[1].asks[0].price += d(2);
    b
}

#[test]
fn bid_ask_mean_matches_raw_chart_points_and_keeps_directions_independent() {
    let now = 4_000_000;
    let mut history = entry_mean::EntryMean::default();
    for i in 0..=300 {
        let t = now - 300_000 + i*1000;
        let b = wide_entry_books(t);
        assert!(history.observe(&b, t, 1500, 300_000));
        assert!(!history.observe(&books(t+100, 99), t+100, 1500, 300_000));
    }
    assert!(history.progress(now, 300_000).ready);
    assert_eq!(history.points.len(), 301);
    assert_eq!(history.mean(now, 300_000, Direction::LighterLong), Some(d(6)));
    assert_eq!(history.mean(now, 300_000, Direction::LighterShort), Some(d(-10)));
    let green_sum: Decimal = history.points.iter().map(|p| p.entropy_bid.unwrap()-p.lighter_ask.unwrap()).sum();
    assert_eq!(history.mean(now, 300_000, Direction::LighterLong), Some(green_sum/d(301)));
    assert!(!history.observe(&books(now-1000, 99), now-1000, 1500, 300_000));
    assert!(history.observe(&books(now+1000, 3), now+1000, 1500, 300_000));
    assert_eq!(history.points.front().unwrap().time_ms, now-299_000);
    assert_eq!(history.mean(now+1000, 300_000, Direction::LighterLong), Some(d(1803)/d(301)));
}

#[test]
fn entry_mean_rejects_stale_gaps_and_does_not_invent_warmup_or_a_high_baseline() {
    let now = 4_000_000;
    let (mut s, _) = accumulation_fixture(now, false);
    s.config.mean_window_ms = 300_000;
    s.entry_mean = Default::default();
    s.mean_initialized = true;
    s.continuity_mean = Some((now, d(99)));
    for i in 0..285 {
        let t = now+i*1000;
        strategy::observe_entry_mean(&mut s, &wide_entry_books(t), t);
    }
    assert!(!strategy::entry_sampling_progress(&s, now+284_000).ready);
    strategy::observe_entry_mean(&mut s, &wide_entry_books(now+285_000), now+285_000);
    assert!(strategy::entry_sampling_progress(&s, now+285_000).ready);
    assert_eq!(strategy::entry_reference_mean_for(&s, now+285_000, Direction::LighterLong), Some(d(6)));
    let t = now+300_000;
    strategy::observe_entry_mean(&mut s, &wide_entry_books(now), t);
    assert!(s.entry_mean.points.back().unwrap().entry_spread(Direction::LighterLong).is_none());
    assert!(!strategy::entry_sampling_progress(&s, t).ready);
    strategy::observe_entry_mean(&mut s, &wide_entry_books(t+1000), t+1000);
    assert!(strategy::entry_sampling_progress(&s, t+1000).ready);
    // After the whole window expires, one fresh quote cannot recreate history.
    strategy::observe_entry_mean(&mut s, &wide_entry_books(t+301_000), t+301_000);
    assert!(!strategy::entry_sampling_progress(&s, t+301_000).ready);
}

#[test]
fn legacy_snapshot_keeps_inventory_but_does_not_reuse_midpoints_for_new_entries() {
    let now = 4_000_000;
    let (s, _) = accumulation_fixture(now, false);
    let mut old = serde_json::to_value(&s).unwrap();
    old.as_object_mut().unwrap().remove("entry_mean");
    let restored: Snapshot = serde_json::from_value(old).unwrap();
    assert!(restored.entry_mean.points.is_empty());
    assert!(!strategy::entry_sampling_progress(&restored, now).ready);
    assert_eq!(strategy::entry_reference_mean_for(&restored, now, restored.direction), None);
    for field in ["lots", "fills", "positions", "anchor", "samples", "opened_groups", "closed_groups", "time_adds_used"] {
        assert_eq!(serde_json::to_value(&restored).unwrap()[field], serde_json::to_value(&s).unwrap()[field], "{field}");
    }
}

#[test]
fn existing_groups_close_during_new_opening_ma_warmup() {
    for reverse in [false, true] {
        let now = 4_000_000;
        let (mut s, mut a) = accumulation_fixture(now, reverse);
        s.entry_mean = Default::default();
        s.samples.clear(); s.continuity_mean=None; s.mean_initialized=false;
        assert!(strategy::evaluate(&mut s, &bidir_books(now, 4, reverse), &a, now).unwrap().is_none());
        for x in &mut a { x.observed_ms=now+1000; }
        let op=strategy::evaluate(&mut s, &bidir_books(now+1000,4,reverse), &a, now+1000).unwrap().unwrap();
        assert_eq!(op.action, Action::Close);
        assert!(!strategy::entry_sampling_progress(&s, now+1000).ready);
        s.pending=Some(op);
        assert!(strategy::normal_exit_eligible(&s, &bidir_books(now+1000,4,reverse), now+1000).unwrap());
    }
}

#[test]
fn opening_uses_bid_ask_mean_below_old_midpoint_but_still_confirms_five_seconds() {
    for reverse in [false, true] {
        let now=4_000_000;
        let (mut s, _) = accumulation_fixture(now,reverse);
        s.config.mean_window_ms=300_000;s.config.entry_offset=Decimal::ZERO;
        s.config.entry_confirmation_ms=Some(5000);
        s.positions=Default::default();s.lots.clear();s.anchor=None;s.armed.fill(true);
        s.last_open_completed=None;
        seed_entry_quotes(&mut s,now,6,reverse);
        for (_,m) in &mut s.samples { *m=d(8)*Decimal::from(s.direction.sign()); }
        s.continuity_mean=Some((now,d(20)*Decimal::from(s.direction.sign())));
        for offset in 0..=5 {
            let t=now+offset*1000;
            let op=strategy::evaluate(&mut s,&bidir_books(t,7,reverse),&accounts(t),t).unwrap();
            if offset<5 { assert!(op.is_none()); }
            else {
                let op=op.unwrap();assert_eq!(op.action,Action::Open);
                assert!(op.min_entry_spread.unwrap()<d(7));
                assert_eq!(s.direction,if reverse {Direction::LighterShort}else{Direction::LighterLong});
            }
        }
        assert_eq!(s.config.entry_threshold(Decimal::ZERO),d(5));
    }
}

#[test]
fn bid_ask_history_survives_restart_while_confirmation_clocks_reset() {
    let now=4_000_000;
    let (mut s, _)=accumulation_fixture(now,false);
    s.entry_confirmations[0]=Some((now,1));s.time_entry_confirmations[0]=Some((now,20));
    let folder=std::env::temp_dir().join(format!("bid-ask-ma-{}",s.instance_id));
    let path=folder.join("test.sqlite");
    let(mut db,_)=store::Store::open(&path,&s.config).unwrap();
    db.commit(&s,now,"test").unwrap();drop(db);
    let(db,restored)=store::Store::open(&path,&s.config).unwrap();
    assert_eq!(restored.entry_mean.points,s.entry_mean.points);
    assert!(restored.entry_mean.initialized);
    assert_eq!(restored.entry_confirmations,[None;2]);
    assert_eq!(restored.time_entry_confirmations,[None;2]);
    drop(db);std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn first_order_submission_rechecks_bid_ask_mean_not_legacy_midpoint() {
    use std::sync::{Arc,RwLock,atomic::AtomicU64};
    let now=4_000_000;
    let (mut s,_)=accumulation_fixture(now,false);
    s.config.mean_window_ms=300_000;s.config.entry_offset=Decimal::ZERO;
    s.positions=Default::default();s.lots.clear();s.anchor=None;s.armed.fill(true);
    seed_entry_quotes(&mut s,now,5,false);
    for (_,mean) in &mut s.samples { *mean=d(99); }
    assert!(strategy::evaluate(&mut s,&books(now,7),&accounts(now),now).unwrap().is_none());
    let t=now+1000;let b=books(t,7);
    s.pending=strategy::evaluate(&mut s,&b,&accounts(t),t).unwrap();
    assert_eq!(s.pending.as_ref().unwrap().action,Action::Open);
    let qty=s.pending.as_ref().unwrap().requested_units;
    let shared=Arc::new(RwLock::new(b.clone()));let clock=Arc::new(AtomicU64::new(t));
    let workers=[Venue::Lighter,Venue::Entropy].map(|v|venue::AccountWorker::spawn_replay(v,
        venue::PaperBackend::new(v,s.config.clone(),s.positions[v.index()].clone(),shared.clone())
            .with_logical_clock(clock.clone()),clock.clone()).unwrap());
    let (mut db,_)=store::Store::offline_replay(&s.config).unwrap();
    for _ in 0..8 {
        if s.pending.is_none(){break;}
        execution::advance(&mut s,&mut db,&workers,&b,t).await.unwrap();
    }
    assert!(s.pending.is_none());assert_eq!(s.lots.len(),1);
    assert_eq!(s.positions[0].units,qty);assert_eq!(s.positions[1].units,-qty);
}

#[test]
fn low_bid_ask_mean_does_not_bypass_five_dollar_entry_floor() {
    for reverse in [false,true] {
        let now=4_000_000;
        let (mut s,_)=accumulation_fixture(now,reverse);
        s.config.entry_offset=Decimal::ZERO;s.config.mean_window_ms=300_000;
        s.positions=Default::default();s.lots.clear();s.anchor=None;s.armed.fill(true);
        s.last_open_completed=None;
        seed_entry_quotes(&mut s,now,0,reverse);
        for offset in 0..=6 {
            let t=now+offset*1000;
            assert!(strategy::evaluate(&mut s,&bidir_books(t,4,reverse),&accounts(t),t).unwrap().is_none());
            assert_eq!(s.entry_confirmations,[None;2]);
        }
    }
}
