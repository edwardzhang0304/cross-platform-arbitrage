use super::{
    store::Store,
    venue::{AccountWorker, VenueBackend},
    *,
};
use crate::crossvenue_a::{
    MarketDataEvent, MarketStreamConfig, MarketVenue, PriceSource, spawn_lighter_market_stream,
    spawn_trade_fast_market_stream,
};
use crate::lighter::{LighterClient, LighterEnvironment};
use anyhow::{Context, Result, ensure};
use rust_decimal::{Decimal};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::Path,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize)]
pub struct InventoryView {
    pub entry_first_venue: Venue,
    pub close_first_venue: Venue,
    pub snapshot: Snapshot,
    pub books: [Book; 2],
    /// Read-only four-hour quote history for the same chart used by paper.
    pub chart_points: VecDeque<charts::QuotePoint>,
    pub marks: [liquidation::Mark;2],
    pub accounts: Option<[AccountEvidence; 2]>,
    pub mean: Option<Decimal>,
    pub directional_means: [Option<Decimal>; 2],
    pub net_pnl: Option<Decimal>,
    pub estimated_exit_net: Option<Decimal>,
    pub profit_accounting: accounting::NetProfitAccounting,
    pub cumulative_fees: Decimal,
    pub cumulative_execution_cost: Decimal,
    pub execution_cost_started_ms: Option<u64>,
    pub execution_cost_tracked_fills: u64,
    pub execution_cost_untracked_fills: u64,
    pub submission_enabled: bool,
    pub funding_note: String,
    pub transient_warning: String,
    pub sampling: strategy::SamplingProgress,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Start,
    StartOneEntry,
    StartLiveStrategy,
    Pause,
    Stop,
    CloseAll,
    Reconcile,
    Shutdown,
    SetEntryOffset { entry_offset: Decimal },
    SetMaxLoss { max_loss_usdc: Decimal },
    SetMaxTimeAdds { max_time_adds: usize },
    SetDirectionPolicy { direction_policy: DirectionPolicy },
}
struct Command {
    id: String,
    control: Control,
    reply: oneshot::Sender<Result<()>>,
}
#[derive(Clone)]
pub struct InventoryService {
    view: Arc<RwLock<InventoryView>>,
    commands: mpsc::Sender<Command>,
}
impl InventoryService {
    pub async fn launch(
        config: InventoryConfig,
        path: &Path,
        process_dry_run: bool,
        live: Option<[Box<dyn VenueBackend>; 2]>,
    ) -> Result<Self> {
        Self::launch_inner(config, path, process_dry_run, live, false).await
    }
    pub async fn launch_with_live_limit_upgrade(
        config: InventoryConfig, path: &Path, process_dry_run: bool,
        live: Option<[Box<dyn VenueBackend>; 2]>,
    ) -> Result<Self> {
        Self::launch_inner(config, path, process_dry_run, live, true).await
    }
    async fn launch_inner(
        config: InventoryConfig, path: &Path, process_dry_run: bool,
        live: Option<[Box<dyn VenueBackend>; 2]>, upgrade: bool,
    ) -> Result<Self> {
        config.validate()?;
        ensure!((config.mode == Mode::Live && cfg!(feature="openai-inventory-live"))
            || (config.mode == Mode::Paper && cfg!(feature="paper-runtime") && process_dry_run && live.is_none()),
            "build, execution mode and backend do not match");
        if config.mode == Mode::Paper {
            crate::profiles::ProfileId::new(config.market, Mode::Paper).validate(&config)?;
        }
        let pair = config.market;
        ensure!(
            config.mode != Mode::Live || (!process_dry_run && live.is_some()),
            "live workers unavailable or process dry-run enabled"
        );
        let (mut store, mut state) = if upgrade { Store::open_with_live_limit_upgrade(path, &config)? }
            else { Store::open(path, &config)? };
        quarantine_live_start(&mut state);
        let client = LighterClient::official(LighterEnvironment::Robinhood)?;
        let market = client.market_by_symbol(pair.lighter_symbol()).await?;
        pair.validate_lighter(&market)?;
        ensure!(
            market.is_active_perp() && market.market_id == pair.lighter_market_id() && market.effective_size_decimals() == pair.quantity_decimals(),
            "Lighter OPENAI metadata drift"
        );
        let meta = crate::hyperliquid::fetch_xyz_market_snapshot_cached("mainnet", "io", 0).await?;
        let asset = meta
            .meta
            .universe
            .iter()
            .find(|x| x.name == pair.entropy_symbol())
            .context("Entropy OAI missing")?;
        ensure!(
            asset.sz_decimals == 3
                && asset.margin_mode.as_deref() == Some(pair.entropy_margin_mode())
                && asset.max_leverage.is_some_and(|v| v >= config.leverage),
            "Entropy OAI precision/margin metadata drift"
        );
        let books = Arc::new(RwLock::new([Book::default(), Book::default()]));
        let marks=Arc::new(RwLock::new([liquidation::Mark::default(),liquidation::Mark::default()]));
        let receivers = [
            spawn_lighter_market_stream(
                client.endpoints().clone(),
                market.market_id,
                PriceSource::Unknown,
                MarketStreamConfig::default(),
            )?,
            spawn_trade_fast_market_stream(
                "mainnet",
                pair.entropy_symbol().into(),
                PriceSource::Unknown,
                MarketStreamConfig::default(),
            )?,
        ];
        let mut feed_tasks = vec![];
        for mut rx in receivers {
            let shared = books.clone();
            let shared_marks=marks.clone();
            feed_tasks.push(tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    match event {
                        MarketDataEvent::Context {venue,context} => {
                            let i=if venue==MarketVenue::Lighter{0}else{1};
                            shared_marks.write().unwrap()[i]=liquidation::Mark {
                                price:context.mark_price.and_then(|p|Decimal::from_str(&p.to_string()).ok()).filter(|p|*p>Decimal::ZERO),
                                received_ms:context.received_timestamp_ms,connected:true,
                            };
                        }
                        MarketDataEvent::Book { venue, book } => {
                            let levels =
                                |a: Vec<crate::crossvenue_a::PriceLevel>| -> Result<Vec<Level>> {
                                    a.into_iter()
                                        .map(|l| {
                                            let price = Decimal::from_str(&l.price.to_string())?;
                                            let units = pair.depth_units(Decimal::from_str(&l.size.to_string())?)?;
                                            Ok(Level { price, units })
                                        })
                                        .filter(|x| !matches!(x,Ok(l) if l.units==0))
                                        .collect()
                                };
                            let idx = if venue == MarketVenue::Lighter { 0 } else { 1 };
                            if let (Ok(bids), Ok(asks)) = (levels(book.bids), levels(book.asks)) {
                                shared.write().unwrap()[idx] = Book {
                                    bids,
                                    asks,
                                    received_ms: book.received_timestamp_ms,
                                    connected: book.sequence_valid,
                                };
                            } else {
                                shared.write().unwrap()[idx].connected = false;
                            }
                        }
                        MarketDataEvent::Gap { venue, .. }
                        | MarketDataEvent::Disconnected { venue, .. } => {
                            shared_marks.write().unwrap()[if venue==MarketVenue::Lighter{0}else{1}].connected=false;
                            shared.write().unwrap()
                                [if venue == MarketVenue::Lighter { 0 } else { 1 }]
                            .connected = false
                        }
                        _ => {}
                    }
                }
            }));
        }
        let backends = match config.mode {
            Mode::Live => live.context("live workers unavailable")?,
            Mode::Paper => {
                #[cfg(feature="paper-runtime")]
                {
                    let parent = path.parent().context("paper ledger directory missing")?;
                    let make = |venue: Venue| -> Result<Box<dyn VenueBackend>> {
                        let backend = venue::PaperBackend::durable(venue,config.clone(),Position::default(),books.clone(),
                            &parent.join(format!("virtual-{:?}.sqlite",venue)))?.with_orderbook_matching().with_isolation(liquidation::IsolationSpec {
                            maintenance_rates:[Decimal::from(market.maintenance_margin_fraction)/Decimal::from(10_000),
                                Decimal::ONE/Decimal::from(asset.max_leverage.context("missing maximum leverage")?*2)],
                            liquidation_fee_rates:[Decimal::new(5,3);2],mark_max_age_ms:5000,
                        },marks.clone())?;
                        Ok(Box::new(backend))
                    };
                    [make(Venue::Lighter)?,make(Venue::Entropy)?]
                }
                #[cfg(not(feature="paper-runtime"))]
                { anyhow::bail!("paper backend not compiled") }
            }
        };
        let [l, e] = backends;
        let workers = [
            AccountWorker::spawn(Venue::Lighter, config.mode, process_dry_run, l)?,
            AccountWorker::spawn(Venue::Entropy, config.mode, process_dry_run, e)?,
        ];
        if state.status == Status::Stopped && state.loss_stop.is_none() {
            state.status = Status::Recovering;
            state.resume_after_recovery = !state.stop_requested;
        }
        store.commit(&state, crate::domain::now_ms(), "service_started")?;
        let mut accounting_cache=accounting::AccountingCache::default();
        let profit_accounting=accounting_cache.report(&state,&books.read().unwrap(),crate::domain::now_ms());
        let view=Arc::new(RwLock::new(InventoryView{entry_first_venue:Venue::Entropy,close_first_venue:Venue::Lighter,snapshot:state.clone(),books:books.read().unwrap().clone(),chart_points:state.entry_mean.points.clone(),marks:marks.read().unwrap().clone(),accounts:None,mean:None,directional_means:[None,None],net_pnl:None,estimated_exit_net:None,profit_accounting,cumulative_fees:state.cumulative_fees(),cumulative_execution_cost:state.execution_cost,execution_cost_started_ms:state.execution_cost_started_ms,execution_cost_tracked_fills:state.execution_cost_tracked_fills,execution_cost_untracked_fills:state.untracked_execution_fills(),
            submission_enabled:false,transient_warning:String::new(),sampling:strategy::entry_sampling_progress(&state,crate::domain::now_ms()),funding_note:if config.mode==Mode::Paper {"虚拟成交；手续费按配置扣除；资金费为模拟估算，并非真实账户结算".into()} else {"Settled funding synchronized from venue history; pending payments are not yet realized".into()}}));
        let (commands, mut rx) = mpsc::channel::<Command>(16);
        let output = view.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(250));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut accounts: Option<[AccountEvidence; 2]> = None;
            let mut last_accounts = 0;
            let mut last_funding_attempt = 0;
            let mut last_funding_backfill = 0;
            let mut funding_warning = String::new();
            let mut last_position_check = 0;
            let mut last_timeout_lookup = 0;
            let mut last_repair_check = 0;
            loop {
                tokio::select! {
                    Some(command)=rx.recv()=>{
                        let name=serde_json::to_string(&command.control).unwrap();
                        let repair_evidence = if matches!(command.control, Control::Reconcile) {
                            let (l,e)=tokio::join!(workers[0].reconcile_account(),workers[1].reconcile_account());
                            match (l,e) {
                                (Ok(l),Ok(e))=>{accounts=Some([l,e]);last_accounts=crate::domain::now_ms();Ok(())},
                                (Err(e),_)|(_,Err(e))=>Err(e),
                            }
                        } else { Ok(()) };
                        let outcome=(||->Result<()>{
                            repair_evidence?;
                            ensure!(!command.id.is_empty() && command.id.len()<=128,"invalid command id");
                            if store.command_seen(&command.id,&name)?{return Ok(());}
                            let mut next=state.clone();
                            ensure!(next.live_orphan.is_none() || matches!(command.control,Control::Shutdown),
                                "emergency incident requires manual reconciliation before strategy controls");
                            validate_loss_stop_control(&next, &command.control)?;
                            match command.control {
                                Control::Start=>{start_continuous(&mut next)?;},
                                Control::StartOneEntry=>{start_one_entry(&mut next, accounts.as_ref(), &books.read().unwrap().clone(), crate::domain::now_ms())?;},
                                Control::StartLiveStrategy=>{start_live_strategy(&mut next, accounts.as_ref(), &books.read().unwrap().clone(), crate::domain::now_ms())?;},
                                Control::Pause=>{next.paused=true;if next.pending.is_none(){next.status=Status::PausedEntries;}},
                                Control::Stop=>{next.stop_requested=true;next.paused=true;next.resume_after_recovery=false;if next.pending.is_none(){next.status=Status::Stopped;}else if next.status!=Status::NeedsAttention {next.reason="finishing active paired operation before stopping".into();}},
                                Control::CloseAll=>{next.paused=true;next.close_requested=true;next.stop_after_close=true;if next.pending.is_none(){next.status=if next.paired_units()==0 {next.close_requested=false;Status::Stopped}else{Status::Closing};}},
                                Control::Reconcile=>{execution::resume_terminal_repair(&mut next,accounts.as_ref(),crate::domain::now_ms())?;next.status=Status::Recovering;},
                                Control::SetDirectionPolicy { direction_policy }=>{set_direction_policy(&mut next, direction_policy, accounts.as_ref(), crate::domain::now_ms())?;},
                                Control::SetEntryOffset { entry_offset }=>{set_entry_offset(&mut next, entry_offset, accounts.as_ref(), crate::domain::now_ms())?;},
                                Control::SetMaxLoss { max_loss_usdc }=>{set_max_loss(&mut next, max_loss_usdc, accounts.as_ref(), crate::domain::now_ms())?;},
                                Control::SetMaxTimeAdds { max_time_adds }=>{set_max_time_adds(&mut next, max_time_adds, accounts.as_ref(), crate::domain::now_ms())?;},
                                Control::Shutdown=>{next.paused=true;next.stop_requested=true;next.status=if next.pending.is_some(){Status::NeedsAttention}else{Status::Stopped};next.reason="worker shut down; reconcile before restarting".into();},
                            }
                            store.command(&next,&command.id,&name,crate::domain::now_ms())?;state=next;Ok(())
                        })();
                        if outcome.as_ref().err().is_some_and(|e|e.to_string().contains("persistence failure")) {state.status=Status::NeedsAttention;state.reason="inventory persistence failure; dispatch disabled".into();}
                        let shutdown=matches!(command.control,Control::Shutdown) && outcome.is_ok();
                        let _=command.reply.send(outcome);
                        if shutdown {let mut v=output.write().unwrap();v.snapshot=state.clone();v.submission_enabled=false;break;}

                    },
                    _=timer.tick()=>{
                        let now=crate::domain::now_ms();let current=books.read().unwrap().clone();
                        let result=async {
                            #[cfg(feature="paper-runtime")]
                            if liquidation::protect(&mut state,&mut store,&workers,&current,now).await? {
                                return Ok::<(),anyhow::Error>(());
                            }
                            // Persist the very same one-second quotes later sent to the chart,
                            // including gaps, before any account I/O or order dispatch.
                            if strategy::observe_entry_mean(&mut state,&current,now) {
                                store.commit(&state,now,"sample")?;
                            }
                            if (state.pending.is_some() || matches!(state.status,Status::NeedsAttention|Status::Stopped|Status::Recovering))
                                && strategy::observe_while_halted(&mut state,&current,now).unwrap_or(false) {
                                store.commit(&state,now,"halted_market_sample")?;
                            }
                            // Check the combined liquidation PnL before awaiting account or
                            // funding refreshes and before any pending first-leg dispatch.
                            if state.live_orphan.is_none() && strategy::enforce_loss_limit(&mut state,&current,now).unwrap_or(false) {
                                store.commit(&state,now,"loss_limit_latched")?;
                                tracing::warn!("OPENAI total loss limit latched; closing owned inventory and stopping");
                            }
                            if state.pending.is_some() && state.status==Status::NeedsAttention
                                && state.reason.starts_with("order unresolved past execution deadline")
                                && now.saturating_sub(last_timeout_lookup)>=30_000 {
                                last_timeout_lookup=now;
                                execution::recheck_timed_out(&mut state,&mut store,&workers,now).await?;
                            } else if state.pending.is_some() && state.status!=Status::NeedsAttention {
                                execution::advance(&mut state,&mut store,&workers,&current,now).await?;
                                last_accounts=0;
                            } else if execution::automatic_repair_due(&state,now)
                                && now.saturating_sub(last_repair_check)>=execution::REPAIR_RETRY_DELAY_MS {
                                last_repair_check=now;
                                let (l,e)=tokio::join!(workers[0].reconcile_account(),workers[1].reconcile_account());
                                let verified=[l?,e?];
                                let checked_at=crate::domain::now_ms();
                                let mut next=state.clone();
                                if execution::resume_protected_repair(&mut next,&verified,
                                    &books.read().unwrap().clone(),checked_at)? {
                                    store.commit(&next,checked_at,"residual_retry_rest_verified")?;
                                    state=next;
                                }
                                accounts=Some(verified);last_accounts=checked_at;
                            } else {
                                if now.saturating_sub(last_accounts)>=1000 {
                                    let (l,e)=if last_accounts==0 {tokio::join!(workers[0].reconcile_account(),workers[1].reconcile_account())}else{tokio::join!(workers[0].account(),workers[1].account())};accounts=Some([l?,e?]);last_accounts=now;
                                }
                                if let Some(a)=&accounts {
                                    if live_orphan::protect(&mut state,&mut store,&workers,&current,a,now).await? {
                                        return Ok::<(),anyhow::Error>(());
                                    }
                                }
                                if (last_funding_attempt==0 || now.saturating_sub(last_funding_attempt)>=30_000) && (state.funding_synced_ms==0 || now.saturating_sub(state.funding_synced_ms)>=60_000) {
                                    last_funding_attempt=now;
                                    // Legacy ledgers retain IDs/totals but not the settled rows.
                                    // Backfill details at most once per five minutes; IDs prevent double booking.
                                    let backfill=!accounting::funding_history_complete(&state)
                                        && (last_funding_backfill==0 || now.saturating_sub(last_funding_backfill)>=300_000);
                                    if backfill {last_funding_backfill=now;}
                                    let start=if backfill {state.created_ms}
                                        else {state.funding_synced_ms.saturating_sub(3_600_000).max(state.created_ms)};
                                    let (l,e)=tokio::join!(workers[0].funding(start,now),workers[1].funding(start,now));
                                    if let (Ok(l),Ok(e))=(l,e) {
                                        let mut next=state.clone();
                                        for f in l.into_iter().chain(e) {accounting::record_funding(&mut next,f)?;}
                                        funding_warning.clear();next.funding_synced_ms=now;store.commit(&next,now,"funding_reconciled")?;state=next;
                                    }else {funding_warning="资金费同步暂不可用，等待重新核验".into();}
                                }
                                let mismatch=accounts.as_ref().is_some_and(|a|state.assert_reconciled(a).is_err());
                                let recovery_due=state.recovery_after_ms.is_some_and(|t|now>=t);
                                let mut fresh_recovery_evidence=false;
                                let checking=state.status==Status::NeedsAttention && state.reason.starts_with("venue position differs from owned ledger");
                                if state.pending.is_none() && (mismatch || checking || recovery_due) && now.saturating_sub(last_position_check)>=15_000 {
                                    last_position_check=now;
                                    let (l,e)=tokio::join!(workers[0].reconcile_account(),workers[1].reconcile_account());
                                    let verified=[l?,e?];
                                    let mut next=state.clone();
                                    if clear_verified_position_halt(&mut next,&verified,crate::domain::now_ms())? {
                                        store.commit(&next,crate::domain::now_ms(),"position_halt_rest_verified")?;state=next;
                                    }
                                    fresh_recovery_evidence=true;accounts=Some(verified);last_accounts=crate::domain::now_ms();
                                }
                                if let Some(a)=&accounts {
                                    state.assert_reconciled(a)?;
                                    let mut resumed=state.clone();
                                    if fresh_recovery_evidence && resume_verified_rollback(&mut resumed,&books.read().unwrap().clone(),a,crate::domain::now_ms())? {
                                        store.commit(&resumed,crate::domain::now_ms(),"rollback_auto_resumed")?;
                                        state=resumed;
                                    }
                                    if state.status==Status::Recovering {
                                        ensure!(a.iter().all(|x|x.open_orders==0),"unowned or unresolved orders at recovery");
                                        state.status=if !state.resume_after_recovery {Status::Stopped}else if state.paused {Status::PausedEntries}else{Status::Warming};
                                    }
                                    let prior=state.last_sample_ms;
                                    let prior_decision=state.decision_observation;
                                    let prior_loss_stop=state.loss_stop.is_some();
                                    let decision_now=crate::domain::now_ms();let decision_books=books.read().unwrap().clone();
                                    if let Some(operation)=strategy::evaluate(&mut state,&decision_books,a,decision_now)? {
                                        state.pending=Some(operation);store.commit(&state,decision_now,"operation_reserved")?;
                                    } else if !prior_loss_stop && state.loss_stop.is_some() {store.commit(&state,decision_now,"loss_limit_latched")?;}
                                    else if state.last_sample_ms!=prior || state.decision_observation!=prior_decision {store.commit(&state,now,"sample")?;}
                                }
                            }
                            Ok::<(),anyhow::Error>(())
                        }.await;
                        let mut transient_warning=funding_warning.clone();
                        if let Err(error)=result {
                            strategy::clear_entry_confirmation(&mut state);
                            let message=format!("{error:#}");
                            let hard=state.pending.is_some() || message.contains("position") || message.contains("unpaired") || message.contains("unowned") || message.contains("worker stopped") || message.contains("worker unavailable") || message.contains("persistence failure");
                            if hard {
                                // Preserve the first actionable hard-stop reason. Once the
                                // halt is established, downstream errors must not append a
                                // full-state event four times per second.
                                let changed=state.status!=Status::NeedsAttention || state.recovery_after_ms.is_some();
                                if state.status!=Status::NeedsAttention || state.recovery_after_ms.is_some() {state.reason=message.clone();}
                                state.recovery_after_ms=None;
                                state.status=Status::NeedsAttention;
                                if changed {let _=store.commit(&state,now,"reconciliation_blocked");}
                            }
                            transient_warning=if transient_warning.is_empty(){message}else{format!("{transient_warning} · {message}")};
                        }
                        let mean_now=crate::domain::now_ms();
                        let mean=strategy::entry_reference_mean_for(&state,mean_now,Direction::LighterLong);
                        let mut v=output.write().unwrap();
                        append_chart_samples(&mut v.chart_points, &state.entry_mean.points, mean_now);
                        if state.status==Status::NeedsAttention && (v.snapshot.status!=state.status || v.snapshot.reason!=state.reason) {tracing::warn!(reason=%state.reason,"OPENAI inventory needs operator attention");}
                        v.transient_warning=transient_warning;
                        v.submission_enabled=v.transient_warning.is_empty() && state.live_orphan.is_none() && config.mode==Mode::Live && workers.iter().all(AccountWorker::is_alive) && matches!(state.status,Status::Running|Status::Closing|Status::PausedEntries);
                        v.snapshot=state.clone();v.books=current.clone();v.accounts=accounts.clone();v.mean=mean;v.directional_means=[Direction::LighterLong,Direction::LighterShort].map(|d|strategy::entry_reference_mean_for(&state,mean_now,d));v.net_pnl=if state.live_orphan.is_some(){None}else{state.total_pnl(&current).ok()};v.estimated_exit_net=if state.live_orphan.is_some(){None}else{state.remaining_net(&current).ok()};v.cumulative_fees=state.cumulative_fees();v.cumulative_execution_cost=state.execution_cost;v.execution_cost_started_ms=state.execution_cost_started_ms;v.execution_cost_tracked_fills=state.execution_cost_tracked_fills;v.execution_cost_untracked_fills=state.untracked_execution_fills();
                        v.profit_accounting=accounting_cache.report(&state,&current,mean_now);
                        v.sampling=strategy::entry_sampling_progress(&state,mean_now);v.marks=marks.read().unwrap().clone();
                    }
                }
            }
            for task in feed_tasks {
                task.abort();
            }
        });
        Ok(Self { view, commands })
    }
    pub fn status(&self) -> InventoryView {
        self.view.read().unwrap().clone()
    }
    pub async fn control(&self, id: String, control: Control) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .try_send(Command {
                id,
                control,
                reply: tx,
            })
            .context("inventory command queue full")?;
        rx.await.context("inventory service stopped")?
    }
}

