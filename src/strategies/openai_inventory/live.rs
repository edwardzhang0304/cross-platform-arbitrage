//! Production venue adapters. Constructed only by the live-enabled control-plane bootstrap.
use super::{
    venue::{BoxFuture, VenueBackend},
    *,
};
use crate::{
    hyperliquid,
    lighter::{
        LighterClient, LighterEnvironment, LighterExactBaseOrderRequest, LighterMarket,
        LighterOrderKind, LighterSide, build_exact_base_order_plan_with_reference,
        build_exact_base_close_plan_with_reference,
    },
    lighter_reconcile::{LighterAccountObservation, LighterOrderObservation, LighterRemoteOrderState, parse_rest_orders},
    lighter_runtime::{
        LIGHTER_NONCE_LOCK_DIR, LighterApiCredential, LighterNonceOwner, LighterNonceProcessGuard,
    },
    secrets::{ApiWalletSecret, LighterApiKeySecret},
    ws_post::WsPostClient,
};
use anyhow::{Context, Result, ensure};
use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    path::Path,
    str::FromStr,
    sync::Arc,
};

fn decimal(v: &Value) -> Result<Decimal> {
    Decimal::from_str(
        v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string())
            .as_str(),
    )
    .context("missing/invalid decimal field")
}
fn integer(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}
/// Account reads remain available with a bad clock; signing requires fresh server time.
fn verify_submission_clock(evidence: Option<(u64, u64, u64)>, now: u64) -> Result<()> {
    let (sent, server, received) = evidence.context("missing exchange clock evidence; sync system time before trading")?;
    ensure!(sent > 0 && server > 0 && received >= sent && received - sent <= 5_000
        && server >= sent.saturating_sub(5_000) && server <= received.saturating_add(5_000)
        && now >= received && now - received <= 15_000,
        "system clock differs from exchange or time evidence is stale; sync system time before trading");
    Ok(())
}

fn lighter_account_equity(account: &Value) -> Result<Decimal> {
    // REST collateral excludes margin allocated to isolated positions. Use the
    // venue's total valuation instead of reconstructing equity from collateral.
    // The WebSocket adapter maps user_stats.portfolio_value to `equity`.
    if let Some(total) = account.get("total_asset_value") {
        return decimal(total).context("invalid Lighter total_asset_value");
    }
    decimal(account.get("equity").context("missing Lighter account equity")?)
        .context("invalid Lighter portfolio value")
}
fn lighter_leverage(v: &Value) -> Result<u32> {
    let reported = decimal(v)?;
    ensure!(reported > Decimal::ZERO, "missing leverage evidence");
    // Lighter deployments have returned IMR as a fraction (0.3334), a
    // percentage (33.34), and basis points (3334). Normalize all documented
    // wire representations before deriving leverage.
    let fraction = if reported <= Decimal::ONE {
        reported
    } else if reported <= Decimal::from(100) {
        reported / Decimal::from(100)
    } else {
        ensure!(reported <= Decimal::from(10_000), "invalid margin fraction");
        reported / Decimal::from(10_000)
    };
    ensure!(
        fraction > Decimal::ZERO && fraction <= Decimal::ONE,
        "invalid margin fraction"
    );
    (Decimal::ONE / fraction)
        .round()
        .to_u32()
        .context("leverage overflow")
}

/// In unified account mode USDC collateral lives in spot state; the io DEX
/// clearinghouse summary is not the unified balance, including while a position
/// has nonzero isolated margin. Active asset data caps the
/// spendable amount after venue-level risk checks, so use the smaller value.
fn entropy_collateral(
    abstraction: &str,
    dex_equity: Decimal,
    dex_margin_used: Decimal,
    spot: &hyperliquid::SpotClearinghouseState,
    active: &Value,
) -> Result<(Decimal, Decimal)> {
    let available = active["availableToTrade"].as_array()
        .context("missing io:OAI availableToTrade")?;
    ensure!(available.len() == 2, "invalid io:OAI availableToTrade");
    let venue_available = available.iter()
        .map(decimal)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min()
        .context("missing io:OAI trade capacity")?;
    ensure!(venue_available >= Decimal::ZERO, "negative io:OAI trade capacity");
    match abstraction {
        "unifiedAccount" => {},
        "disabled" | "default" | "dexAbstraction" =>
            return Ok((dex_equity, (dex_equity - dex_margin_used).min(venue_available))),
        _ => anyhow::bail!("unsupported account abstraction mode for collateral"),
    }
    let usdc = spot.balances.iter().find(|balance| balance.coin == "USDC")
        .context("missing unified USDC balance")?;
    let total = Decimal::from_str(&usdc.total)?;
    let hold = Decimal::from_str(&usdc.hold)?;
    ensure!(total >= Decimal::ZERO && hold >= Decimal::ZERO && hold <= total,
        "invalid unified USDC balance or hold");
    Ok((total, (total - hold).min(venue_available)))
}
fn timestamp(v: &Value) -> Result<u64> {
    let t = integer(v).context("missing exchange timestamp")?;
    ensure!(t > 0, "invalid timestamp");
    Ok(if t >= 100_000_000_000_000_000 {
        t as u64 / 1_000_000
    } else if t >= 10_000_000_000_000 {
        t as u64 / 1000
    } else if t < 100_000_000_000 {
        t as u64 * 1000
    } else {
        t as u64
    })
}
fn id(r: &OrderRequest) -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, r.id.as_bytes())
}
fn client_id(r: &OrderRequest) -> i64 {
    let bytes = *id(r).as_bytes();
    let bits = if r.id.contains("-v2-") { 31 } else { 48 };
    ((u64::from_le_bytes(bytes[..8].try_into().unwrap()) & ((1u64 << bits) - 1)).max(1)) as i64
}

struct BoundLighterOrder {
    observation: LighterOrderObservation,
    order_index: i64,
    created_ms: u64,
    filled_units: i64,
}

/// A short client ID alone cannot authenticate a historical fill. Bind the
/// exchange order ID, owner, market, side, quantity, limit and reduction flag.
fn bound_lighter_order(page: &Value, config: &InventoryConfig, account: i64,
    r: &OrderRequest) -> Result<Option<BoundLighterOrder>> {
    let rows = page["orders"].as_array().context("missing RH orders array")?;
    let matches: Vec<_> = rows.iter().filter(|row|
        integer(&row["client_order_index"]).or_else(|| integer(&row["client_order_id"])) == Some(client_id(r))).collect();
    ensure!(matches.len() <= 1, "ambiguous RH client order identity");
    let Some(row) = matches.first() else { return Ok(None); };
    ensure!(integer(&row["owner_account_index"]) == Some(account)
        && integer(&row["market_index"]).or_else(|| integer(&row["market_id"])) == Some(config.market.lighter_market_id().into())
        && row["is_ask"].as_bool() == Some(r.side == Side::Sell)
        && row["reduce_only"].as_bool() == Some(r.reduce_only)
        && config.units(decimal(&row["initial_base_amount"])?)? == r.units,
        "RH order identity mismatch");
    let scale = Decimal::from(10u64.pow(config.market.lighter_price_decimals()));
    let price = if r.side == Side::Buy { (r.limit * scale).floor() / scale }
        else { (r.limit * scale).ceil() / scale };
    ensure!(decimal(&row["price"])? == price, "RH order limit mismatch");
    let created_ms = r.verified_exchange_created(timestamp(&row["created_at"])?)?;
    let order_index = integer(&row["order_index"]).or_else(|| integer(&row["order_id"]))
        .filter(|id| *id > 0).context("missing RH exchange order identity")?;
    let filled_units = config.units(decimal(&row["filled_base_amount"])?)?;
    let remaining = config.units(decimal(&row["remaining_base_amount"])?)?;
    ensure!(filled_units >= 0 && filled_units <= r.units && remaining >= 0 && remaining <= r.units - filled_units,
        "RH order quantity mismatch");
    let parsed = inventory_orders(&json!({"orders":[row]}))?;
    let LighterAccountObservation::Order(observation) = parsed.into_iter().next().context("missing RH order")?
        else { anyhow::bail!("invalid RH order evidence"); };
    Ok(Some(BoundLighterOrder { observation, order_index, created_ms, filled_units }))
}

