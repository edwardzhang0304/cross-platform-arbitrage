//! Offline execution and risk fixtures. Compiled only for unit tests.
//! The caller freezes one book frame and one logical clock for both variants.
use super::{store::Store, venue::{AccountWorker, PaperBackend}, *};
use anyhow::{Result, ensure};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::{path::Path, sync::{Arc, RwLock, atomic::AtomicU64}};

pub struct Variant {
    protection: Option<(liquidation::IsolationSpec,Arc<RwLock<[liquidation::Mark;2]>>)>,
    pub state: Snapshot,
    pub store: Store,
    workers: [AccountWorker; 2],
    pub warning: String,
    pub accounts: Option<[AccountEvidence; 2]>,
    last_lookup: u64,
}
impl Variant {
    /// Reuse the paired execution/risk kernel with an isolated in-memory paper
    /// backend. The caller advances recorded time only after all variants finish.
    pub fn offline_replay(config: InventoryConfig, started: u64,
        books: Arc<RwLock<[Book;2]>>, clock: Arc<AtomicU64>,
        protection: (liquidation::IsolationSpec,Arc<RwLock<[liquidation::Mark;2]>>)) -> Result<Self> {
        config.validate()?;
        protection.0.validate(&config)?;
        let (mut store, mut state) = Store::offline_replay(&config)?;
        state.created_ms = started;
        state.status = Status::Warming;
        state.resume_after_recovery = true;
        store.commit(&state, started, "offline_equal_capital_start")?;
        let workers = [Venue::Lighter, Venue::Entropy].map(|venue| {
            let backend = PaperBackend::new(venue, config.clone(), Position::default(), books.clone())
                .with_logical_clock(clock.clone()).with_isolation(protection.0.clone(), protection.1.clone())?;
            AccountWorker::spawn_replay(venue, backend, clock.clone())
        });
        let [l,e] = workers;
        Ok(Self {state, store, workers:[l?,e?], warning:String::new(), accounts:None,
            last_lookup:0, protection:Some(protection)})
    }
    pub fn open(path: &Path, config: InventoryConfig, seed: &Snapshot, started: u64,
        books: Arc<RwLock<[Book; 2]>>, clock: Arc<AtomicU64>) -> Result<Self> {
        Self::open_protected(path,config,seed,started,books,clock,None)
    }
    pub fn open_protected(path: &Path, config: InventoryConfig, seed: &Snapshot, started: u64,
        books: Arc<RwLock<[Book;2]>>, clock: Arc<AtomicU64>,
        protection:Option<(liquidation::IsolationSpec,Arc<RwLock<[liquidation::Mark;2]>>)>) -> Result<Self> {
        ensure!(config.mode == Mode::Paper, "comparison cannot use live workers");
        if let Some((spec,_))=&protection {spec.validate(&config)?;}
        let fresh=!path.exists();
        let (mut store, mut state)=Store::open(path, &config)?;
        if fresh {
            state.created_ms=started;
            state.samples=seed.samples.iter().filter(|(t,_)|*t<=started && started-*t<=config.mean_window_ms).cloned().collect();
            state.entry_mean=seed.entry_mean.clone();
            state.entry_mean.points.retain(|p|p.time_ms<=started && started-p.time_ms<=config.mean_window_ms);
            state.mean_initialized=seed.mean_initialized;
            state.continuity_mean=seed.continuity_mean.filter(|(t,_)|*t<=started);
            state.last_sample_ms=state.samples.back().map_or(0,|x|x.0);
            state.status=Status::Warming;
            state.resume_after_recovery=true;
            store.commit(&state, started, "comparison_fresh_equal_capital")?;
        }
        let workers=[Venue::Lighter, Venue::Entropy].map(|v| {
            let backend=PaperBackend::durable(v, config.clone(),state.positions[v.index()].clone(),books.clone(),
                &path.with_file_name(format!("{}-{:?}-remote.sqlite", state.instance_id,v)))?
                .with_logical_clock(clock.clone());
            let backend=if let Some((spec,marks))=&protection {backend.with_isolation(spec.clone(),marks.clone())?}else{backend};
            AccountWorker::spawn(v,Mode::Paper,true,Box::new(backend))
        });
        let [l,e]=workers;
        Ok(Self{state,store,workers:[l?,e?],warning:String::new(),accounts:None,last_lookup:0,protection})
    }