fn append_chart_samples(points: &mut VecDeque<charts::QuotePoint>, samples: &VecDeque<charts::QuotePoint>, now: u64) {
    for sample in samples {
        if sample.time_ms <= now && points.back().is_none_or(|p| p.time_ms < sample.time_ms) {
            points.push_back(sample.clone());
        }
    }
    let cutoff = now.saturating_sub(4 * 60 * 60 * 1000);
    while points.front().is_some_and(|first| first.time_ms < cutoff) {
        points.pop_front();
    }
}

#[cfg(test)]
mod live_chart_tests {
    use super::*;

    #[test]
    fn live_chart_uses_fresh_quotes_once_per_second_and_keeps_four_hours() {
        let mut points = VecDeque::new();
        let mut sampler = entry_mean::EntryMean::default();
        let books = [(105, 106), (100, 101)].map(|(bid, ask)| Book {
            bids: vec![Level { price: bid.into(), units: 100 }],
            asks: vec![Level { price: ask.into(), units: 100 }],
            received_ms: 1_000,
            connected: true,
        });
        sampler.observe(&books, 1_000, 1_500, 300_000);
        append_chart_samples(&mut points, &sampler.points, 1_000);
        sampler.observe(&books, 1_500, 1_500, 300_000);
        append_chart_samples(&mut points, &sampler.points, 1_500);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].lighter_bid, Some(105.into()));
        sampler.observe(&books, 3_000, 1_500, 300_000);
        append_chart_samples(&mut points, &sampler.points, 3_000);
        assert!(points[1].lighter_bid.is_none());
        let end = 4 * 60 * 60 * 1_000 + 3_000;
        sampler.observe(&books, end, 1_500, 300_000);
        append_chart_samples(&mut points, &sampler.points, end);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].time_ms, 3_000);
    }
}