fn lighter_exact_request(config: &InventoryConfig, r: &OrderRequest) -> LighterExactBaseOrderRequest {
    LighterExactBaseOrderRequest {
        symbol: config.market.lighter_symbol().into(),
        side: if r.side == Side::Buy { LighterSide::Buy } else { LighterSide::Sell },
        base_amount: r.units,
        size_decimals: config.market.quantity_decimals(),
        kind: LighterOrderKind::Market, limit_price: None, reduce_only: r.reduce_only,
        max_slippage_bps: 0., client_order_index: client_id(r),
    }
}

/// RH order history can contain transaction_time=0 while updated_at and
/// created_at are Unix seconds. Normalize a positive fallback before using
/// the shared order parser; never substitute the local receipt time.
fn inventory_orders(value: &Value) -> Result<Vec<LighterAccountObservation>> {
    let mut normalized = value.clone();
    for row in normalized["orders"]
        .as_array_mut()
        .context("missing RH orders array")?
    {
        let time = ["transaction_time", "updated_at", "timestamp", "created_at"]
            .into_iter()
            .filter_map(|key| row.get(key))
            .find(|v| integer(v).is_some_and(|t| t > 0));
        let at = timestamp(time.context("RH order has no positive exchange timestamp")?)?;
        row["transaction_time"] = json!(at);
    }
    parse_rest_orders(&normalized)
}
fn rejected(reason: impl ToString) -> OrderResult {
    OrderResult {
        exchange_created_ms: None,
        terminal: true,
        fills: vec![],
        reason: reason.to_string(),
    }
}

