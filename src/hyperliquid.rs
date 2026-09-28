use std::{
    collections::{HashMap, VecDeque},
    sync::{LazyLock, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use ethers::{
    abi::{ParamType, Tokenizable, encode},
    signers::{LocalWallet, Signer},
    types::{
        U256,
        transaction::eip712::{
            EIP712Domain, Eip712, Eip712Error, encode_eip712_type, make_type_hash,
        },
    },
    utils::keccak256,
};
use hyperliquid_rust_sdk::{
    AssetMeta as SdkAssetMeta, BaseUrl as SdkBaseUrl, InfoClient as SdkInfoClient,
    Message as SdkWsMessage, Meta as SdkMeta, Subscription as SdkSubscription,
};
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tracing::info;

const DEFAULT_INFO_URL: &str = "https://api.hyperliquid.xyz/info";
const TESTNET_INFO_URL: &str = "https://api.hyperliquid-testnet.xyz/info";
pub const MAINNET_USDC_TOKEN: &str = "USDC:0x6d1e7cde53ba9467b783cb7c530ce054";
const HYPERLIQUID_EIP_PREFIX: &str = "HyperliquidTransaction:";
const HYPERLIQUID_SIGNATURE_CHAIN_ID: u64 = 421_614;
const INFO_MAX_ATTEMPTS: usize = 6;
const INFO_BASE_BACKOFF_MS: u64 = 750;
const INFO_MAX_BACKOFF_MS: u64 = 15_000;
const INFO_GLOBAL_COOLDOWN_CAP_MS: u64 = 60_000;
const INFO_REQUEST_TIMEOUT_SECS: u64 = 15;
const INFO_RATE_WINDOW_MS: u64 = 60_000;
const INFO_RATE_LIMIT_WEIGHT_PER_MIN: u32 = 300;
// Background account seeding may not consume the entire foreground budget.
const INFO_BACKGROUND_WEIGHT_PER_MIN: u32 = 140;
tokio::task_local! { static BACKGROUND_INFO: bool; }
pub async fn background_info<T>(read: impl std::future::Future<Output = T>) -> T {
    BACKGROUND_INFO.scope(true, read).await
}
const XYZ_SNAPSHOT_CACHE_TTL_MS: u64 = 15_000;
const SPOT_SNAPSHOT_CACHE_TTL_MS: u64 = 15_000;
const PERP_DEX_INDEX_CACHE_TTL_MS: u64 = 300_000;
const USER_RATE_LIMIT_CACHE_TTL_MS: u64 = 10_000;
const USER_RATE_LIMIT_CACHE_STALE_FALLBACK_MS: u64 = 60_000;

#[derive(Debug, Clone)]
struct TimedCacheEntry<T> {
    value: T,
    fetched_at_ms: u64,
}

type XyzSnapshotCacheMap = HashMap<String, TimedCacheEntry<XyzMarketSnapshot>>;
type SpotSnapshotCacheMap = HashMap<String, TimedCacheEntry<SpotMarketSnapshot>>;
type PerpAllMidsCacheMap = HashMap<String, TimedCacheEntry<HashMap<String, String>>>;
type PerpDexIndexCacheMap = HashMap<String, TimedCacheEntry<u32>>;
type UserRateLimitCacheMap = HashMap<String, TimedCacheEntry<UserRateLimit>>;

static XYZ_SNAPSHOT_CACHE: LazyLock<Mutex<XyzSnapshotCacheMap>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SPOT_SNAPSHOT_CACHE: LazyLock<Mutex<SpotSnapshotCacheMap>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PERP_ALL_MIDS_CACHE: LazyLock<Mutex<PerpAllMidsCacheMap>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PERP_DEX_INDEX_CACHE: LazyLock<Mutex<PerpDexIndexCacheMap>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static USER_RATE_LIMIT_CACHE: LazyLock<Mutex<UserRateLimitCacheMap>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static INFO_RATE_WINDOWS: LazyLock<Mutex<HashMap<String, InfoRateWindow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static INFO_COOLDOWN_UNTIL_MS: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Default)]
struct InfoRateWindow {
    entries: VecDeque<(u64, u32, bool)>,
    used_weight: u32,
    background_weight: u32,
}

fn reserve_info_capacity(
    window: &mut InfoRateWindow,
    now: u64,
    weight: u32,
    background: bool,
) -> bool {
    prune_info_rate_window(window, now);
    if window.used_weight.saturating_add(weight) > INFO_RATE_LIMIT_WEIGHT_PER_MIN
        || (background
            && window.background_weight.saturating_add(weight) > INFO_BACKGROUND_WEIGHT_PER_MIN)
    {
        return false;
    }
    window.entries.push_back((now, weight, background));
    window.used_weight += weight;
    if background {
        window.background_weight += weight;
    }
    true
}

fn xyz_snapshot_flight(key: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    type Flights = HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>;
    static FLIGHTS: LazyLock<Mutex<Flights>> = LazyLock::new(Default::default);
    let mut flights = FLIGHTS.lock().unwrap();
    flights.retain(|_, v| v.strong_count() > 0);
    if let Some(flight) = flights.get(key).and_then(std::sync::Weak::upgrade) {
        return flight;
    }
    let flight = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    flights.insert(key.into(), std::sync::Arc::downgrade(&flight));
    flight
}