/// A live process may resume hedging an already submitted leg and managing
/// owned inventory, but starting the process alone must never arm a new entry.
fn quarantine_live_start(state: &mut Snapshot) {
    if state.config.mode != Mode::Live {
        return;
    }
    state.paused = true;
    if state.live_orphan.is_none() && state.pending.is_none()
        && state.lots.is_empty()
        && state.positions.iter().all(|position| position.units == 0)
    {
        state.stop_requested = true;
        state.resume_after_recovery = false;
        state.status = Status::Stopped;
        state.reason = "live entry stopped until explicit bounded start".into();
    }
}

#[cfg(test)]
mod live_start_tests {
    use super::*;

    #[test]
    fn a_flat_live_launch_requires_an_explicit_bounded_start() {
        let mut config = InventoryConfig::default();
        config.mode = Mode::Live;
        config.entropy_address = "0x1111111111111111111111111111111111111111".into();
        config.lighter_account_index = Some(1);
        let mut state = Snapshot::new(config).unwrap();
        state.status = Status::Recovering;
        state.resume_after_recovery = true;
        quarantine_live_start(&mut state);
        assert_eq!(state.status, Status::Stopped);
        assert!(state.paused && state.stop_requested);
        assert!(!state.resume_after_recovery);
        assert!(start_continuous(&mut state).is_err());
        assert_eq!(state.status, Status::Stopped);

        let mut exposed = state.clone();
        exposed.positions[0].units = 100;
        exposed.status = Status::Recovering;
        quarantine_live_start(&mut exposed);
        assert_eq!(exposed.status, Status::Recovering);
        assert!(exposed.paused);
    }
}

