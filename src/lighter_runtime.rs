//! Safety-critical Lighter signing, nonce ownership, and copy persistence.
//!
//! This module is intentionally independent from the Hyperliquid runtime. It
//! keeps Robinhood Lighter secrets in zeroizing memory, enforces one in-flight
//! nonce per API key, and persists copy decisions before submission.

use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use nautilus_lighter::signing::{
    auth_token::{build_auth_token, fresh_k},
    schnorr::PrivateKey,
    tx::{
        CreateOrderTxInfo, L2TxAttributes, OrderInfo, TxContext, TxInfoJson, compute_tx_hash,
        sign_tx,
    },
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    lighter::{LighterOrderPlan, SignedLighterTransaction},
    lighter_reconcile::{LighterAccountObservation, LighterRemoteOrderState},
};

const COPY_LEDGER_SCHEMA_VERSION: u32 = 1;

fn missing_account_index() -> i64 {
    -1
}

/// Shared local lock directory for every Lighter execution path. All modules
/// that can sign with an API key must use this directory so one process owns
/// that key's nonce sequence at a time.
pub const LIGHTER_NONCE_LOCK_DIR: &str = ".codex-longrun/lighter-nonce-locks";

/// A Lighter API credential. The 40-byte secret is redacted from Debug output
/// and zeroized when this value is dropped.
pub struct LighterApiCredential {
    pub account_index: i64,
    pub api_key_index: u8,
    private_key: Zeroizing<[u8; 40]>,
}