pub fn signer_guard(private_key: &str) -> Result<File> {
    let wallet: LocalWallet = private_key.parse().context("invalid Hyperliquid signer")?;
    #[cfg(not(test))]
    let root = Path::new(".codex-longrun/hyperliquid-nonce-locks").to_path_buf();
    // Independent test fixtures reuse the same public test wallet; each test still enforces ownership.
    #[cfg(test)]
    let root = std::env::temp_dir().join(format!(
        "hyperliquid-nonce-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&root)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!("{:x}.lock", wallet.address())))?;
    f.try_lock()
        .context("Hyperliquid signer is owned by another account worker")?;
    Ok(f)
}

pub async fn bootstrap(
    config: &InventoryConfig,
    lighter: LighterApiKeySecret,
    entropy: ApiWalletSecret,
    process_dry_run: bool,
) -> Result<[Box<dyn VenueBackend>; 2]> {
    ensure!(
        cfg!(feature = "openai-inventory-live") && !process_dry_run && config.mode == Mode::Live,
        "inventory live build/runtime gate closed"
    );
    build_backends(config, lighter, entropy).await
}

/// Read-only account/metadata/authentication checks. Never invokes prepare, submit, or leverage changes.
pub async fn read_only_preflight(
    config: &InventoryConfig,
    lighter: LighterApiKeySecret,
    entropy: ApiWalletSecret,
) -> Result<[AccountEvidence; 2]> {
    let [mut l, mut e] = build_backends(config, lighter, entropy).await?;
    let (l, e) = tokio::join!(l.account(), e.account());
    Ok([l?, e?])
}
async fn build_backends(
    config: &InventoryConfig,
    lighter: LighterApiKeySecret,
    entropy: ApiWalletSecret,
) -> Result<[Box<dyn VenueBackend>; 2]> {
    config.validate()?;
    config.validate_live_identity()?;
    ensure!(
        lighter.account_id == config.lighter_account
            && entropy.account_id == config.entropy_account,
        "Vault account mismatch"
    );
    ensure!(Some(lighter.account_index) == config.lighter_account_index,
        "Lighter credential account index mismatch");
    let client = LighterClient::official(LighterEnvironment::Robinhood)?;
    if let Some(address)=&config.lighter_address {
        ensure!(client.account_by_index(lighter.account_index).await?.l1_address.eq_ignore_ascii_case(address),
            "RH account belongs to a different master address");
    }
    let market = client.market_by_symbol(config.market.lighter_symbol()).await?;
        config.market.validate_lighter(&market)?;
    ensure!(
        market.is_active_perp() && market.market_id == config.market.lighter_market_id() && market.effective_size_decimals() == config.market.quantity_decimals(),
        "RH OPENAI metadata mismatch"
    );
    // Until fee-bearing RH fills have a tested account-fee field, require the actual zero-fee market.
    ensure!(
        Decimal::from_str(&market.taker_fee)?.is_zero() && config.fee_lighter.is_zero(),
        "Lighter fee contract changed"
    );
    let credential = Arc::new(LighterApiCredential::from_hex(
        lighter.account_index,
        lighter.api_key_index,
        &lighter.private_key,
    )?);
    let guard = LighterNonceProcessGuard::acquire(
        Path::new(LIGHTER_NONCE_LOCK_DIR),
        lighter.account_index,
        lighter.api_key_index,
    )?;
    credential.verify_registered_public_key(
        &client
            .api_key(lighter.account_index, lighter.api_key_index)
            .await?
            .public_key,
    )?;
    let nonce = LighterNonceOwner::from_venue(
        lighter.account_index,
        lighter.api_key_index,
        client
            .next_nonce(lighter.account_index, lighter.api_key_index)
            .await?,
    )?;
    let hlguard = signer_guard(&entropy.private_key)?;
    verify_entropy_fee_contract(config).await?;
    let meta = hyperliquid::fetch_xyz_market_snapshot_cached("mainnet", "io", 0).await?;
    let asset = meta.asset(config.market.entropy_symbol())?;
    ensure!(
        asset.meta.sz_decimals == 3 && asset.meta.margin_mode.as_deref() == Some(config.market.entropy_margin_mode()),
        "OAI margin/precision changed"
    );
    let wallet: LocalWallet = entropy
        .private_key
        .parse()
        .context("invalid Entropy API signer")?;
    let signer_address = wallet.address();
    let agents: Value = hyperliquid::info_client()?
        .post(hyperliquid::effective_info_url("mainnet")?)
        .json(&json!({"type":"extraAgents","user":config.entropy_address}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let authorized = format!("{signer_address:#x}").eq_ignore_ascii_case(&config.entropy_address)
        || agents.as_array().is_some_and(|a| {
            a.iter().any(|x| {
                x["address"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&format!("{signer_address:#x}")))
                    && integer(&x["validUntil"]).is_some_and(|t| t as u64 > crate::domain::now_ms())
            })
        });
    ensure!(
        authorized,
        "Entropy API signer is not registered to expected master account"
    );
    let exchange = ExchangeClient::new_offline_signing_with_asset_map(
        wallet,
        Some(hyperliquid::sdk_base_url("mainnet")?),
        meta.sdk_meta(),
        meta.coin_to_asset.clone(),
        None,
    )?;
    let lfeed = super::feed::AccountFeed::start_for(
        Venue::Lighter,
        client.endpoints().ws_url.clone(),
        lighter.account_index.to_string(),
        Some(credential.clone()),
        config.market,
    );
    let efeed = super::feed::AccountFeed::start_for(
        Venue::Entropy,
        "wss://api.hyperliquid.xyz/ws".into(),
        config.entropy_address.clone(),
        None,
        config.market,
    );
    Ok([
        Box::new(LighterLive {
            config: config.clone(),
            client,
            market,
            credential,
            _guard: guard,
            nonce,
            feed: lfeed,
            last_rest: 0,
            leverage_confirmed: false,
            clock_evidence: None,
        }),
        Box::new(EntropyLive {
            config: config.clone(),
            exchange,
            _guard: hlguard,
            last_nonce: 0,
            feed: efeed,
            last_rest: 0,
            leverage_confirmed: false,
            clock_evidence: None,
            abstraction: None,
            preflight_account: None,
        }),
    ])
}

struct LighterLive {
    config: InventoryConfig,
    client: LighterClient,
    market: LighterMarket,
    credential: Arc<LighterApiCredential>,
    _guard: LighterNonceProcessGuard,
    nonce: LighterNonceOwner,
    feed: super::feed::AccountFeed,
    last_rest: u64,
    leverage_confirmed: bool,
    clock_evidence: Option<(u64, u64, u64)>,
}
impl LighterLive {
    async fn inspect(&self, r: &OrderRequest) -> Result<OrderResult> {
        let auth = self.credential.auth_token(600)?;
        let cid = client_id(r);
        let active = self
            .client
            .account_active_orders(&auth, self.credential.account_index, self.market.market_id)
            .await?;
        let mut order = bound_lighter_order(&active, &self.config, self.credential.account_index, r)?;
        let mut cursor = None;
        for _ in 0..32 {
            if order.is_some() {
                break;
            }
            let page = self
                .client
                .inventory_history_page(
                    &auth,
                    self.credential.account_index,
                    self.market.market_id,
                    false,
                    cursor,
                )
                .await?;
            order = bound_lighter_order(&page, &self.config, self.credential.account_index, r)?;
            cursor = page["next_cursor"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let Some(order) = order else {
            if crate::domain::now_ms() > r.signed_expiry().saturating_add(30_000)
                && active["orders"].as_array().is_some_and(|a| a.is_empty())
            {
                let history = self
                    .client
                    .inventory_history_page(
                        &auth,
                        self.credential.account_index,
                        self.market.market_id,
                        true,
                        None,
                    )
                    .await?;
                let (server_ms, account) = self
                    .client
                    .inventory_dated_account(self.credential.account_index)
                    .await?;
                if expired_lighter_absence_for(
                    r,
                    server_ms,
                    &account,
                    &history,
                    self.credential.account_index,
                    self.market.market_id,
                )? {
                    return Ok(rejected(
                        "signed RH request expired; fresh account and trade history confirm no execution",
                    ));
                }
            }
            return Ok(OrderResult {
                exchange_created_ms: None,
                terminal: false,
                fills: vec![],
                reason: "client order id not yet found; do not resend".into(),
            });
        };
        let reported = order.filled_units;
        let mut fills = vec![];
        let mut cursor = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..32 {
            if fills.iter().map(|f: &Fill| f.units).sum::<i64>() >= reported {
                break;
            }
            let page = self
                .client
                .inventory_history_page(
                    &auth,
                    self.credential.account_index,
                    self.market.market_id,
                    true,
                    cursor,
                )
                .await?;
            for row in page["trades"].as_array().context("missing trades list")? {
                let ask = integer(&row["ask_account_id"]) == Some(self.credential.account_index);
                let bid = integer(&row["bid_account_id"]) == Some(self.credential.account_index);
                if !ask && !bid {
                    continue;
                }
                let key = if ask {
                    "ask_client_id"
                } else {
                    "bid_client_id"
                };
                if integer(&row[key]).or_else(|| integer(&row[format!("{key}_str")])) != Some(cid) {
                    continue;
                }
                let exchange_id_key = if ask { "ask_id" } else { "bid_id" };
                if integer(&row[exchange_id_key]).or_else(|| integer(&row[format!("{exchange_id_key}_str")])) != Some(order.order_index) {
                    continue;
                }
                ensure!(
                    integer(&row["market_id"]) == Some(self.market.market_id as i64),
                    "fill market mismatch"
                );
                let fid = integer(&row["trade_id"])
                    .or_else(|| integer(&row["trade_id_str"]))
                    .context("missing trade id")?
                    .to_string();
                if !seen.insert(fid.clone()) {
                    continue;
                }
                let side = if ask { Side::Sell } else { Side::Buy };
                ensure!(side == r.side, "fill side mismatch");
                ensure!(
                    integer(&row["taker_fee"]).unwrap_or(0) == 0,
                    "unexpected RH execution fee; reconcile charged costs"
                );
                fills.push(Fill {
                    id: fid,
                    order_id: r.id.clone(),
                    venue: Venue::Lighter,
                    side,
                    units: self.config.units(decimal(&row["size"])?)?,
                    price: decimal(&row["price"])?,
                    fee: Decimal::ZERO,
                    time_ms: timestamp(
                        row.get("timestamp")
                            .or_else(|| row.get("transaction_time"))
                            .context("missing fill timestamp")?,
                    )?,
                });
            }
            cursor = page["next_cursor"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let complete = fills.iter().map(|x| x.units).sum::<i64>() == reported;
        Ok(OrderResult { exchange_created_ms: Some(order.created_ms),
            terminal: complete
                && matches!(
                    order.observation.state,
                    LighterRemoteOrderState::Filled | LighterRemoteOrderState::Cancelled
                ),
            fills,
            reason: if complete {
                "authenticated RH order and fills"
            } else {
                "fill history incomplete"
            }
            .into(),
        })
    }
}
impl VenueBackend for LighterLive {
    fn prepare(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let a = self.account().await?;
            ensure!(
                a.open_orders == 0,
                "existing OPENAI orders block worker preparation"
            );
            require_isolated_setting(&a, self.config.leverage)?;
            self.leverage_confirmed = true;
            Ok(())
        })
    }
    fn reconcile_account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        self.last_rest = 0;
        self.account()
    }

    fn funding(&mut self, start: u64, end: u64) -> BoxFuture<'_, Vec<Funding>> {
        Box::pin(async move {
            let auth = self.credential.auth_token(600)?;
            let mut cursor = None;
            let mut out = vec![];
            for _ in 0..100 {
                let page = self
                    .client
                    .inventory_funding_page(
                        &auth,
                        self.credential.account_index,
                        self.market.market_id,
                        cursor,
                        start,
                    )
                    .await?;
                for row in page["position_fundings"]
                    .as_array()
                    .context("missing funding history")?
                {
                    let t = timestamp(&row["timestamp"])?;
                    if t < start || t > end {
                        continue;
                    }
                    ensure!(
                        integer(&row["market_id"]) == Some(self.market.market_id as i64),
                        "funding market mismatch"
                    );
                    out.push(Funding {
                        id: integer(&row["funding_id"])
                            .context("missing funding id")?
                            .to_string(),
                        venue: Venue::Lighter,
                        amount: funding_cashflow(row)?,
                        time_ms: t,
                    });
                }
                cursor = page["next_cursor"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                if cursor.is_none() {
                    return Ok(out);
                }
            }
            anyhow::bail!("funding history exceeds bounded pagination; reconciliation required")
        })
    }

    fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        Box::pin(async move {
            ensure!(cfg!(feature = "openai-inventory-live"), "live gate closed");
            let a = match self.reconcile_account().await {
                Ok(a) => a,
                Err(e) => return Ok(rejected(e)),
            };
            if let Err(e) = final_risk(&self.config, &a, &r) {
                return Ok(rejected(e));
            }
            if let Err(e) = verify_submission_clock(self.clock_evidence, crate::domain::now_ms()) {
                return Ok(rejected(e));
            }
            let req = lighter_exact_request(&self.config, &r);
            let scale = Decimal::from(10u64.pow(self.market.effective_price_decimals()));
            let protected = if r.side == Side::Buy {
                (r.limit * scale).floor() / scale
            } else {
                (r.limit * scale).ceil() / scale
            };
            let reference = protected.to_f64().context("price overflow")?;
            let planned = if r.reduce_only && a.position_units.checked_abs() == Some(r.units) {
                build_exact_base_close_plan_with_reference(&self.market, &req, reference, a.position_units)
            } else {
                build_exact_base_order_plan_with_reference(&self.market, &req, reference)
            };
            let plan = match planned {
                Ok(x) => x,
                Err(e) => return Ok(rejected(e)),
            };
            ensure!(plan.time_in_force == 0, "expected IOC order plan");
            let encoded = Decimal::new(plan.price as i64, plan.price_decimals);
            ensure!(
                if r.side == Side::Buy {
                    encoded <= r.limit
                } else {
                    encoded >= r.limit
                },
                "rounded price exceeds execution protection"
            );
            if self.nonce.refresh_required() {
                self.nonce.refresh(
                    self.client
                        .next_nonce(self.credential.account_index, self.credential.api_key_index)
                        .await?,
                )?;
            }
            let mut reservation = self.nonce.reservation()?;
            if crate::domain::now_ms() > r.expires_ms {
                return Ok(rejected("request expired before signing"));
            }
            let signed = self.credential.sign_order(
                &plan,
                self.client.endpoints().chain_id,
                reservation.nonce(),
                r.signed_expiry() as i64,
            )?;
            tracing::info!(request_id=%r.id,tx_hash=?signed.tx_hash,"RH signed request prepared");
            use crate::lighter_manual::{LighterSubmitDisposition, classify_submit_result};
            reservation.dispatching();
            match classify_submit_result(self.client.submit_signed_transaction(&signed).await) {
                LighterSubmitDisposition::Accepted(_) => reservation.accepted()?,
                LighterSubmitDisposition::Rejected(e) => {
                    return Ok(rejected(e));
                }
                LighterSubmitDisposition::Ambiguous(e) => {
                    return Err(e.context("RH submission uncertain; reconcile client id"));
                }
            }
            self.inspect(&r).await
        })
    }
    fn lookup(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        Box::pin(async move { self.inspect(&r).await })
    }
    fn account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        Box::pin(async move {
            let streamed = (|| -> Result<Value> {
                let (_, p) = self
                    .feed
                    .get("account_all_positions", self.config.account_max_age_ms)?;
                let (_, stats) = self
                    .feed
                    .get("user_stats", self.config.account_max_age_ms)?;
                let positions = p["positions"]
                    .as_object()
                    .context("missing position snapshot")?
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(
                    json!({"positions":positions,"available_balance":stats["stats"]["available_balance"],"equity":stats["stats"]["portfolio_value"],"collateral":stats["stats"]["collateral"]}),
                )
            })();
            let a = if self.last_rest != 0 && streamed.is_ok() {
                streamed?
            } else {
                let now = crate::domain::now_ms();
                ensure!(
                    self.last_rest == 0 || now.saturating_sub(self.last_rest) >= 15_000,
                    "account stream unavailable; bounded REST fallback cooling down"
                );
                self.last_rest = now;
                let started = crate::domain::now_ms();
                let (server_ms, data) = self.client.inventory_dated_account(self.credential.account_index).await?;
                self.clock_evidence = Some((started, server_ms, crate::domain::now_ms()));
                data["accounts"]
                    .as_array()
                    .context("missing accounts")?
                    .iter()
                    .find(|x| integer(&x["index"]) == Some(self.credential.account_index))
                    .context("RH account index mismatch")?
                    .clone()
            };
            let p = a["positions"]
                .as_array()
                .context("missing positions")?
                .iter()
                .find(|p| integer(&p["market_id"]) == Some(self.market.market_id as i64));
            let (position_units, leverage, isolated) = if let Some(p) = p {
                let q = self.config.units(decimal(&p["position"])?)?
                    * integer(&p["sign"]).context("missing position sign")?;
                (q, lighter_leverage(&p["initial_margin_fraction"])?,
                    integer(&p["margin_mode"]) == Some(1))
            } else {
                (
                    0,
                    if self.leverage_confirmed {
                        self.config.leverage
                    } else {
                        0
                    },
                    false,
                )
            };
            // Do not fabricate a leverage setting from config when the flat account lacks evidence.
            Ok(AccountEvidence {
                venue: Venue::Lighter,
                account: self.config.lighter_account.clone(),
                observed_ms: crate::domain::now_ms(),
                position_units,
                free_margin: decimal(&a["available_balance"])?,
                equity: lighter_account_equity(&a)?,
                leverage,
                isolated,
                open_orders: p
                    .map(|p| {
                        integer(&p["pending_order_count"]).unwrap_or(0)
                            + integer(&p["open_order_count"]).unwrap_or(0)
                    })
                    .unwrap_or(0) as usize,
                authenticated: true,
                liquidation_price: p.and_then(|p| decimal(&p["liquidation_price"]).ok()),
            })
        })
    }
}

fn expired_lighter_absence_for(
    r: &OrderRequest,
    server_ms: u64,
    account: &Value,
    history: &Value,
    account_index: i64,
    market_id: i32,
) -> Result<bool> {
    let now = crate::domain::now_ms();
    if r.venue != Venue::Lighter
        || server_ms <= r.signed_expiry().saturating_add(30_000)
        || now.abs_diff(server_ms) > 15_000
    {
        return Ok(false);
    }
    let accounts = account["accounts"].as_array().context("missing accounts")?;
    let a = accounts
        .iter()
        .find(|a| {
            integer(&a["account_index"]).or_else(|| integer(&a["index"])) == Some(account_index)
        })
        .context("account identity missing")?;
    let ps = a["positions"].as_array().context("missing positions")?;
    let p = ps
        .iter()
        .find(|p| integer(&p["market_id"]) == Some(i64::from(market_id)))
        .context("missing OPENAI position")?;
    if !decimal(&p["position"])?.is_zero()
        || integer(&p["open_order_count"]) != Some(0)
        || integer(&p["pending_order_count"]) != Some(0)
    {
        return Ok(false);
    }
    ensure!(
        history["code"].as_i64() == Some(200),
        "trade history rejected"
    );
    // Descending history is complete for the interval only if the page has no
    // continuation. Be conservative when there is any pagination ambiguity.
    if history["next_cursor"]
        .as_str()
        .is_some_and(|c| !c.is_empty())
    {
        return Ok(false);
    }
    for row in history["trades"].as_array().context("missing trades")? {
        let t = timestamp(
            row.get("timestamp")
                .or_else(|| row.get("transaction_time"))
                .context("missing trade time")?,
        )?;
        if t >= r.created_ms.saturating_sub(300_000) {
            return Ok(false);
        }
    }
    Ok(true)
}

struct EntropyLive {
    config: InventoryConfig,
    exchange: ExchangeClient,
    _guard: File,
    last_nonce: u64,
    feed: super::feed::AccountFeed,
    last_rest: u64,
    leverage_confirmed: bool,
    clock_evidence: Option<(u64, u64, u64)>,
    abstraction: Option<(String, u64)>,
    preflight_account: Option<AccountEvidence>,
}

/// Consume once, preserve the original observation time, and never carry
/// evidence across a submission or an order lookup.
fn take_entropy_preflight(cache: &mut Option<AccountEvidence>, c: &InventoryConfig, now: u64) -> Option<AccountEvidence> {
    cache.take().filter(|a| a.venue == Venue::Entropy && a.account == c.entropy_account
        && a.authenticated && a.observed_ms <= now && now - a.observed_ms <= c.account_max_age_ms)
}

fn entropy_price(r: &OrderRequest) -> Result<Decimal> {
    let magnitude = r.limit.to_f64().context("invalid price")?.log10().floor() as i32;
    let scale = Decimal::from(10u64.pow((4 - magnitude).clamp(0, 3) as u32));
    Ok(if r.side == Side::Buy { (r.limit * scale).floor() / scale }
        else { (r.limit * scale).ceil() / scale })
}

fn bound_entropy_created(config: &InventoryConfig, r: &OrderRequest,
    order: &hyperliquid::OrderStatusInfo) -> Result<u64> {
    let cloid = format!("0x{}", id(r).simple());
    ensure!(order.order.coin == config.market.entropy_symbol()
        && order.order.oid > 0 && order.order.cloid.as_deref() == Some(cloid.as_str())
        && config.units(Decimal::from_str(&order.order.orig_sz)?)? == r.units
        && order.order.side == (if r.side == Side::Buy { "B" } else { "A" })
        && order.order.reduce_only == r.reduce_only
        && Decimal::from_str(&order.order.limit_px)? == entropy_price(r)?,
        "Entropy order identity mismatch");
    r.verified_exchange_created(order.order.timestamp)
}

fn entropy_result(config: &InventoryConfig, r: &OrderRequest,
    order: &hyperliquid::OrderStatusInfo, rows: &[hyperliquid::UserFill]) -> Result<OrderResult> {
    let created_ms = bound_entropy_created(config, r, order)?;
    let mut fills = Vec::new();
    for f in rows.iter().filter(|f| f.oid == order.order.oid && f.coin == config.market.entropy_symbol()) {
        let side = match f.side.as_str() { "B" => Side::Buy, "A" => Side::Sell,
            _ => anyhow::bail!("invalid Entropy fill side") };
        ensure!(side == r.side, "Entropy fill side mismatch");
        fills.push(Fill {
            id: hyperliquid::user_fill_identity(f), order_id: r.id.clone(), venue: Venue::Entropy,
            side, units: config.units(Decimal::from_str(&f.sz)?)?, price: Decimal::from_str(&f.px)?,
            fee: Decimal::from_str(&f.fee)?, time_ms: f.time,
        });
    }
    let terminal = matches!(order.status.as_str(), "filled" | "canceled" | "rejected")
        || order.status.ends_with("Rejected") || order.status.ends_with("Canceled");
    let remaining = config.units(Decimal::from_str(&order.order.sz)?)?;
    ensure!((0..=r.units).contains(&remaining), "invalid remaining size");
    // A filled order can never be reconciled as a zero-fill rejection.
    let expected = if order.status == "filled" { r.units } else { r.units - remaining };
    let enough = fills.iter().map(|f| f.units).sum::<i64>() == expected;
    Ok(OrderResult { exchange_created_ms: Some(created_ms), terminal: terminal && enough,
        fills, reason: if terminal && !enough { "Entropy fill history incomplete".into() } else { order.status.clone() } })
}
impl EntropyLive {
    async fn inspect(&self, r: &OrderRequest, evidence: Option<&venue::LookupEvidence>) -> Result<OrderResult> {
        hyperliquid::trading_info(true, self.inspect_inner(r, evidence)).await
    }
    async fn inspect_inner(&self, r: &OrderRequest, evidence: Option<&venue::LookupEvidence>) -> Result<OrderResult> {
        let cloid = format!("0x{}", id(r).simple());
        let status = hyperliquid::fetch_order_status_by_cloid(
            "mainnet",
            &self.config.entropy_address,
            &cloid,
        )
        .await?;
        let Some(order) = status.order else {
            if status.status == "unknownOid"
                && crate::domain::now_ms() > r.expires_ms.saturating_add(30_000)
            {
                // Unknown cloid alone is never a rejection. Bind fresh chain
                // position and complete fill history to the durable ledger.
                let (chain, orders, fills) = tokio::try_join!(
                    hyperliquid::fetch_clearinghouse_state(
                        "mainnet",
                        "io",
                        &self.config.entropy_address
                    ),
                    hyperliquid::fetch_open_orders("mainnet", "io", &self.config.entropy_address),
                    hyperliquid::fetch_user_fills_by_time_unfiltered(
                        "mainnet",
                        &self.config.entropy_address,
                        r.created_ms.saturating_sub(300_000),
                    )
                )?;
                return Ok(match entropy_absence_result(&self.config, r, evidence, &chain,
                    &orders, &fills, crate::domain::now_ms()) {
                    Ok(result) => result,
                    Err(error) => OrderResult { exchange_created_ms: None, terminal: false,
                        fills: vec![], reason: format!("Entropy absence not verified: {error}") },
                });
            }
            return Ok(OrderResult {
                exchange_created_ms: None,
                terminal: false,
                fills: vec![],
                reason: "Entropy cloid not yet found".into(),
            });
        };
        let created_ms = bound_entropy_created(&self.config, r, &order)?;
        let rows = hyperliquid::fetch_user_fills_by_time(
            "mainnet",
            "io",
            &self.config.entropy_address,
            created_ms.saturating_sub(1000),
            None,
        )
        .await?;
        entropy_result(&self.config, r, &order, &rows)
    }
}
impl VenueBackend for EntropyLive {
    fn prepare(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // Establish and validate the submission connection before a live
            // order consumes its short execution deadline. This is read-only.
            let warm=WsPostClient::for_environment("mainnet").post_info(json!({"type":"clearinghouseState","user":self.config.entropy_address,"dex":"io"})).await?;
            ensure!(
                entropy_warm_response(&warm),
                "Entropy submission channel not ready"
            );
            let a = self.account().await?;
            ensure!(
                a.open_orders == 0,
                "existing OAI orders block worker preparation"
            );
            require_isolated_setting(&a, self.config.leverage)?;
            self.leverage_confirmed = true;
            Ok(())
        })
    }
    fn reconcile_account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        self.last_rest = 0;
        Box::pin(async move { hyperliquid::trading_info(true, self.account()).await })
    }

    fn funding(&mut self, start: u64, end: u64) -> BoxFuture<'_, Vec<Funding>> {
        Box::pin(hyperliquid::trading_info(false, async move {
            let mut from = start;
            let mut out = vec![];
            for _ in 0..100 {
                let rows=hyperliquid::fetch_user_funding("mainnet",&self.config.entropy_address,from,end).await?;
                let rows = rows.as_array().context("invalid funding history")?;
                for row in rows {
                    let t = timestamp(&row["time"])?;
                    if row["delta"]["coin"] != self.config.market.entropy_symbol() || t < start || t > end {
                        continue;
                    }
                    out.push(Funding {
                        id: format!("{}:{}", t, row["hash"]),
                        venue: Venue::Entropy,
                        amount: decimal(&row["delta"]["usdc"])?,
                        time_ms: t,
                    });
                }
                if rows.len() < 500 {
                    return Ok(out);
                }
                let last = rows
                    .iter()
                    .map(|r| timestamp(&r["time"]))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .max()
                    .context("empty page")?;
                ensure!(last > from, "funding pagination stalled");
                from = last;
            }
            anyhow::bail!("funding history exceeds bounded pagination")
        }))
    }

    fn submit(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        Box::pin(async move {
            ensure!(
                cfg!(feature = "openai-inventory-live") && r.units % self.config.common_step() == 0,
                "Entropy live/precision gate"
            );
            if !r.reduce_only && self.config.quantity(r.units) * r.limit < Decimal::from(10) {
                return Ok(rejected("below Entropy minimum"));
            }
            // Perp prices: at most 6-sizeDecimals decimal places and five significant figures.
            // Reserve time for signing/dispatch. A read-only preflight timeout
            // is a proven non-submission, not an unknown exchange order.
            let remaining = r.expires_ms.saturating_sub(crate::domain::now_ms());
            let a = match take_entropy_preflight(&mut self.preflight_account, &self.config, crate::domain::now_ms()) {
                Some(a) => a,
                None => match entropy_preflight(self.reconcile_account(), remaining).await {
                    Ok(a) => { self.preflight_account = None; a },
                    Err(e) => return Ok(rejected(format!("{e:#}"))),
                },
            };
            if let Err(e) = final_risk(&self.config, &a, &r) {
                return Ok(rejected(e));
            }
            if let Err(e) = verify_submission_clock(self.clock_evidence, crate::domain::now_ms()) {
                return Ok(rejected(e));
            }
            let price = entropy_price(&r)?;
            let request = ClientOrderRequest {
                asset: self.config.market.entropy_symbol().into(),
                is_buy: r.side == Side::Buy,
                reduce_only: r.reduce_only,
                limit_px: price.to_f64().context("price overflow")?,
                sz: self.config.quantity(r.units).to_f64().context("size overflow")?,
                cloid: Some(id(&r)),
                order_type: ClientOrder::Limit(ClientLimit { tif: "Ioc".into() }),
            };
            if crate::domain::now_ms() > r.expires_ms {
                return Ok(rejected("request expired before signing"));
            }
            let nonce = crate::domain::now_ms().max(self.last_nonce + 1);
            self.last_nonce = nonce;
            let payload = self
                .exchange
                .signed_bulk_order_payload_with_grouping_nonce_and_expiry(
                    vec![request],
                    None,
                    "na",
                    nonce,
                    r.expires_ms,
                )?;
            let response = WsPostClient::for_environment("mainnet")
                .post_action(payload)
                .await?;
            if response["status"] == "err" {
                return Ok(rejected(response["response"].to_string()));
            }
            if let Some(error) = response.pointer("/response/data/statuses/0/error") {
                return Ok(rejected(error));
            }
            self.inspect(&r, None).await
        })
    }
    fn lookup(&mut self, r: OrderRequest) -> BoxFuture<'_, OrderResult> {
        self.preflight_account = None;
        Box::pin(async move { self.inspect(&r, None).await })
    }
    fn lookup_reconciled(&mut self, r: OrderRequest, evidence: venue::LookupEvidence) -> BoxFuture<'_, OrderResult> {
        self.preflight_account = None;
        Box::pin(async move { self.inspect(&r, Some(&evidence)).await })
    }
    fn account(&mut self) -> BoxFuture<'_, AccountEvidence> {
        self.preflight_account = None;
        Box::pin(hyperliquid::trading_info(false, async move {
            let now = crate::domain::now_ms();
            if self.abstraction.as_ref().is_none_or(|(_, at)| now.saturating_sub(*at) >= 60_000) {
                let mode = hyperliquid::fetch_user_abstraction("mainnet", &self.config.entropy_address).await?;
                self.abstraction = Some((mode, crate::domain::now_ms()));
            }
            let stream = (|| -> Result<(crate::hyperliquid::ClearinghouseState, usize, Value, crate::hyperliquid::SpotClearinghouseState)> {
                let (_, state) = self
                    .feed
                    .get("clearinghouseState", self.config.account_max_age_ms)?;
                let (_, orders) = self
                    .feed
                    .get("openOrders", self.config.account_max_age_ms)?;
                let (_, active) = self
                    .feed
                    .get("activeAssetData", self.config.account_max_age_ms)?;
                let (_, spot) = self
                    .feed
                    .get("spotState", self.config.account_max_age_ms)?;
                let state = state.get("clearinghouseState").unwrap_or(&state).clone();
                let spot = spot.get("spotState").unwrap_or(&spot).clone();
                let count = orders["orders"]
                    .as_array()
                    .context("missing streamed orders")?
                    .iter()
                    .filter(|o| o["coin"] == self.config.market.entropy_symbol())
                    .count();
                Ok((serde_json::from_value(state)?, count, active, serde_json::from_value(spot)?))
            })();
            let (a, order_count, active, spot, rest_started) = if self.last_rest != 0 && stream.is_ok() {
                let (a,n,active,spot)=stream?;
                (a,n,active,spot,None)
            } else {
                let now = crate::domain::now_ms();
                ensure!(
                    self.last_rest == 0 || now.saturating_sub(self.last_rest) >= 15_000,
                    "account stream unavailable; bounded REST fallback cooling down"
                );
                self.last_rest = now;
                let started = crate::domain::now_ms();
                // Independent read-only evidence used to run serially and
                // consume the entire submit preflight deadline in latency.
                let (a,orders,active,spot)=tokio::try_join!(
                    hyperliquid::fetch_clearinghouse_state("mainnet","io",&self.config.entropy_address),
                    hyperliquid::fetch_open_orders("mainnet","io",&self.config.entropy_address),
                    hyperliquid::fetch_active_asset_data("mainnet",&self.config.entropy_address,self.config.market.entropy_symbol()),
                    hyperliquid::fetch_spot_clearinghouse_state("mainnet",&self.config.entropy_address)
                )?;
                self.clock_evidence = a.time.map(|server| (started, server, crate::domain::now_ms()));
                (a, orders.iter().filter(|o| o.coin == self.config.market.entropy_symbol()).count(), active, spot, Some(started))
            };
            let p = a
                .asset_positions
                .iter()
                .find(|p| p.position.coin == self.config.market.entropy_symbol());
            let (position_units, leverage, isolated) = if let Some(p) = p {
                let l = p
                    .position
                    .leverage
                    .as_ref()
                    .context("missing isolated leverage")?;
                (
                    self.config.units(Decimal::from_str(&p.position.szi)?)?,
                    l.value.context("missing leverage value")?,
                    l.leverage_type == "isolated",
                )
            } else {
                (
                    0,
                    integer(&active["leverage"]["value"])
                        .and_then(|v| u32::try_from(v).ok())
                        .context("missing io:OAI configured leverage")?,
                    active["leverage"]["type"] == "isolated",
                )
            };
            let (equity, free) = entropy_collateral(
                &self.abstraction.as_ref().context("missing account abstraction")?.0,
                Decimal::from_str(&a.margin_summary.account_value)?,
                Decimal::from_str(&a.margin_summary.total_margin_used)?,
                &spot,
                &active,
            )?;
            let evidence=AccountEvidence {
                venue: Venue::Entropy,
                account: self.config.entropy_account.clone(),
                observed_ms: rest_started.unwrap_or_else(crate::domain::now_ms),
                position_units,
                free_margin: free,
                equity,
                leverage,
                isolated,
                open_orders: order_count,
                authenticated: true,
                liquidation_price: p
                    .and_then(|p| p.position.liquidation_px.as_ref())
                    .and_then(|p| Decimal::from_str(p).ok()),
            };
            // Only a complete REST snapshot is eligible. Failed/cancelled
            // queries and stream-only updates never populate this cache.
            if rest_started.is_some() { self.preflight_account=Some(evidence.clone()); }
            Ok(evidence)
        }))
    }
}