pub(super) fn set_entry_offset(
    s: &mut Snapshot,
    value: Decimal,
    accounts: Option<&[AccountEvidence; 2]>,
    now: u64,
) -> Result<()> {
    ensure!(
        s.loss_stop.is_none()
            && s.paused
            && s.pending.is_none()
            && s.lots.is_empty()
            && s.positions.iter().all(|p| p.units == 0),
        "entry tuning requires paused, flat, reconciled inventory without a loss latch"
    );
    let a = accounts.context("account evidence required for entry tuning")?;
    ensure!(
        a.iter().all(|a| a.authenticated
            && a.open_orders == 0
            && a.position_units == 0
            && a.observed_ms <= now
            && now - a.observed_ms <= s.config.account_max_age_ms),
        "fresh flat accounts without orders required for entry tuning"
    );
    let mut cfg = s.config.clone();
    cfg.entry_offset = value;
    cfg.validate()?;
    s.config = cfg;
    s.previous_signal = None;
    s.previous_reverse_signal = None;
    s.first_armed = true;
    s.reason = format!("entry offset updated to {}; remains paused", value);
    Ok(())
}

pub(super) fn set_direction_policy(
    s: &mut Snapshot, policy: DirectionPolicy, accounts: Option<&[AccountEvidence; 2]>, now: u64,
) -> Result<()> {
    let mut next = s.clone();
    // Reuse the paused/flat/fresh-account controls without altering the entry offset.
    let evidence = accounts.context("account evidence required for direction update")?;
    next.assert_reconciled(evidence)?;
    ensure!(evidence[0].account == next.config.lighter_account && evidence[1].account == next.config.entropy_account,
        "direction update account binding mismatch");
    let offset = next.config.entry_offset;
    set_entry_offset(&mut next, offset, accounts, now)?;
    next.config.direction_policy = policy;
    next.config.validate()?;
    next.direction = Direction::default();
    next.previous_signal = None;
    next.previous_reverse_signal = None;
    next.previous_exit = None;
    next.first_armed = true;
    next.reverse_first_armed = true;
    next.reason = format!("direction policy updated to {policy:?}; remains paused");
    *s = next;
    Ok(())
}

