use super::*;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub struct Store {
    db: Connection,
    _lock: Option<File>,
}
impl Store {
    /// Isolated, non-durable ledger used only by offline paper replays.
    #[cfg(test)]
    pub fn offline_replay(config: &InventoryConfig) -> Result<(Self, Snapshot)> {
        ensure!(config.mode == Mode::Paper, "offline replay requires paper mode");
        let state = Snapshot::new(config.clone())?;
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE state(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);
            CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT,at_ms INTEGER NOT NULL,kind TEXT NOT NULL,body TEXT NOT NULL);
            CREATE TABLE commands(id TEXT PRIMARY KEY,command TEXT NOT NULL,result TEXT NOT NULL);")?;
        db.execute_batch(alerts::SCHEMA)?;
        let mut store = Self { db, _lock: None };
        store.commit(&state, 0, "offline_replay_created")?;
        Ok((store, state))
    }
    /// Export a completed offline ledger; never replaces a running ledger.
    #[cfg(test)]
    pub fn export_replay(&self, path: &Path) -> Result<()> {
        ensure!(self._lock.is_none() && !path.exists(), "replay export requires a new path and an offline ledger");
        self.db.execute("VACUUM INTO ?1", [path.to_str().context("invalid export path")?])?;
        Ok(())
    }
    pub fn open(path: &Path, config: &InventoryConfig) -> Result<(Self, Snapshot)> {
        Self::open_inner(path, config, false)
    }
    pub fn open_with_live_limit_upgrade(path: &Path, config: &InventoryConfig) -> Result<(Self, Snapshot)> {
        Self::open_inner(path, config, true)
    }
    fn open_inner(path: &Path, config: &InventoryConfig, upgrade: bool) -> Result<(Self, Snapshot)> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.try_lock()
            .context("another inventory process owns this database")?;
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS state(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT,at_ms INTEGER NOT NULL,kind TEXT NOT NULL,body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS commands(id TEXT PRIMARY KEY,command TEXT NOT NULL,result TEXT NOT NULL);")?;
        db.execute_batch(alerts::SCHEMA)?;
        let body: Option<String> = db
            .query_row("SELECT body FROM state WHERE id=1", [], |r| r.get(0))
            .optional()?;
        let mut state = if let Some(body) = body {
            let mut s: Snapshot = serde_json::from_str(&body)?;
            if upgrade && s.config != *config {
                config.validate_live_limit_upgrade(&s.config)?;
                ensure!(s.schema == 1 && s.paused && s.stop_requested
                    && s.status == Status::Stopped && s.pending.is_none()
                    && s.live_orphan.is_none() && s.loss_stop.is_none()
                    && !s.close_requested && !s.stop_after_close && s.time_adds_used == 0,
                    "live limit upgrade requires stopped inventory without unresolved operations or risk locks");
                ensure!(s.lots.iter().all(|lot| lot.level < s.config.max_groups),
                    "cannot remap existing time-add inventory");
                s.armed.resize(config.max_groups, true);
                s.config = config.clone();
                // Restart still requires explicit activation after account reconciliation.
                s.entry_attempts_remaining = Some(0);
            }
            ensure!(
                s.schema == 1 && s.config == *config,
                "database configuration/mode differs; use original configuration and database"
            );
            s
        } else {
            Snapshot::new(config.clone())?
        };
        ensure!(state.direction == Direction::LighterLong || config.direction_policy == DirectionPolicy::Both,
            "reverse inventory requires bidirectional configuration");
        if !state.mean_initialized
            && state.last_sample_ms > 0
            && strategy::sampling_progress(&state, state.last_sample_ms).ready
        {
            state.mean_initialized = true;
        }
        if state.mean_initialized
            && state.continuity_mean.is_none()
            && state.last_sample_ms > 0
            && strategy::sampling_progress(&state, state.last_sample_ms).ready
        {
            state.continuity_mean = strategy::reference_mean(&state, state.last_sample_ms)
                .map(|m| (state.last_sample_ms, m));
        }
        state.resume_after_recovery =
            !matches!(state.status, Status::Stopped | Status::NeedsAttention);
        if state.resume_after_recovery || state.pending.is_some() || !state.lots.is_empty() {
            state.status = Status::Recovering;
        }
        if state.live_orphan.is_some() {
            state.status = Status::NeedsAttention;
            state.paused = true;
            state.stop_requested = true;
            state.resume_after_recovery = false;
        }
        let mut out = Self { db, _lock: Some(lock) };
        if state.config.decision_ms.is_some() {
            strategy::clear_decision_confirmation(&mut state);
        }
        out.commit(&state, 0, "loaded")?;
        Ok((out, state))
    }
    pub fn commit(&mut self, s: &Snapshot, at: u64, kind: &str) -> Result<()> {
        self.commit_inner(s, at, kind)
            .context("inventory persistence failure")
    }
    fn commit_inner(&mut self, s: &Snapshot, at: u64, kind: &str) -> Result<()> {
        let body = serde_json::to_string(s)?;
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO state(id,body) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body",[&body])?;
        // Full durable state in one row; compact event log avoids duplicating long fill/sample histories.
        let event = serde_json::json!({"status":s.status,"sequence":s.sequence,"pending":s.pending,"reason":s.reason,"groups":s.lots.len()});
        if kind != "sample" {
            tx.execute(
                "INSERT INTO events(at_ms,kind,body) VALUES(?1,?2,?3)",
                params![at, kind, event.to_string()],
            )?;
        }
        alerts::observe(&tx,s,at)?;
        tx.commit()?;
        Ok(())
    }
    pub fn command_seen(&self, id: &str, command: &str) -> Result<bool> {
        let existing: Option<String> = self
            .db
            .query_row("SELECT command FROM commands WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(x) = existing {
            ensure!(x == command, "command id collision");
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub fn command(&mut self, s: &Snapshot, id: &str, command: &str, at: u64) -> Result<()> {
        self.command_inner(s, id, command, at)
            .context("inventory persistence failure")
    }
    fn command_inner(&mut self, s: &Snapshot, id: &str, command: &str, at: u64) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2,'accepted')",
            params![id, command],
        )?;
        tx.execute(
            "UPDATE state SET body=?1 WHERE id=1",
            [serde_json::to_string(s)?],
        )?;
        tx.execute(
            "INSERT INTO events(at_ms,kind,body) VALUES(?1,'command',?2)",
            params![
                at,
                serde_json::json!({"id":id,"command":command}).to_string()
            ],
        )?;
        alerts::observe(&tx,s,at)?;
        tx.commit()?;
        Ok(())
    }
}