impl fmt::Debug for LighterApiCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LighterApiCredential")
            .field("account_index", &self.account_index)
            .field("api_key_index", &self.api_key_index)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl LighterApiCredential {
    pub fn from_hex(account_index: i64, api_key_index: u8, private_key: &str) -> Result<Self> {
        ensure!(account_index >= 0, "account_index must be non-negative");
        ensure!(
            api_key_index <= 254,
            "api_key_index must be between 0 and 254"
        );
        let decoded = decode_fixed_hex::<40>(private_key, "Lighter API private key")?;
        let scalar = PrivateKey::from_le_bytes_reduce(decoded).as_scalar();
        ensure!(!scalar.is_zero(), "Lighter API private key reduces to zero");
        Ok(Self {
            account_index,
            api_key_index,
            private_key: Zeroizing::new(decoded),
        })
    }

    pub fn public_key_hex(&self) -> String {
        let signer = PrivateKey::from_le_bytes_reduce(*self.private_key);
        encode_hex_prefixed(&signer.public_key().to_le_bytes())
    }

    /// Account-worker-only caller must hold the shared nonce guard before signing.
    pub fn sign_inventory_leverage(
        &self,
        market: i32,
        leverage: u32,
        margin_mode: u8,
        chain_id: i64,
        nonce: i64,
        expires: i64,
    ) -> Result<SignedLighterTransaction> {
        use nautilus_lighter::signing::tx::UpdateLeverageTxInfo;
        ensure!(
            (1..=3).contains(&leverage) && margin_mode <= 1 && nonce >= 0 && chain_id > 0,
            "invalid leverage action"
        );
        let fraction = 10_000u32.div_ceil(leverage);
        let tx = UpdateLeverageTxInfo {
            context: TxContext {
                account_index: self.account_index,
                api_key_index: self.api_key_index,
                nonce,
                expired_at: expires,
            },
            market_index: i16::try_from(market)?,
            initial_margin_fraction: u16::try_from(fraction)?,
            margin_mode,
            skip_nonce: 0,
        };
        let key = PrivateKey::from_le_bytes_reduce(*self.private_key);
        let signed = sign_tx(&tx, u32::try_from(chain_id)?, &key, fresh_k());
        Ok(SignedLighterTransaction {
            tx_type: 20,
            tx_info: TxInfoJson::update_leverage(&tx, &signed),
            tx_hash: Some(signed.tx_hash_hex()),
        })
    }

    pub fn verify_registered_public_key(&self, registered_public_key: &str) -> Result<()> {
        let local = self.public_key_hex();
        let normalize = |value: &str| {
            let trimmed = value.trim();
            let hex = if trimmed
                .get(..2)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
            {
                &trimmed[2..]
            } else {
                trimmed
            };
            hex.to_ascii_lowercase()
        };
        ensure!(
            normalize(&local) == normalize(registered_public_key),
            "Vault Lighter private key does not match the public key registered for this account/API-key index"
        );
        Ok(())
    }

    /// Mints a short-lived API-key-bound token for authenticated REST and
    /// private WebSocket account streams. The venue maximum is eight hours;
    /// callers use a shorter lifetime so a stalled worker fails closed.
    pub fn auth_token(&self, ttl_secs: i64) -> Result<Zeroizing<String>> {
        ensure!(
            (60..=8 * 60 * 60).contains(&ttl_secs),
            "Lighter auth token TTL must be between 60 seconds and 8 hours"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock is before UNIX epoch")?
            .as_secs();
        let now = i64::try_from(now).context("system clock seconds exceed i64")?;
        let deadline = now
            .checked_add(ttl_secs)
            .context("auth deadline overflow")?;
        let signer = PrivateKey::from_le_bytes_reduce(*self.private_key);
        let k = fresh_k();
        ensure!(!k.is_zero(), "CSPRNG produced an invalid zero auth nonce");
        let token = build_auth_token(deadline, self.account_index, self.api_key_index, &signer, k)
            .context("failed to mint Lighter auth token")?;
        Ok(Zeroizing::new(token))
    }

    /// Signs one already risk-approved deterministic order plan.
    pub fn sign_order(
        &self,
        plan: &LighterOrderPlan,
        chain_id: i64,
        nonce: i64,
        expired_at_ms: i64,
    ) -> Result<SignedLighterTransaction> {
        ensure!(
            chain_id > 0 && chain_id <= u32::MAX as i64,
            "invalid chain id"
        );
        ensure!(nonce >= 0, "nonce must be non-negative");
        ensure!(expired_at_ms > 0, "expired_at_ms must be positive");
        ensure!(plan.base_amount > 0, "base_amount must be positive");
        let market_index = i16::try_from(plan.market_index).context("market index exceeds i16")?;
        let price = u32::try_from(plan.price).context("price must fit u32")?;
        let order_type = u8::try_from(plan.order_type).context("order type must fit u8")?;
        let time_in_force =
            u8::try_from(plan.time_in_force).context("time in force must fit u8")?;
        ensure!((0..=1).contains(&plan.is_ask), "is_ask must be 0 or 1");

        let tx = CreateOrderTxInfo {
            context: TxContext {
                account_index: self.account_index,
                api_key_index: self.api_key_index,
                nonce,
                expired_at: expired_at_ms,
            },
            order: OrderInfo {
                market_index,
                client_order_index: plan.client_order_index,
                base_amount: plan.base_amount,
                price,
                is_ask: plan.is_ask == 1,
                order_type,
                time_in_force,
                reduce_only: plan.reduce_only,
                trigger_price: 0,
                order_expiry: plan.order_expiry,
            },
            // Emit the same canonical zero-valued integrator attribute object
            // as the official create-order signer.
            attributes: L2TxAttributes::default(),
        };
        let signer = PrivateKey::from_le_bytes_reduce(*self.private_key);
        let k = fresh_k();
        ensure!(
            !k.is_zero(),
            "CSPRNG produced an invalid zero Schnorr nonce"
        );
        let signed = sign_tx(&tx, chain_id as u32, &signer, k);
        let expected_hash = compute_tx_hash(&tx, chain_id as u32);
        ensure!(
            signed.tx_hash == expected_hash,
            "signer returned a transaction hash mismatch"
        );
        // `TxInfoJson::create_order` already matches the official signer wire
        // shape, including the mandatory zero-valued integrator attributes
        // {"1":0,"2":0,"3":0}. Do not rewrite them to null: Robinhood's
        // sendTx decoder rejects that legacy shape with HTTP 400.
        let tx_info = TxInfoJson::create_order(&tx, &signed);
        Ok(SignedLighterTransaction {
            tx_type: 14,
            tx_info,
            tx_hash: Some(signed.tx_hash_hex()),
        })
    }
}

/// Single-flight nonce state. Any ambiguous or rejected submission poisons the
/// owner until it is refreshed from the venue, preventing nonce guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LighterNonceOwner {
    account_index: i64,
    api_key_index: u8,
    next_nonce: i64,
    in_flight: Option<i64>,
    refresh_required: bool,
}

impl LighterNonceOwner {
    pub fn from_venue(account_index: i64, api_key_index: u8, next_nonce: i64) -> Result<Self> {
        ensure!(account_index >= 0, "account_index must be non-negative");
        ensure!(
            api_key_index <= 254,
            "api_key_index must be between 0 and 254"
        );
        ensure!(next_nonce >= 0, "next_nonce must be non-negative");
        Ok(Self {
            account_index,
            api_key_index,
            next_nonce,
            in_flight: None,
            refresh_required: false,
        })
    }

    pub fn reserve(&mut self) -> Result<i64> {
        ensure!(!self.refresh_required, "nonce owner requires venue refresh");
        ensure!(
            self.in_flight.is_none(),
            "nonce owner already has an in-flight transaction"
        );
        let nonce = self.next_nonce;
        self.in_flight = Some(nonce);
        Ok(nonce)
    }