pub(super) fn start_continuous(s: &mut Snapshot) -> Result<()> {
    ensure!(s.config.mode == Mode::Paper,
        "continuous OPENAI inventory start remains paper-only; live uses bounded entry");
    ensure!(s.pending.is_none(), "reconcile pending operation first");
    s.entry_attempts_remaining = None;
    s.recovery_after_ms = None;
    s.consecutive_rollbacks = 0;
    s.reason.clear();
    s.paused = false;
    s.stop_requested = false;
    s.stop_after_close = false;
    s.status = Status::Recovering;
    s.resume_after_recovery = true;
    Ok(())
}

pub(super) fn set_max_loss(
    s: &mut Snapshot,
    value: Decimal,
    accounts: Option<&[AccountEvidence; 2]>,
    now: u64,
) -> Result<()> {
    ensure!(
        s.loss_stop.is_none()
            && s.paused
            && s.stop_requested
            && s.pending.is_none()
            && s.lots.is_empty()
            && s.positions.iter().all(|p| p.units == 0),
        "loss-limit tuning requires stopped, flat, reconciled inventory without a loss latch"
    );
    let a = accounts.context("account evidence required for loss-limit tuning")?;
    ensure!(
        a.iter().all(|a| a.authenticated
            && a.open_orders == 0
            && a.position_units == 0
            && a.observed_ms <= now
            && now - a.observed_ms <= s.config.account_max_age_ms),
        "fresh flat accounts without orders required for loss-limit tuning"
    );
    let mut cfg = s.config.clone();
    cfg.max_loss_usdc = value;
    cfg.validate()?;
    s.config = cfg;
    s.reason = format!("loss limit updated to {} USDC; remains stopped", value);
    Ok(())
}