fn require_isolated_setting(account: &AccountEvidence, leverage: u32) -> Result<()> {
    ensure!(
        account.isolated && account.leverage == leverage,
        "venue isolated leverage differs from the configured setting; change it manually before live trading"
    );
    Ok(())
}

fn final_risk(c: &InventoryConfig, a: &AccountEvidence, r: &OrderRequest) -> Result<()> {
    ensure!(
        a.authenticated && a.venue == r.venue && a.open_orders == 0,
        "final account evidence rejected"
    );
    ensure!(
        crate::domain::now_ms().saturating_sub(a.observed_ms) <= c.account_max_age_ms,
        "final account stale"
    );
    if r.reduce_only {
        ensure!(
            a.position_units.signum() != r.side.sign() && a.position_units.abs() >= r.units,
            "reduce-only exceeds actual position"
        );
    } else {
        ensure!(
            a.leverage == c.leverage && a.isolated,
            "final leverage gate"
        );
        ensure!(
            a.position_units == 0 || a.position_units.signum() == r.side.sign(),
            "unexpected opposite position"
        );
        let notional = c.quantity(r.units) * r.limit;
        ensure!(
            c.quantity(a.position_units.abs() + r.units) * r.limit <= c.max_notional_per_venue,
            "final notional cap"
        );
        let fee = if a.venue == Venue::Lighter {
            c.fee_lighter
        } else {
            c.fee_entropy
        };
        ensure!(
            a.free_margin - notional / Decimal::from(c.leverage) - notional * fee
                >= c.min_free_margin,
            "final free margin gate"
        );
    }
    Ok(())
}