    pub fn acknowledge_accepted(&mut self, nonce: i64) -> Result<()> {
        self.ensure_in_flight(nonce)?;
        self.next_nonce = nonce.checked_add(1).context("nonce overflow")?;
        self.in_flight = None;
        Ok(())
    }

    /// Releases a nonce only when no bytes could have reached the venue.
    pub fn acknowledge_pre_submit_failure(&mut self, nonce: i64) -> Result<()> {
        self.ensure_in_flight(nonce)?;
        self.in_flight = None;
        Ok(())
    }

    /// Used for transport ambiguity or venue rejection. Both require a fresh
    /// `nextNonce` snapshot before another transaction can be signed.
    pub fn require_refresh(&mut self, nonce: i64) -> Result<()> {
        self.ensure_in_flight(nonce)?;
        self.in_flight = None;
        self.refresh_required = true;
        Ok(())
    }

    pub fn refresh(&mut self, venue_next_nonce: i64) -> Result<()> {
        ensure!(
            self.in_flight.is_none(),
            "cannot refresh with a transaction in flight"
        );
        ensure!(
            venue_next_nonce >= 0,
            "venue next nonce must be non-negative"
        );
        self.next_nonce = venue_next_nonce;
        self.refresh_required = false;
        Ok(())
    }

    pub fn next_nonce(&self) -> i64 {
        self.next_nonce
    }

    pub fn refresh_required(&self) -> bool {
        self.refresh_required
    }

    fn ensure_in_flight(&self, nonce: i64) -> Result<()> {
        ensure!(
            self.in_flight == Some(nonce),
            "nonce {nonce} is not the current in-flight nonce"
        );
        Ok(())
    }
}

/// Advisory OS lock guaranteeing one local nonce owner for one API key. The
/// lock is released by the kernel if the process crashes.
#[derive(Debug)]
pub struct LighterNonceProcessGuard {
    file: File,
    path: PathBuf,
}

impl LighterNonceProcessGuard {
    pub fn acquire(state_dir: &Path, account_index: i64, api_key_index: u8) -> Result<Self> {
        ensure!(account_index >= 0, "account_index must be non-negative");
        ensure!(
            api_key_index <= 254,
            "api_key_index must be between 0 and 254"
        );
        fs::create_dir_all(state_dir).with_context(|| {
            format!(
                "failed to create Lighter state directory {}",
                state_dir.display()
            )
        })?;
        let path = state_dir.join(format!("nonce-{account_index}-{api_key_index}.lock"));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open nonce lock {}", path.display()))?;
        file.try_lock().with_context(|| {
            format!(
                "nonce key ({account_index},{api_key_index}) is already owned by another process"
            )
        })?;
        file.set_len(0)
            .context("failed to truncate nonce lock metadata")?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "pid={}\n", std::process::id())?;
        file.sync_data()
            .context("failed to sync nonce lock metadata")?;
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LighterNonceProcessGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LighterCopyRecordStatus {
    Planned,
    Signed,
    Submitted,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Ambiguous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterCopyRecord {
    pub source_event_id: String,
    pub exchange_time_ms: i64,
    pub client_order_index: i64,
    pub status: LighterCopyRecordStatus,
    #[serde(default)]
    pub order_plan: Option<LighterOrderPlan>,
    #[serde(default)]
    pub nonce: Option<i64>,
    #[serde(default)]
    pub tx_hash: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub filled_base_size: f64,
    #[serde(default)]
    pub observed_trade_ids: Vec<i64>,
    #[serde(default)]
    pub observed_trade_base_size: f64,
    #[serde(default)]
    pub last_exchange_time_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterCopyLedger {
    pub schema_version: u32,
    pub leader_account_index: i64,
    #[serde(default = "missing_account_index")]
    pub target_account_index: i64,
    pub market_index: i32,
    pub watermark_ms: i64,
    pub saved_at_ms: i64,
    pub records: Vec<LighterCopyRecord>,
    #[serde(skip)]
    seen_event_ids: HashSet<String>,
}

impl LighterCopyLedger {
    pub fn new(leader_account_index: i64, market_index: i32, seed_watermark_ms: i64) -> Self {
        Self {
            schema_version: COPY_LEDGER_SCHEMA_VERSION,
            leader_account_index,
            target_account_index: -1,
            market_index,
            watermark_ms: seed_watermark_ms,
            saved_at_ms: 0,
            records: Vec::new(),
            seen_event_ids: HashSet::new(),
        }
    }

    pub fn new_scoped(
        leader_account_index: i64,
        target_account_index: i64,
        market_index: i32,
        seed_watermark_ms: i64,
    ) -> Result<Self> {
        ensure!(
            target_account_index >= 0,
            "target account index must be non-negative"
        );
        let mut ledger = Self::new(leader_account_index, market_index, seed_watermark_ms);
        ledger.target_account_index = target_account_index;
        Ok(ledger)
    }

    pub fn load_or_new(
        path: &Path,
        leader_account_index: i64,
        market_index: i32,
        seed_watermark_ms: i64,
    ) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new(
                leader_account_index,
                market_index,
                seed_watermark_ms,
            ));
        }
        let raw = fs::read(path)
            .with_context(|| format!("failed to read Lighter copy ledger {}", path.display()))?;
        let mut ledger: Self = serde_json::from_slice(&raw)
            .with_context(|| format!("failed to parse Lighter copy ledger {}", path.display()))?;
        ensure!(
            ledger.schema_version == COPY_LEDGER_SCHEMA_VERSION,
            "unsupported Lighter copy ledger schema {}",
            ledger.schema_version
        );
        ensure!(
            ledger.leader_account_index == leader_account_index
                && ledger.market_index == market_index,
            "copy ledger belongs to a different leader or market"
        );
        ledger.rebuild_index()?;
        Ok(ledger)
    }