/// Adjust the approved time-add quota without resetting held inventory or its used count.
pub(super) fn set_max_time_adds(
    s: &mut Snapshot, value: usize, accounts: Option<&[AccountEvidence; 2]>, now: u64,
) -> Result<()> {
    ensure!(s.status == Status::Stopped && s.paused && s.stop_requested
        && s.pending.is_none() && s.loss_stop.is_none() && s.live_orphan.is_none()
        && !s.close_requested && !s.stop_after_close && s.recovery_after_ms.is_none(),
        "time-add tuning requires stopped, reconciled inventory without pending operations or risk locks");
    ensure!(value >= s.time_adds_used, "time-add quota cannot be below the used count");
    let a = accounts.context("account evidence required for time-add tuning")?;
    s.assert_reconciled(a)?;
    for (i, x) in a.iter().enumerate() {
        let expected = if i == 0 { &s.config.lighter_account } else { &s.config.entropy_account };
        ensure!(x.authenticated && x.venue.index() == i && &x.account == expected
            && x.open_orders == 0 && x.observed_ms <= now
            && now - x.observed_ms <= s.config.account_max_age_ms,
            "fresh bound accounts without orders required for time-add tuning");
    }
    let mut cfg = s.config.clone();
    cfg.accumulation.as_mut().context("accumulation rules required")?.max_time_adds = value;
    cfg.validate()?;
    s.config = cfg;
    strategy::clear_entry_confirmation(s);
    s.reason = format!("time-add quota updated to {value}; used count retained; remains stopped");
    Ok(())
}