#[derive(Debug, Deserialize)]
struct PerpDex {
    name: String,
    #[serde(rename = "fullName")]
    full_name: String,
    #[serde(rename = "assetToStreamingOiCap", default)]
    asset_to_streaming_oi_cap: Vec<(String, String)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DexMeta {
    pub universe: Vec<DexAssetMeta>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DexAssetMeta {
    pub name: String,
    pub sz_decimals: u32,
    #[serde(default)]
    pub max_leverage: Option<u32>,
    #[serde(default)]
    pub only_isolated: Option<bool>,
    #[serde(default)]
    pub is_delisted: Option<bool>,
    #[serde(default)]
    pub margin_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DexAssetContext {
    #[serde(default)]
    pub funding: Option<String>,
    #[serde(default)]
    pub open_interest: Option<String>,
    #[serde(default)]
    pub prev_day_px: Option<String>,
    #[serde(default)]
    pub day_ntl_vlm: Option<String>,
    #[serde(default)]
    pub premium: Option<String>,
    #[serde(default)]
    pub oracle_px: Option<String>,
    #[serde(default)]
    pub mark_px: Option<String>,
    #[serde(default)]
    pub mid_px: Option<String>,
    #[serde(default)]
    pub impact_pxs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotMetaResponse {
    #[serde(default)]
    pub universe: Vec<SpotMetaAsset>,
    #[serde(default)]
    pub tokens: Vec<SpotTokenInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotTokenInfo {
    pub name: String,
    #[serde(default)]
    pub sz_decimals: u32,
    #[serde(default)]
    pub index: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotMetaAsset {
    pub name: String,
    #[serde(default)]
    pub tokens: Vec<usize>,
    pub index: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotAssetContext {
    pub day_ntl_vlm: String,
    pub mark_px: String,
    pub mid_px: Option<String>,
    pub prev_day_px: String,
    #[serde(default)]
    pub circulating_supply: Option<String>,
    pub coin: String,
}

#[derive(Debug, Clone)]
pub struct SpotMarketSnapshot {
    pub meta: SpotMetaResponse,
    pub asset_contexts: Vec<SpotAssetContext>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CandleSnapshot {
    pub t: u64,
    #[serde(rename = "T")]
    pub t_: u64,
    pub s: String,
    pub i: String,
    pub o: String,
    pub c: String,
    pub h: String,
    pub l: String,
    pub v: String,
    pub n: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsCandleProbe {
    pub environment: String,
    pub coin: String,
    pub interval: String,
    pub time_open: u64,
    pub time_close: u64,
    pub open: String,
    pub close: String,
    pub high: String,
    pub low: String,
    pub volume: String,
    pub num_trades: u64,
    pub received_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct XyzMarketSnapshot {
    pub dex: String,
    pub dex_index: u32,
    pub meta: DexMeta,
    pub asset_contexts: Vec<DexAssetContext>,
    pub coin_to_asset: HashMap<String, u32>,
}

#[derive(Debug, Clone)]
pub struct XyzAsset {
    pub index: usize,
    pub asset_id: u32,
    pub meta: DexAssetMeta,
    pub context: DexAssetContext,
}

#[derive(Debug, Clone)]
pub struct SpotAsset {
    pub index: usize,
    pub asset_id: u32,
    pub coin: String,
    pub sz_decimals: u32,
    pub context: SpotAssetContext,
}

#[derive(Debug, Clone)]
pub struct OrderPlan {
    pub coin: String,
    pub asset_id: u32,
    pub sz_decimals: u32,
    pub reference_price: f64,
    pub limit_price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub coin: String,
    pub limit_px: String,
    pub oid: u64,
    pub side: String,
    pub sz: String,
    pub timestamp: u64,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub trigger_condition: String,
    #[serde(default)]
    pub is_trigger: bool,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub trigger_px: String,
    #[serde(default)]
    pub is_position_tpsl: bool,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub order_type: String,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub orig_sz: String,
    #[serde(default)]
    pub cloid: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String,
    pub time: u64,
    pub dir: String,
    pub closed_pnl: String,
    pub hash: String,
    pub oid: u64,
    pub crossed: bool,
    pub fee: String,
    #[serde(default)]
    pub builder_fee: Option<String>,
    #[serde(default)]
    pub tid: Option<u64>,
}

pub fn user_fill_identity(fill: &UserFill) -> String {
    let base = format!("{}:{}:{}:{}", fill.hash, fill.oid, fill.time, fill.coin);
    fill.tid.map_or(base.clone(), |tid| format!("{base}:{tid}"))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusResponse {
    pub status: String,
    #[serde(default)]
    pub order: Option<OrderStatusInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusInfo {
    pub order: OrderStatusOrder,
    pub status: String,
    pub status_timestamp: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusOrder {
    pub coin: String,
    pub side: String,
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub trigger_condition: String,
    #[serde(default)]
    pub is_trigger: bool,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub trigger_px: String,
    #[serde(default)]
    pub children: Vec<Value>,
    #[serde(default)]
    pub is_position_tpsl: bool,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub order_type: String,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub orig_sz: String,
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
    pub tif: String,
    #[serde(default)]
    pub cloid: Option<String>,
}

fn deserialize_null_string_as_default<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRateLimit {
    pub cum_vlm: String,
    pub n_requests_used: u64,
    pub n_requests_cap: u64,
    pub n_requests_surplus: i64,
}

impl UserRateLimit {
    pub fn request_capacity_remaining(&self) -> i64 {
        self.n_requests_cap as i64 + self.n_requests_surplus - self.n_requests_used as i64
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    pub margin_summary: MarginSummary,
    #[serde(default)]
    pub cross_margin_summary: Option<MarginSummary>,
    #[serde(default)]
    pub cross_maintenance_margin_used: Option<String>,
    #[serde(default)]
    pub withdrawable: Option<String>,
    #[serde(default)]
    pub asset_positions: Vec<AssetPosition>,
    #[serde(default)]
    pub time: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotClearinghouseState {
    #[serde(default)]
    pub balances: Vec<SpotBalance>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotBalance {
    #[serde(default)]
    pub coin: String,
    #[serde(default)]
    pub token: Option<Value>,
    #[serde(default)]
    pub total: String,
    #[serde(default)]
    pub hold: String,
    #[serde(default)]
    pub entry_ntl: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendAssetSubmitResult {
    pub nonce: u64,
    pub signer_address: String,
    pub action: SendAssetAction,
    pub response: Value,
}

#[derive(Debug, Clone)]
pub struct SendAssetSubmitRequest<'a> {
    pub exchange_url: &'a str,
    pub environment: &'a str,
    pub wallet_private_key: &'a str,
    pub destination: &'a str,
    pub source_dex: &'a str,
    pub destination_dex: &'a str,
    pub token: &'a str,
    pub amount: &'a str,
    pub from_sub_account: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAssetAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub signature_chain_id: U256,
    pub hyperliquid_chain: String,
    pub destination: String,
    pub source_dex: String,
    pub destination_dex: String,
    pub token: String,
    pub amount: String,
    pub from_sub_account: String,
    pub nonce: u64,
}

impl Eip712 for SendAssetAction {
    type Error = Eip712Error;

    fn domain(&self) -> std::result::Result<EIP712Domain, Self::Error> {
        Ok(EIP712Domain {
            name: Some("HyperliquidSignTransaction".to_string()),
            version: Some("1".to_string()),
            chain_id: Some(self.signature_chain_id),
            verifying_contract: Some(
                "0x0000000000000000000000000000000000000000"
                    .parse()
                    .expect("zero verifying contract"),
            ),
            salt: None,
        })
    }

    fn type_hash() -> std::result::Result<[u8; 32], Self::Error> {
        Ok(make_type_hash(
            format!("{HYPERLIQUID_EIP_PREFIX}SendAsset"),
            &[
                ("hyperliquidChain".to_string(), ParamType::String),
                ("destination".to_string(), ParamType::String),
                ("sourceDex".to_string(), ParamType::String),
                ("destinationDex".to_string(), ParamType::String),
                ("token".to_string(), ParamType::String),
                ("amount".to_string(), ParamType::String),
                ("fromSubAccount".to_string(), ParamType::String),
                ("nonce".to_string(), ParamType::Uint(64)),
            ],
        ))
    }

    fn struct_hash(&self) -> std::result::Result<[u8; 32], Self::Error> {
        let items = vec![
            ethers::abi::Token::Uint(Self::type_hash()?.into()),
            encode_eip712_type(self.hyperliquid_chain.clone().into_token()),
            encode_eip712_type(self.destination.clone().into_token()),
            encode_eip712_type(self.source_dex.clone().into_token()),
            encode_eip712_type(self.destination_dex.clone().into_token()),
            encode_eip712_type(self.token.clone().into_token()),
            encode_eip712_type(self.amount.clone().into_token()),
            encode_eip712_type(self.from_sub_account.clone().into_token()),
            encode_eip712_type(self.nonce.into_token()),
        ];
        Ok(keccak256(encode(&items)))
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    #[serde(default)]
    pub account_value: String,
    #[serde(default)]
    pub total_ntl_pos: String,
    #[serde(default)]
    pub total_raw_usd: String,
    #[serde(default)]
    pub total_margin_used: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPosition {
    #[serde(default)]
    pub position: PerpPosition,
    #[serde(default, rename = "type")]
    pub position_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpPosition {
    #[serde(default)]
    pub coin: String,
    #[serde(default)]
    pub szi: String,
    #[serde(default)]
    pub entry_px: Option<String>,
    #[serde(default)]
    pub position_value: Option<String>,
    #[serde(default)]
    pub unrealized_pnl: Option<String>,
    #[serde(default)]
    pub return_on_equity: Option<String>,
    #[serde(default)]
    pub liquidation_px: Option<String>,
    #[serde(default)]
    pub margin_used: Option<String>,
    #[serde(default)]
    pub leverage: Option<PerpLeverage>,
    #[serde(default)]
    pub max_leverage: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpLeverage {
    #[serde(rename = "type", default)]
    pub leverage_type: String,
    #[serde(default)]
    pub value: Option<u32>,
    #[serde(default)]
    pub raw_usd: Option<String>,
}

pub async fn run_smoke_test(info_url: Option<String>) -> Result<()> {
    let info_url = info_url.unwrap_or_else(|| DEFAULT_INFO_URL.to_string());
    let client = Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("failed to build HTTP client")?;

    let dexes: Vec<Option<PerpDex>> = client
        .post(&info_url)
        .json(&json!({ "type": "perpDexs" }))
        .send()
        .await
        .context("failed to call Hyperliquid info endpoint")?
        .error_for_status()
        .context("Hyperliquid info endpoint returned an error status")?
        .json()
        .await
        .context("failed to parse perpDexs response")?;

    let xyz = dexes
        .into_iter()
        .flatten()
        .find(|dex| dex.name == "xyz")
        .context("trade[XYZ] DEX `xyz` was not found in perpDexs response")?;

    let sample_assets = xyz
        .asset_to_streaming_oi_cap
        .iter()
        .take(5)
        .map(|(asset, cap)| format!("{asset} (OI cap {cap})"))
        .collect::<Vec<_>>()
        .join(", ");

    info!(
        dex = %xyz.name,
        full_name = %xyz.full_name,
        asset_count = xyz.asset_to_streaming_oi_cap.len(),
        sample_assets = %sample_assets,
        "Rust environment smoke test passed"
    );
    println!(
        "Rust environment smoke test passed: found {} ({}) with {} assets. Sample: {}",
        xyz.name,
        xyz.full_name,
        xyz.asset_to_streaming_oi_cap.len(),
        sample_assets
    );

    Ok(())
}

pub fn sdk_base_url(environment: &str) -> Result<SdkBaseUrl> {
    match environment {
        "mainnet" => Ok(SdkBaseUrl::Mainnet),
        "testnet" => Ok(SdkBaseUrl::Testnet),
        other => anyhow::bail!("unsupported Hyperliquid environment {other}"),
    }
}

pub fn effective_info_url(environment: &str) -> Result<&'static str> {
    match environment {
        "mainnet" => Ok(DEFAULT_INFO_URL),
        "testnet" => Ok(TESTNET_INFO_URL),
        other => anyhow::bail!("unsupported Hyperliquid environment {other}"),
    }
}

pub async fn submit_send_asset(
    request: SendAssetSubmitRequest<'_>,
) -> Result<SendAssetSubmitResult> {
    let wallet: LocalWallet = request
        .wallet_private_key
        .parse()
        .context("failed to parse transfer signer private key")?;
    let nonce = crate::domain::now_ms();
    let action = SendAssetAction {
        action_type: "sendAsset".to_string(),
        signature_chain_id: HYPERLIQUID_SIGNATURE_CHAIN_ID.into(),
        hyperliquid_chain: hyperliquid_chain_name(request.environment)?.to_string(),
        destination: request.destination.trim().to_ascii_lowercase(),
        source_dex: request.source_dex.trim().to_ascii_lowercase(),
        destination_dex: request.destination_dex.trim().to_ascii_lowercase(),
        token: request.token.trim().to_string(),
        amount: request.amount.trim().to_string(),
        from_sub_account: request.from_sub_account.trim().to_ascii_lowercase(),
        nonce,
    };
    let signature = wallet
        .sign_typed_data(&action)
        .await
        .context("failed to sign sendAsset action")?;
    let signer_address = format!("{:#x}", wallet.address());

    let client = info_client()?;
    let response = client
        .post(request.exchange_url)
        .json(&json!({
            "action": action,
            "nonce": nonce,
            "signature": signature,
            "vaultAddress": Value::Null,
        }))
        .send()
        .await
        .with_context(|| format!("failed to call {}", request.exchange_url))?
        .error_for_status()
        .with_context(|| format!("{} returned an error status", request.exchange_url))?
        .json::<Value>()
        .await
        .context("failed to parse Hyperliquid exchange response")?;

    Ok(SendAssetSubmitResult {
        nonce,
        signer_address,
        action,
        response,
    })
}

fn hyperliquid_chain_name(environment: &str) -> Result<&'static str> {
    match environment {
        "mainnet" => Ok("Mainnet"),
        "testnet" => Ok("Testnet"),
        other => anyhow::bail!("unsupported Hyperliquid environment {other}"),
    }
}

pub fn normalize_dex_coin(dex: &str, coin: &str) -> String {
    let dex = dex.trim().to_ascii_lowercase();
    let coin = coin.trim();
    if let Some((prefix, symbol)) = coin.split_once(':') {
        format!(
            "{}:{}",
            prefix.trim().to_ascii_lowercase(),
            symbol.trim().to_ascii_uppercase()
        )
    } else if dex.is_empty() {
        coin.to_ascii_uppercase()
    } else {
        format!("{dex}:{}", coin.to_ascii_uppercase())
    }
}

pub fn normalize_spot_coin(coin: &str) -> String {
    let raw = coin.trim();
    if raw.is_empty() {
        return String::new();
    }
    if let Some(index) = raw.strip_prefix('@') {
        return format!("@{}", index.trim());
    }
    let normalized = raw.replace('-', "/");
    if let Some((base, quote)) = normalized.split_once('/') {
        return format!(
            "{}/{}",
            base.trim().to_ascii_uppercase(),
            quote.trim().to_ascii_uppercase()
        );
    }
    raw.to_ascii_uppercase()
}

fn xyz_snapshot_cache_key(environment: &str, dex: &str) -> String {
    format!(
        "{}::{}",
        environment.trim().to_ascii_lowercase(),
        dex.trim().to_ascii_lowercase()
    )
}

fn spot_snapshot_cache_key(environment: &str) -> String {
    environment.trim().to_ascii_lowercase()
}

fn perp_all_mids_cache_key(environment: &str, dex: &str) -> String {
    format!(
        "{}::{}",
        environment.trim().to_ascii_lowercase(),
        dex.trim().to_ascii_lowercase()
    )
}

fn user_rate_limit_cache_key(environment: &str, user_address: &str) -> String {
    format!(
        "{}::{}",
        environment.trim().to_ascii_lowercase(),
        user_address.trim().to_ascii_lowercase()
    )
}

fn cache_entry_is_fresh(fetched_at_ms: u64, max_age_ms: u64) -> bool {
    if max_age_ms == 0 {
        return false;
    }
    let now = crate::domain::now_ms();
    now.saturating_sub(fetched_at_ms) <= max_age_ms
}

pub async fn fetch_xyz_market_snapshot_cached(
    environment: &str,
    dex: &str,
    max_age_ms: u64,
) -> Result<XyzMarketSnapshot> {
    let max_age_ms = if max_age_ms == 0 {
        XYZ_SNAPSHOT_CACHE_TTL_MS
    } else {
        max_age_ms
    };
    let key = xyz_snapshot_cache_key(environment, dex);
    if let Ok(cache) = XYZ_SNAPSHOT_CACHE.lock()
        && let Some(entry) = cache.get(&key)
        && cache_entry_is_fresh(entry.fetched_at_ms, max_age_ms)
    {
        return Ok(entry.value.clone());
    }

    // Coalesce cold starts: cancelled preflights must not consume the local
    // request window with duplicate SDK metadata fetches for every account pair.
    let flight = xyz_snapshot_flight(&key);
    tracing::info!(dex, "metadata: waiting for shared fetch");
    let _flight = flight.lock().await;
    tracing::info!(dex, "metadata: acquired shared fetch");
    if let Ok(cache) = XYZ_SNAPSHOT_CACHE.lock()
        && let Some(entry) = cache.get(&key)
        && cache_entry_is_fresh(entry.fetched_at_ms, max_age_ms)
    {
        return Ok(entry.value.clone());
    }

    let snapshot = fetch_xyz_market_snapshot(environment, dex).await?;
    tracing::info!(dex, "metadata: snapshot ready");
    if let Ok(mut cache) = XYZ_SNAPSHOT_CACHE.lock() {
        cache.insert(
            key,
            TimedCacheEntry {
                value: snapshot.clone(),
                fetched_at_ms: crate::domain::now_ms(),
            },
        );
    }
    Ok(snapshot)
}

pub async fn fetch_spot_market_snapshot_cached(
    environment: &str,
    max_age_ms: u64,
) -> Result<SpotMarketSnapshot> {
    let max_age_ms = if max_age_ms == 0 {
        SPOT_SNAPSHOT_CACHE_TTL_MS
    } else {
        max_age_ms
    };
    let key = spot_snapshot_cache_key(environment);
    if let Ok(cache) = SPOT_SNAPSHOT_CACHE.lock()
        && let Some(entry) = cache.get(&key)
        && cache_entry_is_fresh(entry.fetched_at_ms, max_age_ms)
    {
        return Ok(entry.value.clone());
    }

    let snapshot = fetch_spot_market_snapshot(environment).await?;
    if let Ok(mut cache) = SPOT_SNAPSHOT_CACHE.lock() {
        cache.insert(
            key,
            TimedCacheEntry {
                value: snapshot.clone(),
                fetched_at_ms: crate::domain::now_ms(),
            },
        );
    }
    Ok(snapshot)
}

pub async fn fetch_perp_all_mids(environment: &str, dex: &str) -> Result<HashMap<String, String>> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let canonical_dex = dex.trim().to_ascii_lowercase();
    let body = if canonical_dex.is_empty() {
        json!({ "type": "allMids" })
    } else {
        json!({
            "type": "allMids",
            "dex": canonical_dex,
        })
    };
    post_info(&client, info_url, body).await
}

/// Fetches one bounded read-only L2 snapshot for final Canary price
/// protection. Continuous strategies should use their WebSocket book; this
/// REST seam is reserved for startup/reconnect and explicit bounded Canary
/// verification.
pub async fn fetch_l2_book_snapshot(environment: &str, coin: &str) -> Result<Value> {
    anyhow::ensure!(!coin.trim().is_empty(), "L2 book coin is required");
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(
        &client,
        info_url,
        json!({
            "type": "l2Book",
            "coin": coin.trim(),
        }),
    )
    .await
}

pub async fn fetch_perp_all_mids_cached(
    environment: &str,
    dex: &str,
    max_age_ms: u64,
) -> Result<HashMap<String, String>> {
    let key = perp_all_mids_cache_key(environment, dex);
    if let Ok(cache) = PERP_ALL_MIDS_CACHE.lock()
        && let Some(entry) = cache.get(&key)
        && cache_entry_is_fresh(entry.fetched_at_ms, max_age_ms)
    {
        return Ok(entry.value.clone());
    }

    let mids = fetch_perp_all_mids(environment, dex).await?;
    if let Ok(mut cache) = PERP_ALL_MIDS_CACHE.lock() {
        cache.insert(
            key,
            TimedCacheEntry {
                value: mids.clone(),
                fetched_at_ms: crate::domain::now_ms(),
            },
        );
    }
    Ok(mids)
}

pub async fn fetch_ws_candle_probe(
    environment: &str,
    coin: &str,
    interval: &str,
    timeout_ms: u64,
) -> Result<WsCandleProbe> {
    let mut client = SdkInfoClient::with_reconnect(None, Some(sdk_base_url(environment)?))
        .await
        .context("failed to initialize Hyperliquid websocket info client")?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let subscription_id = client
        .subscribe(
            SdkSubscription::Candle {
                coin: coin.to_string(),
                interval: interval.to_string(),
            },
            sender,
        )
        .await
        .with_context(|| {
            format!("failed to subscribe to websocket candles for {coin} {interval}")
        })?;

    let timeout_duration = Duration::from_millis(timeout_ms.clamp(500, 15_000));
    let result = tokio::time::timeout(timeout_duration, async {
        loop {
            let Some(message) = receiver.recv().await else {
                anyhow::bail!("websocket candle stream closed before data arrived");
            };
            match message {
                SdkWsMessage::Candle(candle) => {
                    let data = candle.data;
                    return Ok(WsCandleProbe {
                        environment: environment.to_string(),
                        coin: data.coin,
                        interval: data.interval,
                        time_open: data.time_open,
                        time_close: data.time_close,
                        open: data.open,
                        close: data.close,
                        high: data.high,
                        low: data.low,
                        volume: data.volume,
                        num_trades: data.num_trades,
                        received_at_ms: crate::domain::now_ms(),
                    });
                }
                SdkWsMessage::HyperliquidError(error) => {
                    anyhow::bail!("Hyperliquid websocket error: {error}");
                }
                _ => {}
            }
        }
    })
    .await
    .with_context(|| format!("timed out waiting for websocket candle {coin} {interval}"))?;

    let unsubscribe_result = client.unsubscribe(subscription_id).await;
    if let Err(error) = unsubscribe_result {
        tracing::warn!(%error, "failed to unsubscribe websocket candle probe");
    }
    result
}

pub async fn fetch_xyz_market_snapshot(environment: &str, dex: &str) -> Result<XyzMarketSnapshot> {
    const MAX_ATTEMPTS: usize = 3;
    let mut backoff_ms = 120_u64;
    for attempt in 1..=MAX_ATTEMPTS {
        match fetch_xyz_market_snapshot_once(environment, dex).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                if attempt == MAX_ATTEMPTS {
                    return Err(error).with_context(|| {
                        format!("failed to fetch XYZ market snapshot after {MAX_ATTEMPTS} attempts")
                    });
                }
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(1_000);
            }
        }
    }
    unreachable!("snapshot retry loop should always return before reaching this point")
}

async fn fetch_xyz_market_snapshot_once(environment: &str, dex: &str) -> Result<XyzMarketSnapshot> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let canonical_dex = dex.trim().to_ascii_lowercase();
    let is_default_perp = canonical_dex.is_empty();
    let dex_index = if is_default_perp {
        0
    } else {
        fetch_perp_dex_index(&client, info_url, &canonical_dex).await?
    };
    let meta_request = if is_default_perp {
        json!({ "type": "meta" })
    } else {
        json!({
            "type": "meta",
            "dex": canonical_dex.clone(),
        })
    };
    let meta: DexMeta = post_info(&client, info_url, meta_request).await?;
    let asset_contexts = fetch_dex_asset_contexts(&client, info_url, &canonical_dex).await?;
    anyhow::ensure!(
        meta.universe.len() == asset_contexts.len(),
        "meta universe length {} does not match asset ctx length {} for dex {}",
        meta.universe.len(),
        asset_contexts.len(),
        if is_default_perp {
            "<default_perp>"
        } else {
            canonical_dex.as_str()
        }
    );

    let coin_to_asset = meta
        .universe
        .iter()
        .enumerate()
        .map(|(index, asset)| {
            let asset_id = if is_default_perp {
                index.try_into().unwrap_or(u32::MAX)
            } else {
                hip3_asset_id(dex_index, index.try_into().unwrap_or(u32::MAX))
            };
            (asset.name.clone(), asset_id)
        })
        .collect();

    Ok(XyzMarketSnapshot {
        dex: canonical_dex,
        dex_index,
        meta,
        asset_contexts,
        coin_to_asset,
    })
}

pub async fn fetch_candle_snapshot(
    environment: &str,
    coin: &str,
    interval: &str,
    start_time_ms: u64,
    end_time_ms: u64,
) -> Result<Vec<CandleSnapshot>> {
    anyhow::ensure!(
        end_time_ms > start_time_ms,
        "end_time_ms must be greater than start_time_ms"
    );
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(
        &client,
        info_url,
        json!({
            "type": "candleSnapshot",
            "req": {
                "coin": coin,
                "interval": interval,
                "startTime": start_time_ms,
                "endTime": end_time_ms,
            }
        }),
    )
    .await
}

pub async fn fetch_open_orders(
    environment: &str,
    dex: &str,
    user_address: &str,
) -> Result<Vec<OpenOrder>> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let canonical_dex = dex.trim().to_ascii_lowercase();
    let query_dex = if canonical_dex == "spot" {
        String::new()
    } else {
        canonical_dex
    };
    post_info(
        &client,
        info_url,
        info_payload_with_optional_dex("openOrders", user_address, &query_dex),
    )
    .await
}

pub async fn fetch_user_fills(
    environment: &str,
    dex: &str,
    user_address: &str,
) -> Result<Vec<UserFill>> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let canonical_dex = dex.trim().to_ascii_lowercase();
    let query_dex = if canonical_dex == "spot" {
        String::new()
    } else {
        canonical_dex.clone()
    };
    let canonical_prefix = if query_dex.is_empty() {
        None
    } else {
        Some(format!("{query_dex}:"))
    };
    let fills: Vec<UserFill> = post_info(
        &client,
        info_url,
        info_payload_with_optional_dex("userFills", user_address, &query_dex),
    )
    .await?;
    Ok(fills
        .into_iter()
        .filter(|fill| {
            canonical_prefix
                .as_deref()
                .map(|prefix| fill.coin.starts_with(prefix))
                .unwrap_or(true)
        })
        .collect())
}

pub async fn fetch_user_fills_by_time(
    environment: &str,
    dex: &str,
    user_address: &str,
    start_time_ms: u64,
    end_time_ms: Option<u64>,
) -> Result<Vec<UserFill>> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let canonical_dex = dex.trim().to_ascii_lowercase();
    let query_dex = if canonical_dex == "spot" {
        String::new()
    } else {
        canonical_dex.clone()
    };
    let canonical_prefix = if query_dex.is_empty() {
        None
    } else {
        Some(format!("{query_dex}:"))
    };
    let mut payload = info_payload_with_optional_dex("userFillsByTime", user_address, &query_dex);
    if let Some(object) = payload.as_object_mut() {
        object.insert("startTime".to_string(), serde_json::json!(start_time_ms));
        if let Some(end_time_ms) = end_time_ms {
            object.insert("endTime".to_string(), serde_json::json!(end_time_ms));
        }
    }
    let fills: Vec<UserFill> = post_info(&client, info_url, payload).await?;
    Ok(fills
        .into_iter()
        .filter(|fill| {
            canonical_prefix
                .as_deref()
                .map(|prefix| fill.coin.starts_with(prefix))
                .unwrap_or(true)
        })
        .collect())
}

/// Absence proofs must see the raw page size before filtering by market/DEX.
/// A full 2000-row response is not evidence of complete history.
pub async fn fetch_user_fills_by_time_unfiltered(
    environment: &str, user_address: &str, start_time_ms: u64,
) -> Result<Vec<UserFill>> {
    post_info(&info_client()?, effective_info_url(environment)?, json!({
        "type":"userFillsByTime", "user":user_address,
        "startTime":start_time_ms, "aggregateByTime":false
    })).await
}

pub async fn fetch_user_rate_limit(environment: &str, user_address: &str) -> Result<UserRateLimit> {
    let key = user_rate_limit_cache_key(environment, user_address);
    if let Ok(cache) = USER_RATE_LIMIT_CACHE.lock()
        && let Some(entry) = cache.get(&key)
        && cache_entry_is_fresh(entry.fetched_at_ms, USER_RATE_LIMIT_CACHE_TTL_MS)
    {
        return Ok(entry.value.clone());
    }
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    match post_info::<UserRateLimit>(
        &client,
        info_url,
        json!({
            "type": "userRateLimit",
            "user": user_address,
        }),
    )
    .await
    {
        Ok(rate_limit) => {
            if let Ok(mut cache) = USER_RATE_LIMIT_CACHE.lock() {
                cache.insert(
                    key,
                    TimedCacheEntry {
                        value: rate_limit.clone(),
                        fetched_at_ms: crate::domain::now_ms(),
                    },
                );
            }
            Ok(rate_limit)
        }
        Err(error) => {
            if let Ok(cache) = USER_RATE_LIMIT_CACHE.lock()
                && let Some(entry) = cache.get(&key)
                && cache_entry_is_fresh(
                    entry.fetched_at_ms,
                    USER_RATE_LIMIT_CACHE_STALE_FALLBACK_MS,
                )
            {
                tracing::warn!(
                    user = %user_address,
                    error = %error,
                    "userRateLimit fetch failed; using stale cached value"
                );
                return Ok(entry.value.clone());
            }
            Err(error)
        }
    }
}

pub async fn fetch_order_status_by_oid(
    environment: &str,
    user_address: &str,
    oid: u64,
) -> Result<OrderStatusResponse> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(
        &client,
        info_url,
        json!({
            "type": "orderStatus",
            "user": user_address,
            "oid": oid,
        }),
    )
    .await
}

pub async fn fetch_order_status_by_cloid(
    environment: &str,
    user_address: &str,
    cloid: &str,
) -> Result<OrderStatusResponse> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let cloid = normalize_cloid_for_info(cloid)?;
    post_info(
        &client,
        info_url,
        json!({
            "type": "orderStatus",
            "user": user_address,
            "oid": cloid,
        }),
    )
    .await
}

pub async fn fetch_clearinghouse_state(
    environment: &str,
    dex: &str,
    user_address: &str,
) -> Result<ClearinghouseState> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(
        &client,
        info_url,
        info_payload_with_optional_dex("clearinghouseState", user_address, dex),
    )
    .await
}

pub async fn fetch_default_clearinghouse_state(
    environment: &str,
    user_address: &str,
) -> Result<ClearinghouseState> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(
        &client,
        info_url,
        json!({
            "type": "clearinghouseState",
            "user": user_address,
        }),
    )
    .await
}

pub async fn fetch_spot_clearinghouse_state(
    environment: &str,
    user_address: &str,
) -> Result<SpotClearinghouseState> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let value: Value = post_info(
        &client,
        info_url,
        json!({
            "type": "spotClearinghouseState",
            "user": user_address,
        }),
    )
    .await?;
    parse_spot_clearinghouse_state_value(value)
}

pub async fn fetch_active_asset_data(
    environment: &str,
    user_address: &str,
    coin: &str,
) -> Result<Value> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(&client, info_url, json!({
        "type": "activeAssetData",
        "user": user_address,
        "coin": coin,
    })).await
}

pub async fn fetch_user_abstraction(environment: &str, user_address: &str) -> Result<String> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    post_info(&client, info_url, json!({"type":"userAbstraction", "user":user_address})).await
}

pub fn parse_spot_clearinghouse_state_value(value: Value) -> Result<SpotClearinghouseState> {
    if value.is_null() {
        return Ok(SpotClearinghouseState::default());
    }
    serde_json::from_value(value).context("failed to parse spot clearinghouse state")
}

impl XyzMarketSnapshot {
    pub fn sdk_meta(&self) -> SdkMeta {
        SdkMeta {
            universe: self
                .meta
                .universe
                .iter()
                .map(|asset| SdkAssetMeta {
                    name: asset.name.clone(),
                    sz_decimals: asset.sz_decimals,
                })
                .collect(),
        }
    }

    pub fn asset(&self, coin: &str) -> Result<XyzAsset> {
        let canonical = normalize_dex_coin(&self.dex, coin);
        let index = self
            .meta
            .universe
            .iter()
            .position(|asset| asset.name == canonical)
            .with_context(|| format!("coin {canonical} not found in dex {}", self.dex))?;
        let meta = self.meta.universe[index].clone();
        anyhow::ensure!(
            meta.is_delisted != Some(true),
            "coin {} is delisted and cannot be traded",
            meta.name
        );
        let context = self
            .asset_contexts
            .get(index)
            .cloned()
            .with_context(|| format!("missing asset context for {}", meta.name))?;
        let asset_id = if self.dex.trim().is_empty() {
            index.try_into()?
        } else {
            hip3_asset_id(self.dex_index, index.try_into()?)
        };
        Ok(XyzAsset {
            index,
            asset_id,
            meta,
            context,
        })
    }
}

impl SpotMarketSnapshot {
    pub fn universe(&self) -> Vec<String> {
        self.meta
            .universe
            .iter()
            .map(|asset| self.display_coin_for_asset(asset))
            .collect()
    }

    pub fn asset_context(&self, coin: &str) -> Result<SpotAssetContext> {
        let asset = self.asset(coin)?;
        Ok(asset.context)
    }

    pub fn asset(&self, coin: &str) -> Result<SpotAsset> {
        let canonical = normalize_spot_coin(coin);
        let asset = self
            .meta
            .universe
            .iter()
            .find(|asset| {
                normalize_spot_coin(&asset.name) == canonical
                    || self.display_coin_for_asset(asset) == canonical
            })
            .with_context(|| format!("spot coin {canonical} not found in spot universe"))?;
        let context = self
            .asset_contexts
            .get(asset.index)
            .cloned()
            .or_else(|| {
                self.asset_contexts
                    .iter()
                    .find(|context| normalize_spot_coin(&context.coin) == canonical)
                    .cloned()
            })
            .with_context(|| format!("spot context missing for {canonical}"))?;
        let sz_decimals = self
            .base_token_sz_decimals(asset)
            .with_context(|| format!("spot szDecimals missing for {canonical}"))?;
        let coin = self.display_coin_for_asset(asset);
        Ok(SpotAsset {
            index: asset.index,
            asset_id: 10_000 + asset.index as u32,
            coin,
            sz_decimals,
            context,
        })
    }

    pub fn candle_coin(&self, coin: &str) -> Result<String> {
        let asset = self.asset(coin)?;
        if asset.coin.eq_ignore_ascii_case("PURR/USDC") {
            Ok(asset.coin)
        } else {
            Ok(format!("@{}", asset.index))
        }
    }

    fn display_coin_for_asset(&self, asset: &SpotMetaAsset) -> String {
        let pair = asset
            .tokens
            .first()
            .and_then(|base_index| {
                let base = self
                    .meta
                    .tokens
                    .iter()
                    .find(|token| token.index == *base_index)?;
                let quote_index = asset.tokens.get(1)?;
                let quote = self
                    .meta
                    .tokens
                    .iter()
                    .find(|token| token.index == *quote_index)?;
                Some(format!(
                    "{}/{}",
                    base.name.to_ascii_uppercase(),
                    quote.name.to_ascii_uppercase()
                ))
            })
            .unwrap_or_else(|| normalize_spot_coin(&asset.name));
        normalize_spot_coin(&pair)
    }

    fn base_token_sz_decimals(&self, asset: &SpotMetaAsset) -> Option<u32> {
        let base_token_index = *asset.tokens.first()?;
        self.meta
            .tokens
            .iter()
            .find(|token| token.index == base_token_index)
            .map(|token| token.sz_decimals)
    }
}

impl XyzAsset {
    pub fn reference_price(&self) -> Result<f64> {
        parse_price(
            self.context
                .mid_px
                .as_deref()
                .or(self.context.mark_px.as_deref())
                .or(self.context.oracle_px.as_deref())
                .or(self.context.prev_day_px.as_deref()),
            &self.meta.name,
        )
    }
}

pub fn build_order_plan(
    snapshot: &XyzMarketSnapshot,
    coin: &str,
    is_buy: bool,
    notional_usd: f64,
    explicit_price: Option<f64>,
    max_slippage_bps: f64,
) -> Result<OrderPlan> {
    anyhow::ensure!(notional_usd > 0.0, "notional_usd must be positive");
    anyhow::ensure!(
        max_slippage_bps >= 0.0,
        "max_slippage_bps cannot be negative"
    );
    let asset = snapshot.asset(coin)?;
    let reference_price = explicit_price.unwrap_or(asset.reference_price()?);
    anyhow::ensure!(
        reference_price.is_finite() && reference_price > 0.0,
        "reference price for {} must be positive",
        asset.meta.name
    );
    let slippage_factor = if explicit_price.is_some() {
        1.0
    } else if is_buy {
        1.0 + max_slippage_bps / 10_000.0
    } else {
        1.0 - max_slippage_bps / 10_000.0
    };
    anyhow::ensure!(
        slippage_factor > 0.0,
        "slippage guard produced non-positive price"
    );
    let limit_price = round_perp_price(reference_price * slippage_factor, asset.meta.sz_decimals);
    let size = round_size_down(notional_usd / reference_price, asset.meta.sz_decimals);
    anyhow::ensure!(
        size > 0.0,
        "order size rounds to zero for {} at notional {} and price {}",
        asset.meta.name,
        notional_usd,
        reference_price
    );

    Ok(OrderPlan {
        coin: asset.meta.name,
        asset_id: asset.asset_id,
        sz_decimals: asset.meta.sz_decimals,
        reference_price,
        limit_price,
        size,
    })
}

pub fn build_spot_order_plan(
    snapshot: &SpotMarketSnapshot,
    coin: &str,
    is_buy: bool,
    notional_usd: f64,
    explicit_price: Option<f64>,
    max_slippage_bps: f64,
) -> Result<OrderPlan> {
    anyhow::ensure!(notional_usd > 0.0, "notional_usd must be positive");
    anyhow::ensure!(
        max_slippage_bps >= 0.0,
        "max_slippage_bps cannot be negative"
    );
    let asset = snapshot.asset(coin)?;
    let reference_price = explicit_price
        .or_else(|| parse_optional_price(asset.context.mid_px.as_deref()))
        .or_else(|| parse_optional_price(Some(asset.context.mark_px.as_str())))
        .or_else(|| parse_optional_price(Some(asset.context.prev_day_px.as_str())))
        .with_context(|| format!("no spot reference price available for {}", asset.coin))?;
    anyhow::ensure!(
        reference_price.is_finite() && reference_price > 0.0,
        "reference price for {} must be positive",
        asset.coin
    );
    let slippage_factor = if explicit_price.is_some() {
        1.0
    } else if is_buy {
        1.0 + max_slippage_bps / 10_000.0
    } else {
        1.0 - max_slippage_bps / 10_000.0
    };
    anyhow::ensure!(
        slippage_factor > 0.0,
        "slippage guard produced non-positive price"
    );
    let limit_price = round_spot_price(reference_price * slippage_factor, asset.sz_decimals);
    let size = round_size_down(notional_usd / reference_price, asset.sz_decimals);
    anyhow::ensure!(
        size > 0.0,
        "order size rounds to zero for {} at notional {} and price {}",
        asset.coin,
        notional_usd,
        reference_price
    );

    Ok(OrderPlan {
        coin: asset.coin,
        asset_id: asset.asset_id,
        sz_decimals: asset.sz_decimals,
        reference_price,
        limit_price,
        size,
    })
}

pub fn round_size_down(size: f64, sz_decimals: u32) -> f64 {
    let scale = 10_f64.powi(sz_decimals.try_into().unwrap_or(i32::MAX));
    let scaled = size * scale;
    // Decimal sizes arrive as f64 values. An exchange-valid size such as
    // 0.0029 can therefore be represented as 28.999999999999996 ticks and a
    // plain floor would incorrectly remove one full size tick. Recover only
    // representation-level drift; values materially below a tick still round
    // down as intended.
    let representation_tolerance = f64::EPSILON * scaled.abs().max(1.0) * 4.0;
    (scaled + representation_tolerance).floor() / scale
}

pub fn round_perp_price(price: f64, sz_decimals: u32) -> f64 {
    let max_decimals = 6_u32.saturating_sub(sz_decimals);
    round_to_significant_and_decimal(price, 5, max_decimals)
}

pub fn round_spot_price(price: f64, sz_decimals: u32) -> f64 {
    let max_decimals = 8_u32.saturating_sub(sz_decimals);
    round_to_significant_and_decimal(price, 5, max_decimals)
}

pub fn hip3_asset_id(perp_dex_index: u32, index_in_meta: u32) -> u32 {
    100_000 + perp_dex_index * 10_000 + index_in_meta
}

pub fn normalize_cloid_for_info(cloid: &str) -> Result<String> {
    let trimmed = cloid.trim();
    if let Ok(uuid) = uuid::Uuid::parse_str(trimmed) {
        let hex = uuid
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        return Ok(format!("0x{hex}"));
    }

    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    anyhow::ensure!(
        hex.len() == 32 && hex.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit()),
        "cloid must be a UUID or 32-byte hex string"
    );
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

fn round_to_significant_and_decimal(value: f64, significant: i32, max_decimals: u32) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }

    let abs = value.abs();
    let magnitude = abs.log10().floor();
    let significant_scale = 10_f64.powf((significant - 1) as f64 - magnitude);
    let significant_rounded = (value * significant_scale).round() / significant_scale;
    let decimal_scale = 10_f64.powi(max_decimals.try_into().unwrap_or(i32::MAX));
    (significant_rounded * decimal_scale).round() / decimal_scale
}

async fn fetch_perp_dex_index(client: &Client, info_url: &str, dex: &str) -> Result<u32> {
    let cache_key = format!("{}::{}", info_url.trim().to_ascii_lowercase(), dex);
    if let Ok(cache) = PERP_DEX_INDEX_CACHE.lock()
        && let Some(entry) = cache.get(&cache_key)
        && cache_entry_is_fresh(entry.fetched_at_ms, PERP_DEX_INDEX_CACHE_TTL_MS)
    {
        return Ok(entry.value);
    }

    let dexes: Vec<Option<PerpDex>> = post_info(client, info_url, json!({ "type": "perpDexs" }))
        .await
        .context("failed to fetch perpDexs")?;
    let index = dexes
        .into_iter()
        .enumerate()
        .find_map(|(index, maybe_dex)| {
            maybe_dex
                .filter(|perp_dex| perp_dex.name == dex)
                .map(|_| index as u32)
        })
        .with_context(|| format!("perp dex {dex} not found"))?;

    if let Ok(mut cache) = PERP_DEX_INDEX_CACHE.lock() {
        cache.insert(
            cache_key,
            TimedCacheEntry {
                value: index,
                fetched_at_ms: crate::domain::now_ms(),
            },
        );
    }
    Ok(index)
}

async fn fetch_dex_asset_contexts(
    client: &Client,
    info_url: &str,
    dex: &str,
) -> Result<Vec<DexAssetContext>> {
    let canonical_dex = dex.trim().to_ascii_lowercase();
    let request = if canonical_dex.is_empty() {
        json!({ "type": "metaAndAssetCtxs" })
    } else {
        json!({
            "type": "metaAndAssetCtxs",
            "dex": canonical_dex,
        })
    };
    let payload: serde_json::Value = post_info(client, info_url, request).await?;
    let array = payload
        .as_array()
        .context("metaAndAssetCtxs response should be an array")?;
    let contexts = array
        .get(1)
        .cloned()
        .context("metaAndAssetCtxs response missing asset contexts")?;
    serde_json::from_value(contexts).context("failed to parse dex asset contexts")
}

pub async fn fetch_spot_market_snapshot(environment: &str) -> Result<SpotMarketSnapshot> {
    let info_url = effective_info_url(environment)?;
    let client = info_client()?;
    let payload: Value = post_info(
        &client,
        info_url,
        json!({
            "type": "spotMetaAndAssetCtxs",
        }),
    )
    .await?;
    let array = payload
        .as_array()
        .context("spotMetaAndAssetCtxs response should be an array")?;
    let meta_value = array
        .first()
        .cloned()
        .context("spotMetaAndAssetCtxs response missing meta payload")?;
    let contexts_value = array
        .get(1)
        .cloned()
        .context("spotMetaAndAssetCtxs response missing asset contexts")?;
    let meta: SpotMetaResponse =
        serde_json::from_value(meta_value).context("failed to parse spot meta")?;
    let asset_contexts: Vec<SpotAssetContext> =
        serde_json::from_value(contexts_value).context("failed to parse spot asset contexts")?;
    Ok(SpotMarketSnapshot {
        meta,
        asset_contexts,
    })
}

fn info_payload_with_optional_dex(action_type: &str, user_address: &str, dex: &str) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert("type".to_string(), Value::String(action_type.to_string()));
    payload.insert("user".to_string(), Value::String(user_address.to_string()));
    let canonical_dex = dex.trim().to_ascii_lowercase();
    if !canonical_dex.is_empty() {
        payload.insert("dex".to_string(), Value::String(canonical_dex));
    }
    Value::Object(payload)
}

fn parse_price(value: Option<&str>, coin: &str) -> Result<f64> {
    let value = value.with_context(|| format!("no price available for {coin}"))?;
    value
        .parse::<f64>()
        .with_context(|| format!("failed to parse price {value} for {coin}"))
}

fn parse_optional_price(value: Option<&str>) -> Option<f64> {
    value.and_then(|value| value.parse::<f64>().ok())
}

pub(crate) fn info_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(INFO_REQUEST_TIMEOUT_SECS))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("failed to build HTTP client")
}