fn funding_cashflow(row: &Value) -> Result<Decimal> {
    let amount = decimal(&row["change"])?.abs();
    let rate = decimal(&row["rate"])?;
    let side = match row["position_side"].as_str() {
        Some("long") => Decimal::ONE,
        Some("short") => -Decimal::ONE,
        _ => anyhow::bail!("invalid funding side"),
    };
    ensure!(
        !rate.is_zero() || amount.is_zero(),
        "nonzero funding with zero rate"
    );
    // Normalize the cash-flow direction from the documented funding formula, never from UI sign conventions.
    Ok(-side
        * if rate.is_sign_negative() {
            -amount
        } else {
            amount
        })
}

fn entropy_warm_response(v: &Value) -> bool {
    v["type"] == "clearinghouseState"
        && v.pointer("/data/time").and_then(Value::as_u64).is_some()
        && v.pointer("/data/assetPositions")
            .is_some_and(Value::is_array)
}

async fn entropy_preflight<T>(check: impl std::future::Future<Output = Result<T>>, remaining_ms: u64) -> Result<T> {
    ensure!(remaining_ms > 500, "Entropy request expired before preflight; not submitted");
    tokio::time::timeout(std::time::Duration::from_millis((remaining_ms - 500).min(4000)), check)
        .await.context("Entropy account preflight timed out; request not submitted")?
}