    pub fn load_or_new_scoped(
        path: &Path,
        leader_account_index: i64,
        target_account_index: i64,
        market_index: i32,
        seed_watermark_ms: i64,
    ) -> Result<Self> {
        if !path.exists() {
            return Self::new_scoped(
                leader_account_index,
                target_account_index,
                market_index,
                seed_watermark_ms,
            );
        }
        let ledger =
            Self::load_or_new(path, leader_account_index, market_index, seed_watermark_ms)?;
        ensure!(
            ledger.target_account_index == target_account_index,
            "copy ledger belongs to a different target account"
        );
        Ok(ledger)
    }

    pub fn should_process(&self, event_id: &str, exchange_time_ms: i64) -> bool {
        exchange_time_ms >= self.watermark_ms && !self.seen_event_ids.contains(event_id)
    }

    pub fn unresolved_records(&self) -> impl Iterator<Item = &LighterCopyRecord> {
        self.records.iter().filter(|record| {
            matches!(
                record.status,
                LighterCopyRecordStatus::Planned
                    | LighterCopyRecordStatus::Signed
                    | LighterCopyRecordStatus::Submitted
                    | LighterCopyRecordStatus::Open
                    | LighterCopyRecordStatus::PartiallyFilled
                    | LighterCopyRecordStatus::Ambiguous
            )
        })
    }

    pub fn owned_signed_base_size(&self) -> Result<f64> {
        let mut signed_base_size = 0.0_f64;
        for record in &self.records {
            let Some(plan) = record.order_plan.as_ref() else {
                continue;
            };
            ensure!(
                plan.market_index == self.market_index,
                "copy ledger plan market does not match ledger scope"
            );
            let filled = if record.filled_base_size > 0.0 {
                record.filled_base_size.min(plan.base_size)
            } else if record.status == LighterCopyRecordStatus::Filled {
                plan.base_size
            } else {
                0.0
            };
            signed_base_size += match plan.side {
                crate::lighter::LighterSide::Buy => filled,
                crate::lighter::LighterSide::Sell => -filled,
            };
        }
        ensure!(
            signed_base_size.is_finite(),
            "copy ledger owned base size is not finite"
        );
        Ok(signed_base_size)
    }

    pub fn record_plan(
        &mut self,
        source_event_id: String,
        exchange_time_ms: i64,
        plan: LighterOrderPlan,
    ) -> Result<usize> {
        ensure!(
            !source_event_id.trim().is_empty(),
            "source event id is required"
        );
        ensure!(
            self.should_process(&source_event_id, exchange_time_ms),
            "copy event is stale or already recorded"
        );
        self.watermark_ms = self.watermark_ms.max(exchange_time_ms);
        self.seen_event_ids.insert(source_event_id.clone());
        self.records.push(LighterCopyRecord {
            source_event_id,
            exchange_time_ms,
            client_order_index: plan.client_order_index,
            status: LighterCopyRecordStatus::Planned,
            order_plan: Some(plan),
            nonce: None,
            tx_hash: None,
            detail: None,
            filled_base_size: 0.0,
            observed_trade_ids: Vec::new(),
            observed_trade_base_size: 0.0,
            last_exchange_time_ms: None,
        });
        Ok(self.records.len() - 1)
    }