async fn post_info<T: DeserializeOwned>(
    client: &Client,
    info_url: &str,
    body: serde_json::Value,
) -> Result<T> {
    let mut last_error: Option<anyhow::Error> = None;
    let info_type = info_request_type(&body);
    let info_weight = info_request_weight(&body);

    for attempt in 1..=INFO_MAX_ATTEMPTS {
        let metadata = matches!(info_type.as_str(), "perpDexs" | "meta" | "metaAndAssetCtxs");
        if metadata {
            tracing::info!(%info_type, attempt, "metadata: waiting rate capacity");
        }
        acquire_info_rate_capacity(info_url, info_weight).await;
        if metadata {
            tracing::info!(%info_type, attempt, "metadata: waiting cooldown");
        }
        wait_info_cooldown_if_needed(info_url).await;
        if metadata {
            tracing::info!(%info_type, attempt, "metadata: HTTP send");
        }
        let request_result = client
            .post(info_url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call {info_url}"));

        match request_result {
            Ok(response) => {
                let status = response.status();
                if metadata {
                    tracing::info!(%info_type, %status, "metadata: HTTP response");
                }
                if status.is_success() {
                    match response
                        .json::<T>()
                        .await
                        .context("failed to parse Hyperliquid info response")
                    {
                        Ok(parsed) => {
                            if metadata {
                                tracing::info!(%info_type, "metadata: parsed");
                            }
                            return Ok(parsed);
                        }
                        Err(error) => {
                            if metadata {
                                tracing::warn!(%info_type, error=%error, "metadata: parse failed");
                            }
                            last_error = Some(error);
                            if attempt < INFO_MAX_ATTEMPTS {
                                let delay_ms =
                                    retry_delay_ms(attempt, None, parse_retry_after_ms(None));
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                continue;
                            }
                        }
                    }
                } else {
                    let retry_after_ms = parse_retry_after_ms(Some(response.headers()));
                    let response_body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "<unreadable response body>".to_string());
                    let response_excerpt = response_body.chars().take(240).collect::<String>();
                    let status_error = anyhow::anyhow!(
                        "{info_url} returned status {}{}",
                        status,
                        if response_excerpt.is_empty() {
                            "".to_string()
                        } else {
                            format!(": {response_excerpt}")
                        }
                    );

                    if should_retry_info_status(status) && attempt < INFO_MAX_ATTEMPTS {
                        let delay_ms = retry_delay_ms(attempt, Some(status), retry_after_ms);
                        let cooldown_ms = if status == StatusCode::TOO_MANY_REQUESTS {
                            delay_ms.max(30_000)
                        } else {
                            delay_ms
                        };
                        if matches!(
                            status,
                            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                        ) {
                            set_info_cooldown(info_url, cooldown_ms);
                        }
                        tracing::warn!(
                            %status,
                            info_type = %info_type,
                            info_weight,
                            attempt,
                            max_attempts = INFO_MAX_ATTEMPTS,
                            delay_ms,
                            cooldown_ms,
                            retry_after_ms,
                            "Hyperliquid info request failed; retrying with backoff"
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    last_error = Some(status_error);
                }
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < INFO_MAX_ATTEMPTS {
                    let delay_ms = retry_delay_ms(attempt, None, None);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    continue;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("unknown info request error"))).with_context(
        || format!("{info_url} info request failed after {INFO_MAX_ATTEMPTS} attempts"),
    )
}

fn info_request_type(body: &serde_json::Value) -> String {
    body.get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn info_request_weight(body: &serde_json::Value) -> u32 {
    match body.get("type").and_then(|value| value.as_str()) {
        Some(
            "l2Book"
            | "allMids"
            | "clearinghouseState"
            | "orderStatus"
            | "spotClearinghouseState"
            | "exchangeStatus",
        ) => 2,
        Some("meta" | "perpDexs" | "metaAndAssetCtxs") => 20,
        Some(_) | None => 20,
    }
}

async fn acquire_info_rate_capacity(info_url: &str, weight: u32) {
    let weight = weight.clamp(1, INFO_RATE_LIMIT_WEIGHT_PER_MIN);
    let endpoint = info_url.trim().to_ascii_lowercase();
    let background = BACKGROUND_INFO.try_with(|v| *v).unwrap_or(false);
    loop {
        let sleep_ms = {
            let now = crate::domain::now_ms();
            let Ok(mut windows) = INFO_RATE_WINDOWS.lock() else {
                return tokio::time::sleep(Duration::from_millis(250)).await;
            };
            let window = windows.entry(endpoint.clone()).or_default();
            if reserve_info_capacity(window, now, weight, background) {
                None
            } else {
                window
                    .entries
                    .front()
                    .map(|(timestamp_ms, _, _)| {
                        timestamp_ms
                            .saturating_add(INFO_RATE_WINDOW_MS)
                            .saturating_add(50)
                            .saturating_sub(now)
                            .clamp(50, 5_000)
                    })
                    .or(Some(250))
            }
        };
        if let Some(sleep_ms) = sleep_ms {
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        } else {
            return;
        }
    }
}

fn prune_info_rate_window(window: &mut InfoRateWindow, now_ms: u64) {
    while let Some((timestamp_ms, weight, background)) = window.entries.front().copied() {
        if now_ms.saturating_sub(timestamp_ms) < INFO_RATE_WINDOW_MS {
            break;
        }
        window.entries.pop_front();
        window.used_weight = window.used_weight.saturating_sub(weight);
        if background {
            window.background_weight = window.background_weight.saturating_sub(weight);
        }
    }
}

fn should_retry_info_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status == StatusCode::SERVICE_UNAVAILABLE
        || status == StatusCode::GATEWAY_TIMEOUT
        || status == StatusCode::BAD_GATEWAY
        || status.is_server_error()
}

fn parse_retry_after_ms(headers: Option<&HeaderMap>) -> Option<u64> {
    let headers = headers?;
    let retry_after = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds = retry_after.parse::<u64>().ok()?;
    let millis = seconds.saturating_mul(1_000);
    Some(millis.clamp(1_000, INFO_GLOBAL_COOLDOWN_CAP_MS))
}

fn retry_delay_ms(attempt: usize, status: Option<StatusCode>, retry_after_ms: Option<u64>) -> u64 {
    let exponent = attempt.saturating_sub(1).min(8) as u32;
    let exp_backoff = INFO_BASE_BACKOFF_MS
        .saturating_mul(2_u64.saturating_pow(exponent))
        .min(INFO_MAX_BACKOFF_MS);
    let status_bias = if matches!(status, Some(StatusCode::TOO_MANY_REQUESTS)) {
        600
    } else {
        0
    };
    let jitter = (crate::domain::now_ms() % 251).saturating_add((attempt as u64 * 17) % 89);
    exp_backoff
        .saturating_add(status_bias)
        .saturating_add(jitter)
        .max(retry_after_ms.unwrap_or(0))
        .min(INFO_GLOBAL_COOLDOWN_CAP_MS)
}

fn set_info_cooldown(info_url: &str, delay_ms: u64) {
    let delay = delay_ms.min(INFO_GLOBAL_COOLDOWN_CAP_MS);
    let until = crate::domain::now_ms().saturating_add(delay);
    if let Ok(mut cooldowns) = INFO_COOLDOWN_UNTIL_MS.lock() {
        let current = cooldowns
            .entry(info_url.trim().to_ascii_lowercase())
            .or_default();
        *current = (*current).max(until);
    }
}

async fn wait_info_cooldown_if_needed(info_url: &str) {
    let now = crate::domain::now_ms();
    let cooldown_until = INFO_COOLDOWN_UNTIL_MS
        .lock()
        .ok()
        .and_then(|cooldowns| {
            cooldowns
                .get(&info_url.trim().to_ascii_lowercase())
                .copied()
        })
        .unwrap_or(0);
    if cooldown_until > now {
        tokio::time::sleep(Duration::from_millis(cooldown_until.saturating_sub(now))).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cold_account_seeds_leave_capacity_for_sdk_mapping_without_raising_total() {
        let mut window = super::InfoRateWindow::default();
        for _ in 0..7 {
            assert!(super::reserve_info_capacity(&mut window, 1000, 20, true));
        }
        assert!(!super::reserve_info_capacity(&mut window, 1000, 20, true));
        // Two markets can still each fetch perpDexs/meta/metaAndAssetCtxs.
        for _ in 0..6 {
            assert!(super::reserve_info_capacity(&mut window, 1000, 20, false));
        }
        assert_eq!(window.used_weight, 260);
        assert!(super::reserve_info_capacity(&mut window, 1000, 40, false));
        assert!(!super::reserve_info_capacity(&mut window, 1000, 1, false));
        assert!(!super::reserve_info_capacity(&mut window, 60999, 20, true));
        assert!(super::reserve_info_capacity(&mut window, 61000, 20, true));
        assert_eq!(window.used_weight, 20);
        assert_eq!(window.background_weight, 20);
    }
    #[tokio::test]
    async fn sdk_mapping_singleflight_is_per_market_and_releases_on_cancel() {
        let xyz = super::xyz_snapshot_flight("test:xyz");
        let same = super::xyz_snapshot_flight("test:xyz");
        assert!(std::sync::Arc::ptr_eq(&xyz, &same));
        let held = xyz.lock().await;
        let io = super::xyz_snapshot_flight("test:io");
        let _io = io.try_lock().expect("unrelated market cannot block io");
        assert!(same.try_lock().is_err());
        drop(held);
        assert!(same.try_lock().is_ok());
        assert!(!super::BACKGROUND_INFO.try_with(|v| *v).unwrap_or(false));
        super::background_info(async {
            assert!(super::BACKGROUND_INFO.with(|v| *v));
        })
        .await;
        assert!(!super::BACKGROUND_INFO.try_with(|v| *v).unwrap_or(false));
    }
    use super::{
        DexAssetContext, DexAssetMeta, DexMeta, SpotAssetContext, SpotMarketSnapshot,
        SpotMetaAsset, SpotMetaResponse, SpotTokenInfo, XyzMarketSnapshot, build_order_plan,
        hip3_asset_id, info_request_type, info_request_weight, normalize_cloid_for_info,
        normalize_dex_coin, parse_spot_clearinghouse_state_value, round_perp_price,
        round_size_down,
    };
    use std::collections::HashMap;

    #[test]
    fn xyz_symbol_normalization_adds_prefix_and_uppercases() {
        assert_eq!(normalize_dex_coin("xyz", "tsla"), "xyz:TSLA");
        assert_eq!(normalize_dex_coin("xyz", "XYZ:nvda"), "xyz:NVDA");
    }

    #[test]
    fn info_request_weight_matches_documented_heavy_defaults() {
        assert_eq!(
            info_request_type(&serde_json::json!({"type": "allMids"})),
            "allMids"
        );
        assert_eq!(
            info_request_weight(&serde_json::json!({"type": "allMids"})),
            2
        );
        assert_eq!(
            info_request_weight(&serde_json::json!({"type": "clearinghouseState"})),
            2
        );
        assert_eq!(
            info_request_weight(&serde_json::json!({"type": "spotClearinghouseState"})),
            2
        );
        assert_eq!(
            info_request_weight(&serde_json::json!({"type": "userFills"})),
            20
        );
        assert_eq!(
            info_request_weight(&serde_json::json!({"type": "meta"})),
            20
        );
        assert_eq!(info_request_weight(&serde_json::json!({})), 20);
    }

    #[test]
    fn hip3_asset_ids_follow_builder_dex_formula() {
        assert_eq!(hip3_asset_id(1, 0), 110_000);
        assert_eq!(hip3_asset_id(1, 42), 110_042);
    }

    #[test]
    fn spot_candle_coin_uses_official_at_index_except_purr() {
        let purr_context = SpotAssetContext {
            day_ntl_vlm: "1".to_string(),
            mark_px: "1".to_string(),
            mid_px: Some("1".to_string()),
            prev_day_px: "1".to_string(),
            circulating_supply: None,
            coin: "PURR/USDC".to_string(),
        };
        let hype_context = SpotAssetContext {
            day_ntl_vlm: "1".to_string(),
            mark_px: "1".to_string(),
            mid_px: Some("1".to_string()),
            prev_day_px: "1".to_string(),
            circulating_supply: None,
            coin: "@107".to_string(),
        };
        let mut asset_contexts = vec![purr_context.clone(); 108];
        asset_contexts[0] = purr_context;
        asset_contexts[107] = hype_context;
        let snapshot = SpotMarketSnapshot {
            meta: SpotMetaResponse {
                universe: vec![
                    SpotMetaAsset {
                        name: "PURR/USDC".to_string(),
                        tokens: vec![1, 0],
                        index: 0,
                    },
                    SpotMetaAsset {
                        name: "@107".to_string(),
                        tokens: vec![150, 0],
                        index: 107,
                    },
                ],
                tokens: vec![
                    SpotTokenInfo {
                        name: "USDC".to_string(),
                        sz_decimals: 2,
                        index: 0,
                    },
                    SpotTokenInfo {
                        name: "PURR".to_string(),
                        sz_decimals: 0,
                        index: 1,
                    },
                    SpotTokenInfo {
                        name: "HYPE".to_string(),
                        sz_decimals: 2,
                        index: 150,
                    },
                ],
            },
            asset_contexts,
        };

        assert_eq!(snapshot.candle_coin("PURR/USDC").unwrap(), "PURR/USDC");
        assert_eq!(snapshot.candle_coin("HYPE/USDC").unwrap(), "@107");
    }

    #[test]
    fn perp_precision_rounds_price_and_size() {
        assert_eq!(round_size_down(0.123456, 3), 0.123);
        assert_eq!(round_size_down(0.0029, 4), 0.0029);
        assert_eq!(round_size_down(0.0029 - 1e-10, 4), 0.0028);
        assert_eq!(round_perp_price(123.4567, 3), 123.46);
        assert_eq!(round_perp_price(0.1234567, 4), 0.12);
    }

    #[test]
    fn order_plan_uses_reference_price_for_size_and_slippage_price_for_limit() {
        let snapshot = XyzMarketSnapshot {
            dex: "xyz".to_string(),
            dex_index: 1,
            meta: DexMeta {
                universe: vec![DexAssetMeta {
                    name: "xyz:NVDA".to_string(),
                    sz_decimals: 3,
                    max_leverage: Some(20),
                    only_isolated: None,
                    is_delisted: None,
                    margin_mode: None,
                }],
            },
            asset_contexts: vec![DexAssetContext {
                funding: None,
                open_interest: None,
                prev_day_px: Some("180.0".to_string()),
                day_ntl_vlm: None,
                premium: None,
                oracle_px: Some("181.0".to_string()),
                mark_px: Some("182.0".to_string()),
                mid_px: Some("183.0".to_string()),
                impact_pxs: None,
            }],
            coin_to_asset: HashMap::from([("xyz:NVDA".to_string(), 110_000)]),
        };

        let plan = build_order_plan(&snapshot, "NVDA", true, 1.0, None, 20.0).expect("order plan");

        assert_eq!(plan.coin, "xyz:NVDA");
        assert_eq!(plan.size, 0.005);
        assert!(plan.limit_price > plan.reference_price);
    }

    #[test]
    fn clearinghouse_state_parses_xyz_account_snapshot() {
        let raw = r#"{
            "marginSummary": {
                "accountValue": "123.4",
                "totalNtlPos": "50.0",
                "totalRawUsd": "123.4",
                "totalMarginUsed": "2.5"
            },
            "crossMarginSummary": {
                "accountValue": "123.4",
                "totalNtlPos": "50.0",
                "totalRawUsd": "123.4",
                "totalMarginUsed": "2.5"
            },
            "crossMaintenanceMarginUsed": "0.2",
            "withdrawable": "120.9",
            "assetPositions": [{
                "type": "oneWay",
                "position": {
                    "coin": "xyz:NVDA",
                    "szi": "0.004",
                    "entryPx": "212.0",
                    "positionValue": "0.85",
                    "unrealizedPnl": "0.01",
                    "returnOnEquity": "0.02",
                    "liquidationPx": null,
                    "marginUsed": "0.04",
                    "leverage": {
                        "type": "isolated",
                        "value": 6,
                        "rawUsd": "0.0"
                    },
                    "maxLeverage": 20
                }
            }],
            "time": 1780093627183
        }"#;

        let state: super::ClearinghouseState =
            serde_json::from_str(raw).expect("clearinghouse state");

        assert_eq!(state.margin_summary.account_value, "123.4");
        assert_eq!(state.withdrawable.as_deref(), Some("120.9"));
        assert_eq!(state.asset_positions[0].position.coin, "xyz:NVDA");
        assert_eq!(state.asset_positions[0].position.szi, "0.004");
        let leverage = state.asset_positions[0]
            .position
            .leverage
            .as_ref()
            .expect("position leverage");
        assert_eq!(leverage.leverage_type, "isolated");
        assert_eq!(leverage.value, Some(6));
        assert_eq!(leverage.raw_usd.as_deref(), Some("0.0"));
    }

    #[test]
    fn hip3_account_info_payloads_preserve_explicit_dex_scope() {
        let user = "0x0000000000000000000000000000000000000009";
        let payload = super::info_payload_with_optional_dex("clearinghouseState", user, " IO ");
        assert_eq!(payload["type"], "clearinghouseState");
        assert_eq!(payload["user"], user);
        assert_eq!(payload["dex"], "io");

        let default_payload = super::info_payload_with_optional_dex("openOrders", user, "   ");
        assert!(default_payload.get("dex").is_none());
    }

    #[test]
    fn user_rate_limit_parses_and_reports_remaining_capacity() {
        let raw = r#"{
            "cumVlm": "0.0",
            "nRequestsUsed": 42,
            "nRequestsCap": 10000,
            "nRequestsSurplus": -2
        }"#;

        let limit: super::UserRateLimit = serde_json::from_str(raw).expect("rate limit");

        assert_eq!(limit.cum_vlm, "0.0");
        assert_eq!(limit.request_capacity_remaining(), 9956);
    }

    #[test]
    fn order_status_parses_known_and_unknown_responses() {
        let known = r#"{
            "status": "order",
            "order": {
                "order": {
                    "coin": "xyz:NVDA",
                    "side": "B",
                    "limitPx": "180.0",
                    "sz": "0.005",
                    "oid": 123,
                    "timestamp": 1780093627183,
                    "triggerCondition": "N/A",
                    "isTrigger": false,
                    "triggerPx": "0.0",
                    "children": [],
                    "isPositionTpsl": false,
                    "reduceOnly": false,
                    "orderType": "Limit",
                    "origSz": "0.005",
                    "tif": "Ioc",
                    "cloid": "0x00000000000000000000000000000001"
                },
                "status": "filled",
                "statusTimestamp": 1780093627199
            }
        }"#;
        let known: super::OrderStatusResponse =
            serde_json::from_str(known).expect("known order status");
        assert_eq!(known.status, "order");
        assert_eq!(known.order.as_ref().expect("order").status, "filled");
        assert_eq!(known.order.expect("order").order.oid, 123);

        let unknown: super::OrderStatusResponse =
            serde_json::from_str(r#"{"status":"unknownOid"}"#).expect("unknown order status");
        assert_eq!(unknown.status, "unknownOid");
        assert!(unknown.order.is_none());
    }

    #[test]
    fn cloid_normalization_matches_hyperliquid_hex_format() {
        let normalized =
            normalize_cloid_for_info("00000000-0000-0000-0000-000000000001").expect("uuid cloid");
        assert_eq!(normalized, "0x00000000000000000000000000000001");
        assert_eq!(
            normalize_cloid_for_info("ABCDEF0123456789ABCDEF0123456789").expect("hex cloid"),
            "0xabcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn spot_clearinghouse_state_parses_null_and_balances() {
        let empty = parse_spot_clearinghouse_state_value(serde_json::Value::Null)
            .expect("null spot state should default");
        assert!(empty.balances.is_empty());

        let raw = serde_json::json!({
            "balances": [{
                "coin": "USDC",
                "token": 0,
                "total": "12.5",
                "hold": "1.5",
                "entryNtl": "0"
            }]
        });
        let state = parse_spot_clearinghouse_state_value(raw).expect("spot state");
        assert_eq!(state.balances.len(), 1);
        assert_eq!(state.balances[0].coin, "USDC");
        assert_eq!(state.balances[0].total, "12.5");
        assert_eq!(state.balances[0].hold, "1.5");
    }
}