/// Only exact, previously authenticated fills for OTHER requests can be
/// excluded. Position equality alone, a side difference, or an empty/truncated
/// API page can never prove this request did not fill.
pub(super) fn entropy_absence_result(
    config: &InventoryConfig, r: &OrderRequest, evidence: Option<&venue::LookupEvidence>,
    chain: &hyperliquid::ClearinghouseState, orders: &[hyperliquid::OpenOrder],
    fills: &[hyperliquid::UserFill], now: u64,
) -> Result<OrderResult> {
    let market = config.market.entropy_symbol();
    ensure!(r.created_ms > 0 && r.expires_ms >= r.created_ms,
        "invalid persisted request lifetime");
    ensure!(expired_entropy_absence(r, chain.time,
        orders.iter().any(|o| o.coin == market), false),
        "signed expiry or open-order evidence incomplete");
    let chain_time = chain.time.context("missing chain time")?;
    ensure!(chain_time.abs_diff(now) <= 15_000, "chain time is stale or local clock differs");
    ensure!(fills.len() < 2000, "fill history page is full; completeness is unknown");
    let evidence = evidence.context("missing durable lookup evidence")?;
    ensure!(evidence.request_id == r.id && evidence.market == config.market,
        "lookup evidence belongs to another request or market");
    let positions: Vec<_> = chain.asset_positions.iter().filter(|p| p.position.coin == market).collect();
    ensure!(positions.len() <= 1, "duplicate market position evidence");
    let actual = positions.first().map(|p| Decimal::from_str(&p.position.szi)
        .map_err(anyhow::Error::from).and_then(|q| config.units(q))).transpose()?.unwrap_or(0);
    ensure!(actual == evidence.position_units, "chain position differs from durable ledger");
    let from = r.created_ms.saturating_sub(300_000);
    ensure!(evidence.known_fills.iter().all(|f| f.venue == Venue::Entropy
        && !f.order_id.is_empty() && f.order_id != r.id
        && f.time_ms >= from && f.time_ms <= chain_time),
        "request already has fills or durable history is inconsistent");
    let known: std::collections::BTreeMap<_, _> = evidence.known_fills.iter().map(|f| (f.id.as_str(), f)).collect();
    ensure!(known.len() == evidence.known_fills.len(), "duplicate durable fill identity");
    let mut seen = std::collections::BTreeSet::new();
    for f in fills.iter().filter(|f| f.coin == market) {
        let identity = hyperliquid::user_fill_identity(f);
        let old = known.get(identity.as_str()).context("unattributed market fill in query window")?;
        ensure!(seen.insert(identity.clone()), "duplicate exchange fill identity");
        let side = match f.side.as_str() { "B" => Side::Buy, "A" => Side::Sell,
            _ => anyhow::bail!("invalid exchange fill side") };
        ensure!(f.oid > 0 && f.time == old.time_ms && side == old.side
            && config.units(Decimal::from_str(&f.sz)?)? == old.units
            && Decimal::from_str(&f.px)? == old.price
            && Decimal::from_str(&f.fee)? == old.fee,
            "exchange fill differs from authenticated durable fill");
    }
    ensure!(seen.len() == known.len(), "exchange history omits known fills; completeness is unknown");
    Ok(OrderResult { exchange_created_ms: None, terminal: true, fills: vec![],
        reason: "signed request expired; chain position and complete known-fill history confirm no fill for this request".into() })
}