    pub fn record_rejected_event(
        &mut self,
        source_event_id: String,
        exchange_time_ms: i64,
        detail: String,
    ) -> Result<usize> {
        ensure!(
            !source_event_id.trim().is_empty(),
            "source event id is required"
        );
        ensure!(
            self.should_process(&source_event_id, exchange_time_ms),
            "copy event is stale or already recorded"
        );
        self.watermark_ms = self.watermark_ms.max(exchange_time_ms);
        self.seen_event_ids.insert(source_event_id.clone());
        self.records.push(LighterCopyRecord {
            source_event_id,
            exchange_time_ms,
            client_order_index: 0,
            status: LighterCopyRecordStatus::Rejected,
            order_plan: None,
            nonce: None,
            tx_hash: None,
            detail: Some(sanitize_detail(&detail)),
            filled_base_size: 0.0,
            observed_trade_ids: Vec::new(),
            observed_trade_base_size: 0.0,
            last_exchange_time_ms: None,
        });
        Ok(self.records.len() - 1)
    }

    pub fn mark_signed(&mut self, index: usize, nonce: i64, tx_hash: String) -> Result<()> {
        let record = self.record_mut(index)?;
        ensure!(
            record.status == LighterCopyRecordStatus::Planned,
            "record is not planned"
        );
        record.status = LighterCopyRecordStatus::Signed;
        record.nonce = Some(nonce);
        record.tx_hash = Some(tx_hash);
        Ok(())
    }

    /// A `planned` record is durably saved before a nonce is reserved or a
    /// signature exists. On restart it can therefore be abandoned without a
    /// venue query; every later state must be reconciled remotely.
    pub fn abandon_unsubmitted_plans(&mut self) -> usize {
        let mut changed = 0;
        for record in &mut self.records {
            if record.status == LighterCopyRecordStatus::Planned
                && record.nonce.is_none()
                && record.tx_hash.is_none()
            {
                record.status = LighterCopyRecordStatus::Rejected;
                record.detail = Some("restart abandoned pre-sign plan".to_string());
                changed += 1;
            }
        }
        changed
    }

    /// Applies one authenticated order or fill observation. Returns whether a
    /// matching ledger record changed. Position observations are consumed by
    /// exposure monitoring and do not mutate an order record.
    pub fn apply_account_observation(
        &mut self,
        observation: &LighterAccountObservation,
    ) -> Result<bool> {
        let (client_order_index, market_index) = match observation {
            LighterAccountObservation::Order(order) => {
                (order.client_order_index, order.market_index)
            }
            LighterAccountObservation::Fill(fill) => (fill.client_order_index, fill.market_index),
            LighterAccountObservation::Ready { .. } | LighterAccountObservation::Position(_) => {
                return Ok(false);
            }
        };
        let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.client_order_index == client_order_index)
        else {
            return Ok(false);
        };
        let plan = record
            .order_plan
            .as_ref()
            .context("matched Lighter ledger record has no order plan")?;
        ensure!(
            plan.market_index == market_index,
            "remote order market does not match durable plan"
        );
        ensure!(
            !matches!(
                record.status,
                LighterCopyRecordStatus::Planned | LighterCopyRecordStatus::Rejected
            ),
            "remote observation matched an order that was never submitted"
        );