    pub async fn tick(&mut self, books: &[Book;2], now:u64) -> Result<()> {
        let result=self.step(books,now).await;
        match result {
            Ok(())=>self.warning.clear(),
            Err(error)=>{
                let message=format!("{error:#}");
                if message.contains("persistence") { return Err(error); }
                let hard=self.state.pending.is_some() || ["position","unpaired","unowned","worker stopped","worker unavailable"]
                    .iter().any(|x|message.contains(x));
                if hard {
                    self.state.status=Status::NeedsAttention;
                    self.state.recovery_after_ms=None;
                    self.state.reason=message.clone();
                }
                self.warning=message;
            }
        }
        self.store.commit(&self.state,now,"sample")?;
        Ok(())
    }

    async fn step(&mut self, books:&[Book;2], now:u64)->Result<()> {
        let s=&mut self.state;
        if self.protection.is_some() && liquidation::protect(s,&mut self.store,&self.workers,books,now).await? {
            let (l,e)=tokio::join!(self.workers[0].reconcile_account(),self.workers[1].reconcile_account());
            self.accounts=Some([l?,e?]);
            if s.positions.iter().all(|p|p.units==0) || books.iter().all(|b|b.validate(now,s.config.book_max_age_ms).is_ok()) {
                if let Ok(net)=s.total_pnl(books) {s.peak_pnl=s.peak_pnl.max(net);s.max_drawdown=s.max_drawdown.max(s.peak_pnl-net);}
            }
            return Ok(());
        }
        if books.iter().all(|b|b.validate(now,s.config.book_max_age_ms).is_ok()) {
            if let Ok(net)=s.total_pnl(books) {
                s.peak_pnl=s.peak_pnl.max(net);
                s.max_drawdown=s.max_drawdown.max(s.peak_pnl-net);
            }
        }
        if s.pending.is_some() || matches!(s.status,Status::NeedsAttention|Status::Stopped|Status::Recovering) {
            strategy::observe_while_halted(s,books,now).unwrap_or(false);
        }
        strategy::enforce_loss_limit(s,books,now).unwrap_or(false);
        // Pending operations must advance only once per recorded shared frame.
        if s.pending.is_some() && s.status==Status::NeedsAttention {
            if s.reason.starts_with("order unresolved past execution deadline") && now.saturating_sub(self.last_lookup)>=30_000 {
                self.last_lookup=now;
                execution::recheck_timed_out(s,&mut self.store,&self.workers,now).await?;
            }
            return Ok(());
        }
        if s.pending.is_some() {
            execution::advance(s,&mut self.store,&self.workers,books,now).await?;
            return Ok(());
        }
        let (l,e)=tokio::join!(self.workers[0].reconcile_account(),self.workers[1].reconcile_account());
        let accounts=[l?,e?];
        s.assert_reconciled(&accounts)?;
        service::clear_verified_position_halt(s,&accounts,now)?;
        service::resume_verified_rollback(s,books,&accounts,now)?;
        if s.status==Status::Recovering {
            ensure!(accounts.iter().all(|a|a.open_orders==0),"unowned orders at recovery");
            s.status=if s.stop_requested {Status::Stopped} else if s.paused {Status::PausedEntries} else {Status::Warming};
        }
        self.accounts=Some(accounts.clone());
        // Funding is deliberately excluded from BOTH paper variants, matching PaperBackend.
        s.funding_synced_ms=now;
        // New timed-entry experiments must not build confirmation while marks
        // are invalid. Reducing signals remain available independently.
        let entry_blocked=s.config.entry_confirmation_ms.is_some() && self.protection.as_ref()
            .is_some_and(|(spec,marks)| !marks.read().unwrap().iter().all(|m|m.valid(now,spec.mark_max_age_ms).is_some()));
        let paused=s.paused;
        if entry_blocked {s.paused=true;strategy::clear_entry_confirmation(s);}
        let proposal=strategy::evaluate(s,books,&accounts,now);
        if entry_blocked && !s.close_requested && s.loss_stop.is_none() && !s.stop_after_close {
            s.paused=paused;
        }
        if entry_blocked {strategy::clear_entry_confirmation(s);}
        if let Some(op)=proposal? {
            if op.action==Action::Open {
                if let Some((spec,marks))=&self.protection {
                    ensure!(marks.read().unwrap().iter().all(|m|m.valid(now,spec.mark_max_age_ms).is_some()),"isolated paper mark disconnected or stale");
                }
            }
            s.pending=Some(op);
            self.store.commit(s,now,"operation_reserved")?;
        }
        Ok(())
    }