fn expired_entropy_absence(
    r: &OrderRequest,
    chain_time: Option<u64>,
    has_orders: bool,
    has_fills: bool,
) -> bool {
    r.venue == Venue::Entropy
        && chain_time.is_some_and(|t| t > r.signed_expiry().saturating_add(30_000))
        && !has_orders
        && !has_fills
}

async fn verify_entropy_fee_contract(c: &InventoryConfig) -> Result<()> {
    let meta: Value = hyperliquid::info_client()?
        .post(hyperliquid::effective_info_url("mainnet")?)
        .json(&json!({"type":"meta","dex":"io"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let asset = meta["universe"]
        .as_array()
        .context("missing universe")?
        .iter()
        .find(|x| x["name"] == c.market.entropy_symbol())
        .context("OAI metadata missing")?;
    ensure!(
        asset["growthMode"] == "enabled",
        "OAI growth-mode fee contract changed"
    );
    // Growth mode caps deployer share at 100%; undiscounted tier-zero taker is an upper bound.
    ensure!(
        c.fee_entropy >= Decimal::new(9, 5),
        "configured Entropy exit fee below verified conservative bound"
    );
    Ok(())
}

#[cfg(test)]
fn expired_lighter_absence(r:&OrderRequest,server_ms:u64,account:&Value,history:&Value,account_index:i64)->Result<bool> {
    expired_lighter_absence_for(r,server_ms,account,history,account_index,42)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lighter_equity_rest_includes_isolated_margin_without_double_counting_pnl() {
        let account = json!({
            "collateral":"40", "available_balance":"41", "total_asset_value":"101.5",
            "positions":[{"margin_mode":1,"allocated_margin":"60","unrealized_pnl":"1.5"}]
        });
        assert_eq!(lighter_account_equity(&account).unwrap(), Decimal::new(1015, 1));
        assert_eq!(decimal(&account["available_balance"]).unwrap(), Decimal::from(41));
    }
    #[test]
    fn lighter_equity_stream_and_rest_use_total_valuation() {
        let rest = json!({"total_asset_value":"101.5","collateral":"40"});
        let stream = json!({"equity":"101.5","collateral":"40"});
        assert_eq!(lighter_account_equity(&rest).unwrap(), lighter_account_equity(&stream).unwrap());
        for total in [json!("0"), json!("-2.5"), json!(100)] {
            assert_eq!(lighter_account_equity(&json!({"total_asset_value":total})).unwrap(), decimal(&total).unwrap());
        }
    }
    #[test]
    fn lighter_equity_missing_or_invalid_total_cannot_fall_back_to_collateral() {
        let missing = json!({"collateral":"40","positions":[{"unrealized_pnl":"1.5"}]});
        assert!(lighter_account_equity(&missing).is_err());
        for total in [Value::Null, json!("NaN"), json!("bad"), json!({})] {
            assert!(lighter_account_equity(&json!({"total_asset_value":total,"equity":"101.5","collateral":"40"})).is_err());
        }
        assert!(lighter_account_equity(&json!({"equity":null,"collateral":"40"})).is_err());
    }
    #[test]
    fn unified_collateral_uses_spot_usdc_capped_by_active_asset_capacity() {
        let spot: hyperliquid::SpotClearinghouseState = serde_json::from_value(json!({
            "balances":[{"coin":"USDC","token":0,"total":"101.0","hold":"1.0"}]
        })).unwrap();
        let active = json!({"leverage":{"type":"isolated","value":3},
            "availableToTrade":["95.0","97.0"]});
        assert_eq!(entropy_collateral("unifiedAccount", Decimal::ZERO, Decimal::ZERO, &spot, &active).unwrap(),
            (Decimal::from(101), Decimal::from(95)));
        assert_eq!(entropy_collateral("disabled", Decimal::from(80), Decimal::from(5), &spot, &active).unwrap(),
            (Decimal::from(80), Decimal::from(75)));
        assert!(entropy_collateral("unifiedAccount", Decimal::ZERO, Decimal::ZERO, &spot,
            &json!({"availableToTrade":["95.0"]})).is_err());
    }
    #[test]
    fn unified_open_position_does_not_hide_unallocated_collateral() {
        let d = |s| Decimal::from_str(s).unwrap();
        let spot: hyperliquid::SpotClearinghouseState = serde_json::from_value(json!({
            "balances":[{"coin":"USDC","token":0,"total":"101.004427","hold":"4.532693"}]
        })).unwrap();
        let active = json!({"availableToTrade":["96.265333","105.3256"]});
        assert_eq!(entropy_collateral("unifiedAccount", d("4.532693"), d("4.532693"), &spot, &active).unwrap(),
            (d("101.004427"), d("96.265333")));
        assert_eq!(entropy_collateral("disabled", d("4.532693"), d("4.532693"), &spot, &active).unwrap(),
            (d("4.532693"), Decimal::ZERO));
        assert!(entropy_collateral("portfolioMargin", d("4.532693"), d("4.532693"), &spot, &active).is_err());
        let bad: hyperliquid::SpotClearinghouseState = serde_json::from_value(json!({
            "balances":[{"coin":"USDC","token":0,"total":"100","hold":"101"}]
        })).unwrap();
        assert!(entropy_collateral("unifiedAccount", d("4.532693"), d("4.532693"), &bad, &active).is_err());
    }
    #[test]
    fn live_open_rejects_cross_margin_even_with_matching_leverage() {
        let now = crate::domain::now_ms();
        let config = InventoryConfig::default();
        let request = OrderRequest {
            id: "isolated-check".into(), venue: Venue::Lighter, side: Side::Buy,
            units: 100, limit: Decimal::from(1700), arrival_mid: None,
            reduce_only: false, created_ms: now, expires_ms: now + 5_000,
            signed_expires_ms: None,
        };
        let mut account = AccountEvidence {
            venue: Venue::Lighter, account: "o1".into(), observed_ms: now,
            position_units: 0, free_margin: Decimal::from(100),
            equity: Decimal::from(100), leverage: 3, isolated: false,
            open_orders: 0, authenticated: true, liquidation_price: None,
        };
        assert!(require_isolated_setting(&account, 3).is_err());
        assert!(final_risk(&config, &account, &request).is_err());
        account.isolated = true;
        assert!(require_isolated_setting(&account, 3).is_ok());
        assert!(require_isolated_setting(&account, 2).is_err());
        assert!(final_risk(&config, &account, &request).is_ok());
    }
    #[test]
    fn rh_zero_transaction_time_uses_real_seconds_fallback() {
        // o1 order 939931406: exact production field shapes, no credentials.
        let raw = json!({"code":200,"orders":[{"client_order_index":939931406,"market_index":42,"status":"filled","filled_base_amount":"0.0090","remaining_base_amount":"0.0000","transaction_time":0,"updated_at":1788838838,"timestamp":1788838838,"created_at":1788838838}]});
        let parsed = inventory_orders(&raw).unwrap();
        let LighterAccountObservation::Order(order) = &parsed[0] else {
            panic!("missing order")
        };
        assert_eq!(order.exchange_time_ms, 1788838838000);
        assert_eq!(order.client_order_index, 939931406);
        assert_eq!(
            units(Decimal::from_str(&order.filled_base_size.to_string()).unwrap()).unwrap(),
            90
        );
        assert!(order.exchange_time_ms >= 1788838838026_i64 - 1000);
        assert_eq!(raw["orders"][0]["transaction_time"], json!(0));
        for wire in [1788838838567_i64, 1788838838567000, 1788838838567000000] {
            let mut value = raw.clone();
            value["orders"][0]["transaction_time"] = json!(wire);
            let LighterAccountObservation::Order(o) = inventory_orders(&value).unwrap().remove(0)
            else {
                panic!()
            };
            assert_eq!(o.exchange_time_ms, 1788838838567);
        }
        let mut missing = raw.clone();
        for k in ["transaction_time", "updated_at", "timestamp", "created_at"] {
            missing["orders"][0][k] = json!(0);
        }
        assert!(inventory_orders(&missing).is_err());
        assert_eq!(
            timestamp(&json!(1788838838598914_i64)).unwrap(),
            1788838838598
        );
    }
    #[test]
    fn rh_expired_absence_requires_fresh_time_flat_account_and_complete_history() {
        let now = crate::domain::now_ms();
        let r = OrderRequest {
            id: "x-v2-hedge".into(),
            venue: Venue::Lighter,
            side: Side::Buy,
            units: 100,
            limit: Decimal::from(1500),
            arrival_mid: None,
            reduce_only: false,
            created_ms: now - 60000,
            expires_ms: now - 55000,
            signed_expires_ms: None,
        };
        let a = json!({"accounts":[{"account_index":14629,"positions":[{"market_id":42,"position":"0","open_order_count":0,"pending_order_count":0}]}]});
        let h = json!({"code":200,"trades":[]});
        assert!(expired_lighter_absence(&r, now, &a, &h, 14629).unwrap());
        let mut still_signed = r.clone();
        still_signed.signed_expires_ms = Some(now + 500_000);
        assert!(!expired_lighter_absence(&still_signed, now, &a, &h, 14629).unwrap());
        assert!(!expired_lighter_absence(&r, now - 20000, &a, &h, 14629).unwrap());
        let mut position = a.clone();
        position["accounts"][0]["positions"][0]["position"] = json!("0.01");
        assert!(!expired_lighter_absence(&r, now, &position, &h, 14629).unwrap());
        let mut orders = a.clone();
        orders["accounts"][0]["positions"][0]["pending_order_count"] = json!(1);
        assert!(!expired_lighter_absence(&r, now, &orders, &h, 14629).unwrap());
        assert!(
            !expired_lighter_absence(
                &r,
                now,
                &a,
                &json!({"code":200,"trades":[{"timestamp":now}]}),
                14629
            )
            .unwrap()
        );
        assert!(
            !expired_lighter_absence(
                &r,
                now,
                &a,
                &json!({"code":200,"trades":[],"next_cursor":"more"}),
                14629
            )
            .unwrap()
        );
        assert!(expired_lighter_absence(&r, now, &a, &h, 1).is_err());
    }
    #[test]
    fn expired_unknown_order_requires_post_expiry_chain_evidence() {
        assert!(entropy_warm_response(
            &json!({"type":"clearinghouseState","data":{"time":123,"assetPositions":[]}})
        ));
        assert!(!entropy_warm_response(
            &json!({"type":"clearinghouseState","data":{"time":123}})
        ));
        let r = OrderRequest {
            id: "x".into(),
            venue: Venue::Entropy,
            side: Side::Sell,
            units: 100,
            limit: Decimal::from(1500),
            arrival_mid: None,
            reduce_only: false,
            created_ms: 1000,
            expires_ms: 6000,
            signed_expires_ms: None,
        };
        assert!(expired_entropy_absence(&r, Some(36001), false, false));
        let legacy = client_id(&r);
        let mut modern = r.clone();
        modern.id = "openai-instance-25-v2-hedge".into();
        assert!((1..=i32::MAX as i64).contains(&client_id(&modern)));
        assert_eq!(client_id(&r), legacy);
        for (t, o, f) in [
            (None, false, false),
            (Some(36000), false, false),
            (Some(36001), true, false),
            (Some(36001), false, true),
        ] {
            assert!(!expired_entropy_absence(&r, t, o, f));
        }
    }
    #[test]
    fn funding_direction_is_normalized_from_rate_and_position() {
        for (side, rate, expected) in [
            ("long", "0.001", "-0.5"),
            ("short", "0.001", "0.5"),
            ("long", "-0.001", "0.5"),
            ("short", "-0.001", "-0.5"),
        ] {
            assert_eq!(
                funding_cashflow(&json!({"position_side":side,"rate":rate,"change":"0.5"}))
                    .unwrap(),
                Decimal::from_str(expected).unwrap()
            );
        }
        assert!(
            funding_cashflow(&json!({"position_side":"long","rate":"0","change":"0.5"})).is_err()
        );
    }
    #[test]
    fn exchange_timestamp_units_are_normalized() {
        assert_eq!(timestamp(&json!(1788770000)).unwrap(), 1788770000000);
        assert_eq!(timestamp(&json!(1788770000000u64)).unwrap(), 1788770000000);
        assert_eq!(
            timestamp(&json!(1788770000000000u64)).unwrap(),
            1788770000000
        );
        assert!(timestamp(&json!(-1)).is_err());
    }
    #[test]
    fn lighter_margin_fraction_wire_formats_produce_same_leverage() {
        for reported in [json!("0.3334"), json!("33.34"), json!("3334")] {
            assert_eq!(lighter_leverage(&reported).unwrap(), 3);
        }
        assert_eq!(lighter_leverage(&json!("50.00")).unwrap(), 2);
        assert!(lighter_leverage(&json!("0")).is_err());
        assert!(lighter_leverage(&json!("10001")).is_err());
    }
}

#[cfg(test)]
#[path = "live_clock_tests.rs"]
mod clock_tests;

#[cfg(test)]
#[path = "entropy_absence_tests.rs"]
mod entropy_absence_tests;

#[cfg(test)]
#[path="live_transport_tests.rs"]
mod transport_tests;