pub(super) fn start_one_entry(
    s: &mut Snapshot,
    accounts: Option<&[AccountEvidence; 2]>,
    books: &[Book; 2],
    now: u64,
) -> Result<()> {
    ensure!(
        s.status == Status::Stopped && s.stop_requested,
        "stop strategy before starting bounded test"
    );
    ensure!(
        s.pending.is_none() && s.live_orphan.is_none() && s.loss_stop.is_none() && !s.close_requested && !s.stop_after_close,
        "pending operation or exit risk lock"
    );
    let accounts = accounts.context("account evidence required")?;
    s.assert_reconciled(accounts)?;
    ensure!(
        s.config.mode == Mode::Paper
            || (s.funding_synced_ms > 0 && now.saturating_sub(s.funding_synced_ms) <= 90_000),
        "funding evidence stale"
    );
    let qty = s.config.common_units(
        s.config.group_notional,
        (books[0].mid().context("missing book")? + books[1].mid().context("missing book")?)
            / Decimal::TWO,
    )?;
    strategy::risk_check(s, books, accounts, now, qty, Action::Open)?;
    s.entry_attempts_remaining = Some(1);
    s.exit_batch_active = false;
    s.previous_signal = None;
    s.previous_reverse_signal = None;
    s.previous_exit = None;
    s.paused = false;
    s.stop_requested = false;
    s.resume_after_recovery = true;
    s.status = Status::Recovering;
    s.reason =
        "bounded test: at most one new entry reservation; profit exits remain enabled".into();
    Ok(())
}

pub(super) fn start_live_strategy(
    s: &mut Snapshot, accounts: Option<&[AccountEvidence; 2]>, books: &[Book; 2], now: u64,
) -> Result<()> {
    s.config.validate()?;
    ensure!(s.config.mode == Mode::Live && s.config.max_groups == 20
        && s.config.max_notional_per_venue == Decimal::from(300)
        && s.config.max_loss_usdc == Decimal::from(30), "approved continuous live bounds required");
    ensure!(matches!(s.status, Status::Stopped | Status::PausedEntries | Status::Running | Status::Warming)
        && s.pending.is_none() && s.live_orphan.is_none() && s.loss_stop.is_none()
        && !s.close_requested && !s.stop_after_close && s.recovery_after_ms.is_none()
        && s.consecutive_rollbacks < 3, "reconcile pending operation or risk lock before activation");
    let a = accounts.context("account evidence required")?;
    s.assert_reconciled(a)?;
    ensure!(s.funding_synced_ms > 0 && s.funding_synced_ms <= now
        && now - s.funding_synced_ms <= 90_000, "funding evidence stale");
    for i in 0..2 {
        books[i].validate(now, s.config.book_max_age_ms)?;
        let expected_account = if i == 0 { &s.config.lighter_account } else { &s.config.entropy_account };
        ensure!(a[i].authenticated && a[i].venue.index() == i
            && &a[i].account == expected_account
            && a[i].observed_ms <= now && now - a[i].observed_ms <= s.config.account_max_age_ms
            && a[i].open_orders == 0 && a[i].isolated && a[i].leverage == s.config.leverage,
            "fresh reconciled isolated accounts without orders required");
    }
    // Preserve lots, anchor, grid arming, add counters, PnL and loss history.
    strategy::clear_decision_confirmation(s);
    s.entry_attempts_remaining = None;
    s.paused = false;
    s.stop_requested = false;
    s.resume_after_recovery = true;
    s.status = Status::Running;
    s.reason = "continuous strategy enabled; existing inventory and risk checks retained".into();
    Ok(())
}