    pub fn summary(&self, books:&[Book;2], now:u64)->Value {
        let s=&self.state;
        let valid=books.iter().all(|b|b.validate(now,s.config.book_max_age_ms).is_ok());
        let realized=s.positions.iter().map(|p|p.realized-p.fees+p.funding).sum::<Decimal>();
        let flat=s.positions.iter().all(|p|p.units==0);
        let unrealized=if flat {Some(Decimal::ZERO)}else if valid {Some(s.positions.iter().zip(books).map(|(p,b)|p.unrealized(b.mid().unwrap())).sum::<Decimal>())}else{None};
        let uncertain=s.liquidation_protection.as_ref().is_some_and(|p|p.orders.iter().flatten().any(|o|!o.terminal));
        let close_net=if uncertain {None} else if flat {Some(realized)} else {valid.then(||s.total_pnl(books).ok()).flatten()};
        let max_age=s.lots.iter().map(|l|now.saturating_sub(l.opened_ms)).max().unwrap_or(0);
        json!({"instance_id":s.instance_id,"policy":s.config.exit_policy,"group_take_profit":s.config.group_take_profit,
            "liquidation_protection_enabled":self.protection.is_some(),"liquidation_protection":s.liquidation_protection,
            "isolated_liquidation_model":self.protection.as_ref().map(|p|&p.0),
            "shared_exit_conditions":s.config.shared_exit_conditions,
            "decision_ms":s.config.decision_interval_ms(),"mean_sample_ms":s.config.sample_ms,
            "entry_confirmation_ms":s.config.entry_confirmation_ms,
            "accumulation":s.config.accumulation,"time_adds_used":s.time_adds_used,"last_open_completed":s.last_open_completed,
            "entry_confirmations":s.entry_confirmations,
            "confirmation_window_ms":s.config.confirmation_window_ms(),
            "decision_observation":s.decision_observation,
            "capital_total":s.config.paper_capital_per_venue*Decimal::TWO,"status":s.status,"warning":self.warning,
            "opened_groups":s.opened_groups,"closed_groups":s.closed_groups,"remaining_groups":s.lots.len(),
            "direction":s.direction,"lots":s.lots,"pending":s.pending,"positions":s.positions,
            "realized_net":realized,"unrealized_mid":unrealized,"estimated_net_if_closed_now":close_net,
            "return_on_200u_pct":close_net.map(|n|n/(s.config.paper_capital_per_venue*Decimal::TWO)*Decimal::from(100)),
            "fees":s.cumulative_fees(),"execution_cost_already_in_prices":s.execution_cost,
            "max_drawdown":s.max_drawdown,"oldest_open_group_ms":max_age,"loss_stop":s.loss_stop,
            "sampling":strategy::sampling_progress(s,now),"mean":strategy::reference_mean(s,now),
            "fill_count":s.fills.len(),"account_evidence":self.accounts})
    }
}