        let previous = record.status;
        match observation {
            LighterAccountObservation::Order(order) => {
                record.filled_base_size = record.filled_base_size.max(order.filled_base_size);
                record.last_exchange_time_ms = Some(order.exchange_time_ms);
                record.detail = Some(sanitize_detail(&order.venue_status));
                let observed_status = match order.state {
                    LighterRemoteOrderState::Open => LighterCopyRecordStatus::Open,
                    LighterRemoteOrderState::PartiallyFilled => {
                        LighterCopyRecordStatus::PartiallyFilled
                    }
                    LighterRemoteOrderState::Filled => LighterCopyRecordStatus::Filled,
                    LighterRemoteOrderState::Cancelled => LighterCopyRecordStatus::Cancelled,
                };
                record.status = match (previous, observed_status) {
                    (LighterCopyRecordStatus::Filled, _) => LighterCopyRecordStatus::Filled,
                    (LighterCopyRecordStatus::Cancelled, LighterCopyRecordStatus::Filled) => {
                        LighterCopyRecordStatus::Filled
                    }
                    (LighterCopyRecordStatus::Cancelled, _) => LighterCopyRecordStatus::Cancelled,
                    (_, status) => status,
                };
            }
            LighterAccountObservation::Fill(fill) => {
                if record.observed_trade_ids.contains(&fill.trade_id) {
                    return Ok(false);
                }
                record.observed_trade_ids.push(fill.trade_id);
                record.observed_trade_base_size += fill.base_size;
                record.filled_base_size =
                    record.filled_base_size.max(record.observed_trade_base_size);
                record.last_exchange_time_ms = Some(fill.exchange_time_ms);
                let complete = record.filled_base_size + 1e-12 >= plan.base_size;
                record.status = if complete {
                    LighterCopyRecordStatus::Filled
                } else if previous == LighterCopyRecordStatus::Cancelled {
                    LighterCopyRecordStatus::Cancelled
                } else {
                    LighterCopyRecordStatus::PartiallyFilled
                };
                record.detail = Some(format!("observed fill {}", fill.trade_id));
            }
            LighterAccountObservation::Ready { .. } | LighterAccountObservation::Position(_) => {
                unreachable!()
            }
        }
        Ok(previous != record.status || matches!(observation, LighterAccountObservation::Fill(_)))
    }

    pub fn mark_terminal(
        &mut self,
        index: usize,
        status: LighterCopyRecordStatus,
        detail: Option<String>,
    ) -> Result<()> {
        ensure!(
            matches!(
                status,
                LighterCopyRecordStatus::Submitted
                    | LighterCopyRecordStatus::Rejected
                    | LighterCopyRecordStatus::Ambiguous
            ),
            "terminal status is required"
        );
        let record = self.record_mut(index)?;
        ensure!(
            record.status == LighterCopyRecordStatus::Signed,
            "record is not signed"
        );
        record.status = status;
        record.detail = detail.map(|value| sanitize_detail(&value));
        Ok(())
    }

    pub fn save(&mut self, path: &Path, now_ms: i64) -> Result<()> {
        self.saved_at_ms = now_ms;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create copy ledger directory {}",
                    parent.display()
                )
            })?;
        }
        let encoded = serde_json::to_vec_pretty(self).context("failed to encode copy ledger")?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)
                .with_context(|| format!("failed to open {}", temporary.display()))?;
            file.write_all(&encoded)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to atomically replace Lighter copy ledger {}",
                path.display()
            )
        })?;
        Ok(())
    }

    fn record_mut(&mut self, index: usize) -> Result<&mut LighterCopyRecord> {
        self.records
            .get_mut(index)
            .with_context(|| format!("copy record index {index} does not exist"))
    }

    fn rebuild_index(&mut self) -> Result<()> {
        self.seen_event_ids.clear();
        let mut client_order_ids = HashSet::new();
        for record in &self.records {
            ensure!(
                self.seen_event_ids.insert(record.source_event_id.clone()),
                "copy ledger contains duplicate source event {}",
                record.source_event_id
            );
            if record.client_order_index != 0 {
                ensure!(
                    client_order_ids.insert(record.client_order_index),
                    "copy ledger contains duplicate client order index {}",
                    record.client_order_index
                );
            }
        }
        Ok(())
    }
}

fn sanitize_detail(value: &str) -> String {
    let one_line = value.replace(['\r', '\n'], " ");
    one_line.chars().take(512).collect()
}

fn decode_fixed_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N]> {
    let trimmed = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    ensure!(
        trimmed.len() == N * 2,
        "{label} must contain exactly {N} bytes"
    );
    let mut output = [0u8; N];
    for (index, slot) in output.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = u8::from_str_radix(&trimmed[offset..offset + 2], 16)
            .with_context(|| format!("{label} contains non-hex characters"))?;
    }
    Ok(output)
}