fn validate_loss_stop_control(s: &Snapshot, control: &Control) -> Result<()> {
    if s.loss_stop.is_some() {
        ensure!(
            !matches!(
                control,
                Control::Start
                    | Control::StartOneEntry
                    | Control::StartLiveStrategy
                    | Control::SetEntryOffset { .. }
                    | Control::SetMaxLoss { .. }
                    | Control::SetMaxTimeAdds { .. }
                    | Control::SetDirectionPolicy { .. }
            ),
            "loss limit latched; this instance cannot resume trading"
        );
        ensure!(
            !matches!(control, Control::Pause | Control::Stop),
            "loss limit exit is latched; use close_all, reconcile or shutdown"
        );
    }
    Ok(())
}

/// Called only with fresh explicit REST reconciliation, never a stream-only match.
pub(super) fn clear_verified_position_halt(
    s: &mut Snapshot,
    a: &[AccountEvidence; 2],
    now: u64,
) -> Result<bool> {
    if s.pending.is_some() {
        return Ok(false);
    }
    s.assert_reconciled(a)?;
    for (i, x) in a.iter().enumerate() {
        ensure!(
            x.authenticated
                && x.observed_ms <= now
                && now - x.observed_ms <= s.config.account_max_age_ms,
            "reconciliation account evidence stale or unauthenticated"
        );
        ensure!(
            x.account
                == if i == 0 {
                    s.config.lighter_account.clone()
                } else {
                    s.config.entropy_account.clone()
                },
            "account binding mismatch"
        );
        ensure!(
            x.open_orders == 0,
            "unowned or unresolved orders at recovery"
        );
    }
    if s.status != Status::NeedsAttention
        || !s
            .reason
            .starts_with("venue position differs from owned ledger")
    {
        return Ok(false);
    }
    s.status = if s.loss_stop.is_some() || s.close_requested {
        Status::Closing
    } else if s.stop_requested {
        Status::Stopped
    } else if s.paused {
        Status::PausedEntries
    } else {
        Status::Warming
    };
    s.reason = "position discrepancy cleared by explicit account reconciliation".into();
    s.previous_signal = None;
    s.previous_reverse_signal = None;
    s.previous_exit = None;
    Ok(true)
}

// Only terminal, fully repaired operations may arm this timer in finish_operation.
pub(super) fn resume_verified_rollback(
    s: &mut Snapshot,
    books: &[Book; 2],
    a: &[AccountEvidence; 2],
    now: u64,
) -> Result<bool> {
    let Some(after) = s.recovery_after_ms else {
        return Ok(false);
    };
    if now < after
        || s.status != Status::NeedsAttention
        || s.pending.is_some()
        || s.loss_stop.is_some()
        || s.paused
        || s.stop_requested
        || s.stop_after_close
        || s.close_requested
        || s.consecutive_rollbacks >= 3
    {
        return Ok(false);
    }
    for b in books {
        if b.validate(now, s.config.book_max_age_ms).is_err() {
            return Ok(false);
        }
    }
    if a.iter().any(|x| {
        !x.authenticated
            || now.saturating_sub(x.observed_ms) > s.config.account_max_age_ms
            || x.open_orders != 0
    }) || now.saturating_sub(s.funding_synced_ms) > 60_000
    {
        return Ok(false);
    }
    s.assert_reconciled(a)?;
    s.recovery_after_ms = None;
    s.reason.clear();
    s.previous_signal = None;
    s.previous_reverse_signal = None;
    s.previous_exit = None;
    s.status = Status::Warming;
    Ok(true)
}

#[cfg(test)]
mod loss_control_tests {
    use super::*;
    #[test]
    fn loss_stop_cannot_be_cleared_by_resume_or_pause() {
        let mut s = Snapshot::new(InventoryConfig::default()).unwrap();
        s.loss_stop = Some(LossStop {
            at_ms: 1,
            net_pnl: Decimal::from(-10),
        });
        for c in [Control::Start, Control::Pause, Control::Stop] {
            assert!(validate_loss_stop_control(&s, &c).is_err());
        }
        for c in [Control::CloseAll, Control::Reconcile, Control::Shutdown] {
            assert!(validate_loss_stop_control(&s, &c).is_ok());
        }
    }
}