fn encode_hex_prefixed(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2 + 2);
    encoded.push_str("0x");
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lighter::{LighterOrderKind, LighterSide};
    use crate::lighter_reconcile::{LighterFillObservation, LighterOrderObservation};

    fn oracle_plan() -> LighterOrderPlan {
        LighterOrderPlan {
            symbol: "BTC".to_string(),
            market_index: 0,
            side: LighterSide::Sell,
            kind: LighterOrderKind::Limit,
            reference_price: 40_500.0,
            limit_price: 40_500.0,
            requested_notional_usd: 40.5,
            planned_notional_usd: 40.5,
            base_size: 0.001,
            base_amount: 1_000,
            price: 405_000,
            is_ask: 1,
            order_type: 0,
            time_in_force: 1,
            order_expiry: 1_735_689_600_000,
            size_decimals: 6,
            price_decimals: 1,
            reduce_only: false,
            client_order_index: 123,
        }
    }

    #[test]
    fn signer_matches_official_create_order_hash_vector() {
        let credential = LighterApiCredential::from_hex(
            12_345,
            5,
            "0b8e0f63c24d8baacd9d29ad4e9a4b73c4a8d2bb8b16dc4fa9d7c2e1d3a8b1f0e8d3a4c5b6e7f001",
        )
        .unwrap();
        let signed = credential
            .sign_order(&oracle_plan(), 300, 0, 1_777_809_907_005)
            .unwrap();
        assert_eq!(signed.tx_type, 14);
        assert_eq!(
            signed.tx_hash.as_deref(),
            Some(
                "32b8a053dbfef46e8940c2aa8f58328d58eb2396a3bf6fc2bc9017ed2af874fbe15c04242a350e0e"
            )
        );
        signed.validate().unwrap();
        let info: serde_json::Value = serde_json::from_str(&signed.tx_info).unwrap();
        assert_eq!(info["AccountIndex"], 12_345);
        assert_eq!(info["ApiKeyIndex"], 5);
        assert_eq!(info["MarketIndex"], 0);
        assert_eq!(info["L2TxAttributes"]["1"], 0);
        assert_eq!(info["L2TxAttributes"]["2"], 0);
        assert_eq!(info["L2TxAttributes"]["3"], 0);
        assert!(info["Sig"].as_str().unwrap().len() > 100);
    }

    #[test]
    fn signer_matches_official_robinhood_chain_hash_vector() {
        let credential = LighterApiCredential::from_hex(
            12_345,
            5,
            "0b8e0f63c24d8baacd9d29ad4e9a4b73c4a8d2bb8b16dc4fa9d7c2e1d3a8b1f0e8d3a4c5b6e7f001",
        )
        .unwrap();
        let signed = credential
            .sign_order(&oracle_plan(), 466_324, 0, 1_787_323_188_335)
            .unwrap();
        assert_eq!(
            signed.tx_hash.as_deref(),
            Some(
                "66a545f746ed12ce4011974e9892311b2879060df9488efa8a92eb461fa97076a0cb5cc7ec47e411"
            )
        );
        let info: serde_json::Value = serde_json::from_str(&signed.tx_info).unwrap();
        assert_eq!(info["L2TxAttributes"]["1"], 0);
        assert_eq!(info["L2TxAttributes"]["2"], 0);
        assert_eq!(info["L2TxAttributes"]["3"], 0);
        signed.validate().unwrap();
    }

    #[test]
    fn auth_token_is_scoped_and_redacted_by_owner() {
        let credential = LighterApiCredential::from_hex(
            12_345,
            5,
            "0b8e0f63c24d8baacd9d29ad4e9a4b73c4a8d2bb8b16dc4fa9d7c2e1d3a8b1f0e8d3a4c5b6e7f001",
        )
        .unwrap();
        let token = credential.auth_token(600).unwrap();
        let parts = token.split(':').collect::<Vec<_>>();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[1], "12345");
        assert_eq!(parts[2], "5");
        assert_eq!(parts[3].len(), 160);
        assert!(credential.auth_token(59).is_err());
        assert!(credential.auth_token(8 * 60 * 60 + 1).is_err());
        assert!(!format!("{credential:?}").contains(parts[3]));
        credential
            .verify_registered_public_key(&credential.public_key_hex().to_ascii_uppercase())
            .unwrap();
        assert!(credential.verify_registered_public_key("0x01").is_err());
    }

    #[test]
    fn nonce_owner_is_single_flight_and_refreshes_after_ambiguity() {
        let mut owner = LighterNonceOwner::from_venue(7, 3, 41).unwrap();
        assert_eq!(owner.reserve().unwrap(), 41);
        assert!(owner.reserve().is_err());
        owner.require_refresh(41).unwrap();
        assert!(owner.reserve().is_err());
        owner.refresh(42).unwrap();
        assert_eq!(owner.reserve().unwrap(), 42);
        owner.acknowledge_accepted(42).unwrap();
        assert_eq!(owner.next_nonce(), 43);
    }

    #[test]
    fn pre_submit_failure_reuses_nonce() {
        let mut owner = LighterNonceOwner::from_venue(7, 3, 10).unwrap();
        let nonce = owner.reserve().unwrap();
        owner.acknowledge_pre_submit_failure(nonce).unwrap();
        assert_eq!(owner.reserve().unwrap(), nonce);
    }

    #[test]
    fn copy_ledger_rejects_replay_and_recovers_indexes() {
        let dir = std::env::temp_dir().join(format!(
            "lighter-copy-ledger-{}-{}",
            std::process::id(),
            crate::domain::now_ms()
        ));
        let path = dir.join("state.json");
        let mut ledger = LighterCopyLedger::new(9, 1, 100);
        let index = ledger
            .record_plan("event-1".to_string(), 101, oracle_plan())
            .unwrap();
        ledger.mark_signed(index, 12, "hash".to_string()).unwrap();
        ledger
            .mark_terminal(index, LighterCopyRecordStatus::Submitted, None)
            .unwrap();
        ledger.save(&path, 102).unwrap();

        let recovered = LighterCopyLedger::load_or_new(&path, 9, 1, 999).unwrap();
        assert!(!recovered.should_process("event-1", 101));
        assert!(!recovered.should_process("event-2", 100));
        assert!(recovered.should_process("event-2", 101));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scoped_copy_ledger_binds_target_and_recovers_owned_position() {
        let dir = std::env::temp_dir().join(format!(
            "lighter-scoped-copy-ledger-{}-{}",
            std::process::id(),
            crate::domain::now_ms()
        ));
        let path = dir.join("state.json");
        let mut ledger = LighterCopyLedger::new_scoped(9, 17_390, 0, 100).unwrap();
        let index = ledger
            .record_plan("event-1".to_string(), 101, oracle_plan())
            .unwrap();
        ledger.mark_signed(index, 12, "hash".to_string()).unwrap();
        ledger
            .mark_terminal(index, LighterCopyRecordStatus::Submitted, None)
            .unwrap();
        let fill = LighterAccountObservation::Fill(LighterFillObservation {
            trade_id: 1,
            client_order_index: 123,
            market_index: 0,
            base_size: 0.001,
            exchange_time_ms: 200,
        });
        ledger.apply_account_observation(&fill).unwrap();
        ledger.save(&path, 201).unwrap();

        let recovered = LighterCopyLedger::load_or_new_scoped(&path, 9, 17_390, 0, 999).unwrap();
        assert_eq!(recovered.owned_signed_base_size().unwrap(), -0.001);
        assert!(LighterCopyLedger::load_or_new_scoped(&path, 9, 17_391, 0, 999).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ledger_reconciles_partial_and_complete_fills_idempotently() {
        let mut ledger = LighterCopyLedger::new(9, 0, 100);
        let index = ledger
            .record_plan("event-1".to_string(), 101, oracle_plan())
            .unwrap();
        ledger.mark_signed(index, 12, "hash".to_string()).unwrap();
        ledger
            .mark_terminal(index, LighterCopyRecordStatus::Submitted, None)
            .unwrap();
        let first = LighterAccountObservation::Fill(LighterFillObservation {
            trade_id: 1,
            client_order_index: 123,
            market_index: 0,
            base_size: 0.0004,
            exchange_time_ms: 200,
        });
        assert!(ledger.apply_account_observation(&first).unwrap());
        assert_eq!(
            ledger.records[index].status,
            LighterCopyRecordStatus::PartiallyFilled
        );
        assert!(!ledger.apply_account_observation(&first).unwrap());
        assert_eq!(ledger.records[index].filled_base_size, 0.0004);

        let second = LighterAccountObservation::Fill(LighterFillObservation {
            trade_id: 2,
            client_order_index: 123,
            market_index: 0,
            base_size: 0.0006,
            exchange_time_ms: 201,
        });
        assert!(ledger.apply_account_observation(&second).unwrap());
        assert_eq!(
            ledger.records[index].status,
            LighterCopyRecordStatus::Filled
        );
        assert_eq!(ledger.records[index].observed_trade_ids, vec![1, 2]);
        assert_eq!(ledger.unresolved_records().count(), 0);
    }

    #[test]
    fn ledger_abandons_only_unsigned_restart_plans_and_accepts_cancel() {
        let mut ledger = LighterCopyLedger::new(9, 0, 100);
        let abandoned = ledger
            .record_plan("event-1".to_string(), 101, oracle_plan())
            .unwrap();
        assert_eq!(ledger.abandon_unsubmitted_plans(), 1);
        assert_eq!(
            ledger.records[abandoned].status,
            LighterCopyRecordStatus::Rejected
        );

        let mut second_plan = oracle_plan();
        second_plan.client_order_index = 124;
        let signed = ledger
            .record_plan("event-2".to_string(), 102, second_plan)
            .unwrap();
        ledger
            .mark_signed(signed, 13, "hash-2".to_string())
            .unwrap();
        assert_eq!(ledger.abandon_unsubmitted_plans(), 0);
        assert_eq!(ledger.unresolved_records().count(), 1);
        let cancelled = LighterAccountObservation::Order(LighterOrderObservation {
            client_order_index: 124,
            market_index: 0,
            state: LighterRemoteOrderState::Cancelled,
            filled_base_size: 0.0,
            remaining_base_size: 0.001,
            exchange_time_ms: 300,
            venue_status: "canceled-too-much-slippage".to_string(),
        });
        assert!(ledger.apply_account_observation(&cancelled).unwrap());
        assert_eq!(
            ledger.records[signed].status,
            LighterCopyRecordStatus::Cancelled
        );
        assert_eq!(ledger.unresolved_records().count(), 0);
    }

    #[test]
    fn process_guard_excludes_second_owner() {
        let dir = std::env::temp_dir().join(format!(
            "lighter-nonce-lock-{}-{}",
            std::process::id(),
            crate::domain::now_ms()
        ));
        let first = LighterNonceProcessGuard::acquire(&dir, 12, 3).unwrap();
        assert!(LighterNonceProcessGuard::acquire(&dir, 12, 3).is_err());
        drop(first);
        LighterNonceProcessGuard::acquire(&dir, 12, 3).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
