//! Lighter protocol adapter for the Robinhood Chain deployment.
//!
//! This module deliberately keeps live signing outside the initial adapter.
//! Read-only market/account access, public trade streaming, deterministic order
//! planning and submission of an already-signed transaction are implemented in
//! Rust. A transaction must be produced by a signer that has independently
//! passed the official Lighter signing vectors before it can reach the submit
//! method.

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{FutureExt, SinkExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls, connect_async,
    tungstenite::{Message, handshake::client::Response},
};

const MAINNET_HTTP_URL: &str = "https://mainnet.zklighter.elliot.ai";
const MAINNET_WS_URL: &str = "wss://mainnet.zklighter.elliot.ai/stream";
const TESTNET_HTTP_URL: &str = "https://testnet.zklighter.elliot.ai";
const TESTNET_WS_URL: &str = "wss://testnet.zklighter.elliot.ai/stream";
const ROBINHOOD_HTTP_URL: &str = "https://api.rh.lighter.xyz";
const ROBINHOOD_WS_URL: &str = "wss://api.rh.lighter.xyz/stream";
const ROBINHOOD_TESTNET_HTTP_URL: &str = "https://api.rh-testnet.lighter.xyz";
const ROBINHOOD_TESTNET_WS_URL: &str = "wss://api.rh-testnet.lighter.xyz/stream";
const REQUEST_TIMEOUT_SECS: u64 = 15;
const WS_RECONNECT_BASE_MS: u64 = 500;
const WS_RECONNECT_MAX_MS: u64 = 15_000;
const WS_CONNECT_TIMEOUT_MS: u64 = 4_000;
pub const LIGHTER_PROXY_ENV: &str = "TRADE_XYZ_LIGHTER_PROXY_URL";
pub const LIGHTER_WS_PROXIES_ENV: &str = "TRADE_XYZ_LIGHTER_WS_PROXY_URLS";

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalHttpProxy {
    url: String,
    host: String,
    port: u16,
}

fn parse_local_http_proxy(value: &str) -> Result<LocalHttpProxy> {
    let url = reqwest::Url::parse(value.trim()).context("invalid Lighter proxy URL")?;
    ensure!(url.scheme() == "http", "Lighter proxy must use http://");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Lighter proxy credentials are not allowed"
    );
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "Lighter proxy URL must not contain a path, query, or fragment"
    );
    let host = url
        .host_str()
        .context("Lighter proxy URL is missing a host")?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    ensure!(
        matches!(host, "127.0.0.1" | "localhost" | "::1"),
        "Lighter proxy must be bound to localhost"
    );
    let port = url
        .port()
        .context("Lighter proxy URL must include an explicit port")?;
    Ok(LocalHttpProxy {
        url: url.to_string(),
        host: host.to_string(),
        port,
    })
}

fn configured_lighter_proxy() -> Result<Option<LocalHttpProxy>> {
    let Some(value) = std::env::var_os(LIGHTER_PROXY_ENV) else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{LIGHTER_PROXY_ENV} is not valid UTF-8"))?;
    if value.trim().is_empty() {
        return Ok(None);
    }
    parse_local_http_proxy(&value).map(Some)
}

fn parse_local_http_proxy_list(value: &str) -> Result<Vec<LocalHttpProxy>> {
    let values = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    ensure!(
        (1..=8).contains(&values.len()),
        "Lighter websocket proxy list must contain 1..=8 local endpoints"
    );
    let mut proxies = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();
    for value in values {
        let proxy = parse_local_http_proxy(value)?;
        ensure!(
            seen.insert(proxy.url.clone()),
            "Lighter websocket proxy list contains a duplicate endpoint"
        );
        proxies.push(proxy);
    }
    Ok(proxies)
}

fn configured_lighter_ws_proxies() -> Result<Vec<LocalHttpProxy>> {
    if let Some(value) = std::env::var_os(LIGHTER_WS_PROXIES_ENV) {
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("{LIGHTER_WS_PROXIES_ENV} is not valid UTF-8"))?;
        return parse_local_http_proxy_list(&value);
    }
    Ok(configured_lighter_proxy()?.into_iter().collect())
}

pub async fn connect_lighter_websocket(
    ws_url: &str,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response)> {
    let proxies = configured_lighter_ws_proxies()?;
    if proxies.is_empty() {
        return tokio::time::timeout(
            Duration::from_millis(WS_CONNECT_TIMEOUT_MS),
            connect_async(ws_url),
        )
        .await
        .context("Lighter direct websocket connection exceeded 4000ms")?
        .context("failed to connect to Lighter direct websocket");
    }
    let attempts = proxies
        .into_iter()
        .map(|proxy| {
            let label = proxy.url.clone();
            (
                label,
                connect_lighter_websocket_via_proxy(ws_url, proxy).boxed(),
            )
        })
        .collect::<Vec<_>>();
    race_lighter_connections(attempts, Duration::from_millis(WS_CONNECT_TIMEOUT_MS)).await
}

// Bound the entire TCP + CONNECT + TLS + websocket attempt, including a silent
// TLS peer. A successful route drops all pending routes; failures retain every
// local endpoint so correlated outages can be distinguished from one bad route.
async fn race_lighter_connections<T>(
    attempts: Vec<(String, BoxFuture<'_, Result<T>>)>,
    deadline: Duration,
) -> Result<T> {
    let mut pending = attempts
        .into_iter()
        .map(|(label, attempt)| async move {
            let result = tokio::time::timeout(deadline, attempt)
                .await
                .with_context(|| {
                    format!("connection deadline exceeded ({}ms)", deadline.as_millis())
                })
                .and_then(|result| result);
            (label, result)
        })
        .collect::<FuturesUnordered<_>>();
    let mut errors = Vec::new();
    while let Some((label, result)) = pending.next().await {
        match result {
            Ok(connection) => return Ok(connection),
            Err(error) => errors.push(format!("{label}: {error:#}")),
        }
    }
    errors.sort();
    bail!(
        "all configured local Lighter websocket proxies failed: {}",
        errors.join("; ")
    )
}

async fn connect_lighter_websocket_via_proxy(
    ws_url: &str,
    proxy: LocalHttpProxy,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response)> {
    let target = reqwest::Url::parse(ws_url).context("invalid Lighter websocket URL")?;
    ensure!(
        target.scheme() == "wss",
        "Lighter websocket must use wss://"
    );
    let host = target
        .host_str()
        .context("Lighter websocket URL is missing a host")?;
    let port = target.port().unwrap_or(443);
    let mut socket = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((proxy.host.as_str(), proxy.port)),
    )
    .await
    .context("timed out connecting to local Lighter proxy")?
    .with_context(|| format!("failed to connect to local Lighter proxy {}", proxy.url))?;
    let connect = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    socket
        .write_all(connect.as_bytes())
        .await
        .context("failed to send Lighter proxy CONNECT request")?;
    let mut response = Vec::with_capacity(1024);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut chunk = [0_u8; 512];
        loop {
            let read = socket
                .read(&mut chunk)
                .await
                .context("failed to read Lighter proxy CONNECT response")?;
            ensure!(read > 0, "local Lighter proxy closed the CONNECT tunnel");
            response.extend_from_slice(&chunk[..read]);
            ensure!(
                response.len() <= 8 * 1024,
                "local Lighter proxy CONNECT response is too large"
            );
            if response.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await
    .context("timed out establishing Lighter proxy CONNECT tunnel")??;
    let status_line = std::str::from_utf8(&response)
        .context("local Lighter proxy returned non-UTF-8 headers")?
        .lines()
        .next()
        .unwrap_or_default();
    ensure!(
        status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200"),
        "local Lighter proxy rejected CONNECT tunnel: {status_line}"
    );
    client_async_tls(ws_url, socket)
        .await
        .with_context(|| format!("failed TLS/WebSocket handshake with {ws_url} via local proxy"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LighterEnvironment {
    Mainnet,
    Testnet,
    Robinhood,
    RobinhoodTestnet,
}

impl FromStr for LighterEnvironment {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mainnet" => Ok(Self::Mainnet),
            "testnet" => Ok(Self::Testnet),
            "robinhood" | "rh" => Ok(Self::Robinhood),
            "robinhood_testnet" | "robinhood-testnet" | "rh_testnet" | "rh-testnet" => {
                Ok(Self::RobinhoodTestnet)
            }
            other => bail!(
                "unsupported Lighter environment {other}; use robinhood, robinhood_testnet, mainnet, or testnet"
            ),
        }
    }
}

impl fmt::Display for LighterEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mainnet => formatter.write_str("mainnet"),
            Self::Testnet => formatter.write_str("testnet"),
            Self::Robinhood => formatter.write_str("robinhood"),
            Self::RobinhoodTestnet => formatter.write_str("robinhood_testnet"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LighterEndpoints {
    pub http_url: String,
    pub ws_url: String,
    pub chain_id: i64,
}

impl LighterEndpoints {
    pub fn official(environment: LighterEnvironment) -> Self {
        match environment {
            LighterEnvironment::Mainnet => Self {
                http_url: MAINNET_HTTP_URL.to_string(),
                ws_url: MAINNET_WS_URL.to_string(),
                chain_id: 304,
            },
            LighterEnvironment::Testnet => Self {
                http_url: TESTNET_HTTP_URL.to_string(),
                ws_url: TESTNET_WS_URL.to_string(),
                chain_id: 300,
            },
            LighterEnvironment::Robinhood => Self {
                http_url: ROBINHOOD_HTTP_URL.to_string(),
                ws_url: ROBINHOOD_WS_URL.to_string(),
                chain_id: 466_324,
            },
            LighterEnvironment::RobinhoodTestnet => Self {
                http_url: ROBINHOOD_TESTNET_HTTP_URL.to_string(),
                ws_url: ROBINHOOD_TESTNET_WS_URL.to_string(),
                chain_id: 300,
            },
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.http_url.starts_with("https://"),
            "Lighter HTTP endpoint must use https"
        );
        ensure!(
            self.ws_url.starts_with("wss://"),
            "Lighter websocket endpoint must use wss"
        );
        ensure!(
            self.chain_id > 0,
            "Lighter signing chain id must be positive"
        );
        Ok(())
    }

    /// Lighter documents `readonly=true` for market-data access from regions
    /// where state-changing access is unavailable. It must never be used by the
    /// signed transaction submit path.
    pub fn public_readonly_ws_url(&self) -> String {
        if self.ws_url.contains('?') {
            format!("{}&readonly=true", self.ws_url)
        } else {
            format!("{}?readonly=true", self.ws_url)
        }
    }
}

#[derive(Debug, Clone)]
pub struct LighterClient {
    environment: LighterEnvironment,
    endpoints: LighterEndpoints,
    http: Client,
}

impl LighterClient {
    pub fn official(environment: LighterEnvironment) -> Result<Self> {
        Self::new(environment, LighterEndpoints::official(environment))
    }

    pub fn new(environment: LighterEnvironment, endpoints: LighterEndpoints) -> Result<Self> {
        endpoints.validate()?;
        let mut builder = Client::builder().timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS));
        if let Some(proxy) = configured_lighter_proxy()? {
            builder = builder.proxy(
                reqwest::Proxy::all(&proxy.url)
                    .context("failed to configure Lighter HTTP proxy")?,
            );
        }
        let http = builder
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .context("failed to build Lighter HTTP client")?;
        Ok(Self {
            environment,
            endpoints,
            http,
        })
    }

    pub fn environment(&self) -> LighterEnvironment {
        self.environment
    }

    pub fn endpoints(&self) -> &LighterEndpoints {
        &self.endpoints
    }

    /// Read-only raw account evidence preserves margin/fee fields omitted by older typed views.
    pub async fn inventory_account_evidence(&self, account_index: i64) -> Result<Value> {
        ensure!(account_index >= 0, "invalid account index");
        let value = self
            .http
            .get(format!("{}/api/v1/account", self.endpoints.http_url))
            .query(&[("by", "index"), ("value", &account_index.to_string())])
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        ensure_api_success(
            value["code"]
                .as_i64()
                .context("missing account response code")?,
            value["message"].as_str(),
        )?;
        Ok(value)
    }

    pub async fn inventory_funding_page(
        &self,
        auth: &str,
        account_index: i64,
        market: i32,
        cursor: Option<String>,
        start_ms: u64,
    ) -> Result<Value> {
        let mut query = vec![
            ("account_index", account_index.to_string()),
            ("market_id", market.to_string()),
            ("limit", "100".into()),
            ("start_timestamp", (start_ms / 1000).to_string()),
        ];
        if let Some(c) = cursor {
            query.push(("cursor", c));
        }
        self.authenticated_account_get("/api/v1/positionFunding", auth, query)
            .await
    }

    pub async fn inventory_history_page(
        &self,
        auth: &str,
        account: i64,
        market: i32,
        trades: bool,
        cursor: Option<String>,
    ) -> Result<Value> {
        let mut query = vec![
            ("account_index", account.to_string()),
            ("market_id", market.to_string()),
            ("limit", "100".into()),
        ];
        if trades {
            query.push(("sort_by", "timestamp".into()));
            query.push(("sort_dir", "desc".into()));
        }
        if let Some(c) = cursor {
            query.push(("cursor", c));
        }
        self.authenticated_account_get(
            if trades {
                "/api/v1/trades"
            } else {
                "/api/v1/accountInactiveOrders"
            },
            auth,
            query,
        )
        .await
    }

    /// Fresh server-dated account evidence for a signed, expired inventory request.
    pub async fn inventory_dated_account(&self, account: i64) -> Result<(u64, Value)> {
        let response = self
            .http
            .get(format!("{}/api/v1/account", self.endpoints.http_url))
            .query(&[("by", "index".to_owned()), ("value", account.to_string())])
            .header("cache-control", "no-cache")
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response
                .headers()
                .get("age")
                .and_then(|x| x.to_str().ok())
                .and_then(|x| x.parse::<u64>().ok())
                .unwrap_or(0)
                == 0,
            "cached account evidence"
        );
        let date = response
            .headers()
            .get("date")
            .context("missing server date")?
            .to_str()?;
        let at = chrono::DateTime::parse_from_rfc2822(date)?.timestamp_millis();
        ensure!(at > 0, "invalid server date");
        let body: Value = response.json().await?;
        ensure!(
            body["code"].as_i64() == Some(200),
            "account evidence rejected"
        );
        Ok((at as u64, body))
    }

    pub async fn markets(&self) -> Result<Vec<LighterMarket>> {
        let url = format!("{}/api/v1/orderBookDetails", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter market endpoint {url} returned an error"))?
            .json::<MarketResponse>()
            .await
            .context("failed to decode Lighter market response")?;
        ensure_api_success(i64::from(response.code), response.message.as_deref())?;
        Ok(response.order_book_details)
    }

    pub async fn market_by_symbol(&self, symbol: &str) -> Result<LighterMarket> {
        let normalized = symbol.trim().to_ascii_uppercase();
        ensure!(!normalized.is_empty(), "market symbol is required");
        self.markets()
            .await?
            .into_iter()
            .find(|market| market.symbol.eq_ignore_ascii_case(&normalized))
            .with_context(|| format!("Lighter market {normalized} was not found"))
    }

    /// Fetch one public market row without downloading the full catalog. This
    /// is used only for bounded read-only market-state reconciliation.
    pub async fn market_by_id(&self, market_id: i32) -> Result<LighterMarket> {
        ensure!(market_id >= 0, "market id must be non-negative");
        let url = format!("{}/api/v1/orderBookDetails", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .query(&[("market_id", market_id)])
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter market endpoint {url} returned an error"))?
            .json::<MarketResponse>()
            .await
            .context("failed to decode Lighter market response")?;
        ensure_api_success(i64::from(response.code), response.message.as_deref())?;
        response
            .order_book_details
            .into_iter()
            .find(|market| market.market_id == market_id)
            .with_context(|| format!("Lighter market id {market_id} was not found"))
    }

    /// Returns Lighter's current funding-rate observation for one market.
    /// The public endpoint also contains external reference exchanges; only
    /// the row explicitly labeled `lighter` is accepted here.
    pub async fn funding_rate(&self, market_id: i32) -> Result<f64> {
        ensure!(market_id >= 0, "market_id must be non-negative");
        let url = format!("{}/api/v1/funding-rates", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter funding-rate endpoint {url} returned an error"))?
            .json::<LighterFundingRatesResponse>()
            .await
            .context("failed to decode Lighter funding-rate response")?;
        ensure_api_success(i64::from(response.code), response.message.as_deref())?;
        let rate = response
            .funding_rates
            .into_iter()
            .find(|row| row.market_id == market_id && row.exchange.eq_ignore_ascii_case("lighter"))
            .with_context(|| format!("Lighter funding rate for market {market_id} was not found"))?
            .rate;
        ensure!(
            rate.is_finite() && (-0.01..=0.01).contains(&rate),
            "Lighter funding rate is outside the bounded range"
        );
        Ok(rate)
    }

    pub async fn order_book(&self, market_id: i32, limit: u32) -> Result<LighterOrderBook> {
        ensure!(market_id >= 0, "market_id must be non-negative");
        ensure!(
            (1..=1_000).contains(&limit),
            "order book limit must be between 1 and 1000"
        );
        let url = format!("{}/api/v1/orderBookOrders", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .query(&[
                ("market_id", market_id.to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter order book endpoint {url} returned an error"))?
            .json::<LighterOrderBook>()
            .await
            .context("failed to decode Lighter order book response")?;
        ensure_api_success(response.code, response.message.as_deref())?;
        Ok(response)
    }

    pub async fn accounts_by_l1_address(&self, l1_address: &str) -> Result<Vec<LighterAccount>> {
        let normalized = normalize_evm_address(l1_address)?;
        let url = format!("{}/api/v1/accountsByL1Address", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .query(&[("l1_address", normalized.as_str())])
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter account endpoint {url} returned an error"))?
            .json::<AccountResponse>()
            .await
            .context("failed to decode Lighter account response")?;
        ensure_api_success(response.code, response.message.as_deref())?;
        Ok(response.sub_accounts)
    }

    /// Returns the detailed account row, including collateral and available
    /// balance, for a previously verified account index.
    pub async fn account_by_index(&self, account_index: i64) -> Result<LighterAccount> {
        ensure!(account_index >= 0, "account_index must be non-negative");
        let url = format!("{}/api/v1/account", self.endpoints.http_url);
        let account_index_value = account_index.to_string();
        let response = self
            .http
            .get(&url)
            .query(&[("by", "index"), ("value", account_index_value.as_str())])
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter detailed account endpoint {url} returned an error"))?
            .json::<DetailedAccountResponse>()
            .await
            .context("failed to decode Lighter detailed account response")?;
        ensure_api_success(response.code, response.message.as_deref())?;
        response
            .accounts
            .into_iter()
            .find(|account| account.index == account_index)
            .context("Lighter detailed account response did not contain the requested index")
    }

    /// Returns the exchange-registered public key and nonce for one API key.
    /// Used as a mandatory signer/account preflight before mainnet submission.
    pub async fn api_key(&self, account_index: i64, api_key_index: u8) -> Result<LighterApiKey> {
        ensure!(account_index >= 0, "account_index must be non-negative");
        ensure!(
            api_key_index <= 254,
            "api_key_index must be between 0 and 254"
        );
        let url = format!("{}/api/v1/apikeys", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .query(&[
                ("account_index", account_index.to_string()),
                ("api_key_index", api_key_index.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter API-key endpoint {url} returned an error"))?
            .json::<LighterApiKeyResponse>()
            .await
            .context("failed to decode Lighter API-key response")?;
        ensure_api_success(i64::from(response.code), response.message.as_deref())?;
        ensure!(
            response.api_keys.len() == 1,
            "Lighter returned {} API keys for an exact account/key lookup",
            response.api_keys.len()
        );
        let key = response
            .api_keys
            .into_iter()
            .next()
            .expect("length checked");
        ensure!(
            key.account_index == account_index && key.api_key_index == api_key_index,
            "Lighter returned a different account/API-key pair"
        );
        Ok(key)
    }

    /// Returns the next nonce expected by Lighter for one account/API-key pair.
    /// Every API key owns an independent strictly increasing nonce sequence.
    pub async fn next_nonce(&self, account_index: i64, api_key_index: u8) -> Result<i64> {
        ensure!(account_index >= 0, "account_index must be non-negative");
        ensure!(
            api_key_index <= 254,
            "api_key_index must be between 0 and 254"
        );
        let url = format!("{}/api/v1/nextNonce", self.endpoints.http_url);
        let response = self
            .http
            .get(&url)
            .query(&[
                ("account_index", account_index.to_string()),
                ("api_key_index", api_key_index.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?
            .error_for_status()
            .with_context(|| format!("Lighter nextNonce endpoint {url} returned an error"))?
            .json::<LighterNextNonceResponse>()
            .await
            .context("failed to decode Lighter nextNonce response")?;
        ensure_api_success(i64::from(response.code), response.message.as_deref())?;
        ensure!(
            response.nonce >= 0,
            "Lighter returned a negative next nonce"
        );
        Ok(response.nonce)
    }

    /// Authenticated startup snapshot of currently active account orders.
    pub async fn account_active_orders(
        &self,
        auth_token: &str,
        account_index: i64,
        market_id: i32,
    ) -> Result<Value> {
        self.authenticated_account_get(
            "/api/v1/accountActiveOrders",
            auth_token,
            vec![
                ("account_index", account_index.to_string()),
                ("market_id", market_id.to_string()),
            ],
        )
        .await
    }

    /// Authenticated bounded history used to resolve orders across restarts.
    pub async fn account_inactive_orders(
        &self,
        auth_token: &str,
        account_index: i64,
        market_id: i32,
        limit: u32,
    ) -> Result<Value> {
        ensure!(
            (1..=100).contains(&limit),
            "inactive order limit must be 1..=100"
        );
        self.authenticated_account_get(
            "/api/v1/accountInactiveOrders",
            auth_token,
            vec![
                ("account_index", account_index.to_string()),
                ("market_id", market_id.to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
    }

    /// Authenticated bounded fill history used for deterministic gap repair.
    pub async fn account_trades(
        &self,
        auth_token: &str,
        account_index: i64,
        market_id: i32,
        limit: u32,
    ) -> Result<Value> {
        ensure!((1..=100).contains(&limit), "trade limit must be 1..=100");
        self.authenticated_account_get(
            "/api/v1/trades",
            auth_token,
            vec![
                ("account_index", account_index.to_string()),
                ("market_id", market_id.to_string()),
                ("sort_by", "timestamp".to_string()),
                ("sort_dir", "desc".to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
    }

    async fn authenticated_account_get(
        &self,
        path: &str,
        auth_token: &str,
        query: Vec<(&str, String)>,
    ) -> Result<Value> {
        ensure!(
            !auth_token.trim().is_empty(),
            "Lighter auth token is required"
        );
        let url = format!("{}{path}", self.endpoints.http_url);
        let value = self
            .http
            .get(&url)
            .header("authorization", auth_token)
            .query(&query)
            .send()
            .await
            .with_context(|| format!("failed to call authenticated Lighter endpoint {url}"))?
            .error_for_status()
            .with_context(|| format!("authenticated Lighter endpoint {url} returned an error"))?
            .json::<Value>()
            .await
            .with_context(|| format!("failed to decode authenticated Lighter response {url}"))?;
        let code = value
            .get("code")
            .and_then(Value::as_i64)
            .context("authenticated Lighter response is missing numeric code")?;
        let message = value.get("message").and_then(Value::as_str);
        ensure_api_success(code, message)?;
        Ok(value)
    }

    /// Sends a transaction that was signed elsewhere and already contains the
    /// official Lighter `tx_type` and JSON encoded `tx_info` fields.
    ///
    /// This function cannot access a private key and cannot create a signature.
    /// The caller must enforce its own live confirmation gate.
    pub async fn submit_signed_transaction(
        &self,
        signed: &SignedLighterTransaction,
    ) -> Result<Value> {
        signed.validate()?;
        let url = format!("{}/api/v1/sendTx", self.endpoints.http_url);
        let response = self
            .http
            .post(&url)
            .form(&[
                ("tx_type", signed.tx_type.to_string()),
                ("tx_info", signed.tx_info.clone()),
                ("price_protection", "true".to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("failed to submit signed Lighter transaction to {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read Lighter sendTx response from {url}"))?;
        if !status.is_success() {
            return Err(LighterSubmitRejected {
                status: status.as_u16(),
                body: sanitize_submit_error_body(&body),
            }
            .into());
        }
        serde_json::from_str::<Value>(&body).context("failed to decode Lighter sendTx response")
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LighterNextNonceResponse {
    code: i32,
    #[serde(default)]
    message: Option<String>,
    nonce: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LighterApiKey {
    pub account_index: i64,
    pub api_key_index: u8,
    pub nonce: i64,
    pub public_key: String,
    #[serde(default)]
    pub transaction_time: i64,
}

#[derive(Debug, Deserialize)]
struct LighterApiKeyResponse {
    code: i32,
    #[serde(default)]
    message: Option<String>,
    api_keys: Vec<LighterApiKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterMarket {
    pub symbol: String,
    pub market_id: i32,
    #[serde(default)]
    pub market_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub taker_fee: String,
    #[serde(default)]
    pub is_taker_fee_enabled: bool,
    pub min_base_amount: String,
    pub min_quote_amount: String,
    pub supported_size_decimals: u32,
    pub supported_price_decimals: u32,
    #[serde(default)]
    pub size_decimals: u32,
    #[serde(default)]
    pub price_decimals: u32,
    pub mark_price: String,
    pub index_price: String,
    #[serde(default)]
    pub last_trade_price: Value,
    #[serde(default)]
    pub default_initial_margin_fraction: u32,
    #[serde(default)]
    pub min_initial_margin_fraction: u32,
    #[serde(default)]
    pub maintenance_margin_fraction: u32,
    #[serde(default)]
    pub market_config: Value,
}

impl LighterMarket {
    pub fn is_active_perp(&self) -> bool {
        self.status.eq_ignore_ascii_case("active") && self.market_type.eq_ignore_ascii_case("perp")
    }

    pub fn effective_size_decimals(&self) -> u32 {
        self.supported_size_decimals.max(self.size_decimals)
    }

    pub fn effective_price_decimals(&self) -> u32 {
        self.supported_price_decimals.max(self.price_decimals)
    }

    pub fn mark_price_f64(&self) -> Result<f64> {
        parse_positive_decimal(&self.mark_price, "mark_price")
    }

    pub fn min_quote_amount_f64(&self) -> Result<f64> {
        parse_positive_decimal(&self.min_quote_amount, "min_quote_amount")
    }

    pub fn taker_fee_rate_f64(&self) -> Result<f64> {
        if !self.is_taker_fee_enabled || self.taker_fee.trim().is_empty() {
            return Ok(0.0);
        }
        let rate = self.taker_fee.parse::<f64>().context("invalid taker_fee")?;
        ensure!(
            rate.is_finite() && (0.0..=0.05).contains(&rate),
            "taker_fee must be between 0 and 5 percent"
        );
        Ok(rate)
    }
}

#[derive(Debug, Deserialize)]
struct MarketResponse {
    code: i64,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    order_book_details: Vec<LighterMarket>,
}

#[derive(Debug, Deserialize)]
struct LighterFundingRatesResponse {
    code: i32,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    funding_rates: Vec<LighterFundingRateRow>,
}

#[derive(Debug, Deserialize)]
struct LighterFundingRateRow {
    market_id: i32,
    exchange: String,
    rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterAccount {
    pub index: i64,
    #[serde(default)]
    pub account_type: i32,
    pub l1_address: String,
    #[serde(default)]
    pub status: i32,
    #[serde(default)]
    pub collateral: String,
    #[serde(default)]
    pub available_balance: String,
    #[serde(default)]
    pub pending_order_count: u64,
    #[serde(default)]
    pub positions: Vec<LighterAccountPosition>,
    #[serde(default)]
    pub account_trading_mode: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterAccountPosition {
    pub market_id: i32,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub sign: i64,
    #[serde(default)]
    pub position: String,
    #[serde(default)]
    pub position_value: String,
    #[serde(default)]
    pub open_order_count: u32,
    #[serde(default)]
    pub pending_order_count: u32,
}

#[derive(Debug, Deserialize)]
struct AccountResponse {
    code: i64,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    sub_accounts: Vec<LighterAccount>,
}

#[derive(Debug, Deserialize)]
struct DetailedAccountResponse {
    code: i64,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    accounts: Vec<LighterAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterSimpleOrder {
    pub order_index: i64,
    #[serde(default)]
    pub order_id: String,
    #[serde(default)]
    pub owner_account_index: i64,
    #[serde(default)]
    pub initial_base_amount: String,
    pub remaining_base_amount: String,
    pub price: String,
    #[serde(default)]
    pub order_expiry: i64,
    #[serde(default)]
    pub transaction_time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterOrderBook {
    pub code: i64,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub total_asks: u64,
    #[serde(default)]
    pub asks: Vec<LighterSimpleOrder>,
    #[serde(default)]
    pub total_bids: u64,
    #[serde(default)]
    pub bids: Vec<LighterSimpleOrder>,
}

impl LighterOrderBook {
    pub fn best_price(&self, side: LighterSide) -> Result<f64> {
        let orders = match side {
            LighterSide::Buy => &self.asks,
            LighterSide::Sell => &self.bids,
        };
        let price = orders
            .first()
            .with_context(|| format!("Lighter order book has no liquidity for {side:?}"))?
            .price
            .as_str();
        parse_positive_decimal(price, "best order book price")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LighterSide {
    Buy,
    Sell,
}

impl FromStr for LighterSide {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "buy" | "long" | "bid" => Ok(Self::Buy),
            "sell" | "short" | "ask" => Ok(Self::Sell),
            other => bail!("unsupported side {other}; use buy or sell"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LighterOrderKind {
    Market,
    Limit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterOrderRequest {
    pub symbol: String,
    pub side: LighterSide,
    pub notional_usd: f64,
    pub kind: LighterOrderKind,
    #[serde(default)]
    pub limit_price: Option<f64>,
    #[serde(default)]
    pub reduce_only: bool,
    pub max_slippage_bps: f64,
    pub client_order_index: i64,
}

/// An order request expressed in Lighter's integer base-size units.
///
/// Cross-venue hedging must not derive size from a USD notional because that
/// can silently round one leg to a different quantity. The caller therefore
/// supplies the exact venue amount and the metadata precision it used. A
/// metadata mismatch is rejected instead of being rounded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterExactBaseOrderRequest {
    pub symbol: String,
    pub side: LighterSide,
    pub base_amount: i64,
    pub size_decimals: u32,
    pub kind: LighterOrderKind,
    #[serde(default)]
    pub limit_price: Option<f64>,
    #[serde(default)]
    pub reduce_only: bool,
    pub max_slippage_bps: f64,
    pub client_order_index: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterOrderPlan {
    pub symbol: String,
    pub market_index: i32,
    pub side: LighterSide,
    pub kind: LighterOrderKind,
    pub reference_price: f64,
    pub limit_price: f64,
    pub requested_notional_usd: f64,
    pub planned_notional_usd: f64,
    pub base_size: f64,
    pub base_amount: i64,
    pub price: i32,
    pub is_ask: i32,
    pub order_type: i32,
    pub time_in_force: i32,
    pub order_expiry: i64,
    pub size_decimals: u32,
    pub price_decimals: u32,
    pub reduce_only: bool,
    pub client_order_index: i64,
}

pub fn build_order_plan(
    market: &LighterMarket,
    request: &LighterOrderRequest,
) -> Result<LighterOrderPlan> {
    build_order_plan_with_reference(market, request, market.mark_price_f64()?)
}

pub fn build_order_plan_with_reference(
    market: &LighterMarket,
    request: &LighterOrderRequest,
    reference_price: f64,
) -> Result<LighterOrderPlan> {
    ensure!(
        market.is_active_perp(),
        "market {} is not an active perp",
        market.symbol
    );
    ensure!(
        market.symbol.eq_ignore_ascii_case(request.symbol.trim()),
        "request symbol {} does not match market {}",
        request.symbol,
        market.symbol
    );
    ensure!(
        request.notional_usd.is_finite() && request.notional_usd > 0.0,
        "notional_usd must be positive"
    );
    ensure!(
        request.max_slippage_bps.is_finite() && (0.0..=5_000.0).contains(&request.max_slippage_bps),
        "max_slippage_bps must be between 0 and 5000"
    );
    ensure!(
        (1..=i64::from(i32::MAX)).contains(&request.client_order_index),
        "client_order_index must be between 1 and 2^31-1"
    );

    ensure!(
        reference_price.is_finite() && reference_price > 0.0,
        "reference price must be positive"
    );
    let raw_limit = match request.kind {
        LighterOrderKind::Limit => request
            .limit_price
            .context("limit_price is required for a limit order")?,
        LighterOrderKind::Market => {
            let slippage = request.max_slippage_bps / 10_000.0;
            match request.side {
                LighterSide::Buy => reference_price * (1.0 + slippage),
                LighterSide::Sell => reference_price * (1.0 - slippage),
            }
        }
    };
    ensure!(
        raw_limit.is_finite() && raw_limit > 0.0,
        "planned limit price must be positive"
    );

    let size_decimals = market.effective_size_decimals();
    let price_decimals = market.effective_price_decimals();
    let raw_size = request.notional_usd / reference_price;
    let base_amount = decimal_to_scaled_i64_floor(raw_size, size_decimals, "base amount")?;
    ensure!(base_amount > 0, "order size rounds down to zero");
    let base_size = scaled_i64_to_decimal(base_amount, size_decimals);
    let min_base = parse_positive_decimal(&market.min_base_amount, "min_base_amount")?;
    ensure!(
        base_size + 1e-12 >= min_base,
        "planned base size {:.12} is below Lighter minimum {:.12}",
        base_size,
        min_base
    );
    let price_i64 = match request.side {
        LighterSide::Buy => decimal_to_scaled_i64_ceil(raw_limit, price_decimals, "price")?,
        LighterSide::Sell => decimal_to_scaled_i64_floor(raw_limit, price_decimals, "price")?,
    };
    let price = i32::try_from(price_i64).context("scaled Lighter price exceeds i32")?;
    ensure!(price > 0, "scaled price must be positive");
    let limit_price = scaled_i64_to_decimal(price_i64, price_decimals);
    let planned_notional_usd = base_size * reference_price;
    let min_quote = market.min_quote_amount_f64()?;
    ensure!(
        planned_notional_usd + 1e-9 >= min_quote,
        "planned order value {:.6} is below Lighter minimum {:.6}",
        planned_notional_usd,
        min_quote
    );

    Ok(LighterOrderPlan {
        symbol: market.symbol.clone(),
        market_index: market.market_id,
        side: request.side,
        kind: request.kind,
        reference_price,
        limit_price,
        requested_notional_usd: request.notional_usd,
        planned_notional_usd,
        base_size,
        base_amount,
        price,
        is_ask: i32::from(request.side == LighterSide::Sell),
        order_type: match request.kind {
            LighterOrderKind::Limit => 0,
            LighterOrderKind::Market => 1,
        },
        time_in_force: match request.kind {
            LighterOrderKind::Limit => 1,
            LighterOrderKind::Market => 0,
        },
        order_expiry: match request.kind {
            LighterOrderKind::Limit => -1,
            LighterOrderKind::Market => 0,
        },
        size_decimals,
        price_decimals,
        reduce_only: request.reduce_only,
        client_order_index: request.client_order_index,
    })
}

/// Builds an order without changing the requested base amount.
///
/// This is the precision boundary used by cross-venue neutral execution. The
/// returned `base_amount` always equals the requested value exactly.
pub fn build_exact_base_order_plan_with_reference(
    market: &LighterMarket,
    request: &LighterExactBaseOrderRequest,
    reference_price: f64,
) -> Result<LighterOrderPlan> {
    exact_base_plan(market, request, reference_price, false)
}

/// A verified full-position reduce-only exit may submit a dust remainder without
/// increasing its size to the entry minimum. The venue still validates the order.
pub fn build_exact_base_close_plan_with_reference(
    market: &LighterMarket,
    request: &LighterExactBaseOrderRequest,
    reference_price: f64,
    signed_position_base: i64,
) -> Result<LighterOrderPlan> {
    let closing_sign = if request.side == LighterSide::Buy { -1 } else { 1 };
    ensure!(request.reduce_only && signed_position_base.signum() == closing_sign
        && signed_position_base.checked_abs() == Some(request.base_amount),
        "full close requires the exact opposite reduce-only position");
    exact_base_plan(market, request, reference_price, true)
}

fn exact_base_plan(
    market: &LighterMarket,
    request: &LighterExactBaseOrderRequest,
    reference_price: f64,
    full_close: bool,
) -> Result<LighterOrderPlan> {
    ensure!(
        market.is_active_perp(),
        "market {} is not an active perp",
        market.symbol
    );
    ensure!(
        market.symbol.eq_ignore_ascii_case(request.symbol.trim()),
        "request symbol {} does not match market {}",
        request.symbol,
        market.symbol
    );
    ensure!(request.base_amount > 0, "base_amount must be positive");
    let size_decimals = market.effective_size_decimals();
    ensure!(
        request.size_decimals == size_decimals,
        "request size_decimals {} does not match Lighter market precision {}",
        request.size_decimals,
        size_decimals
    );
    ensure!(
        request.max_slippage_bps.is_finite() && (0.0..=5_000.0).contains(&request.max_slippage_bps),
        "max_slippage_bps must be between 0 and 5000"
    );
    ensure!(
        (1..=i64::from(i32::MAX)).contains(&request.client_order_index),
        "client_order_index must be between 1 and 2^31-1"
    );
    ensure!(
        reference_price.is_finite() && reference_price > 0.0,
        "reference price must be positive"
    );

    let base_size = scaled_i64_to_decimal(request.base_amount, size_decimals);
    let min_base = parse_positive_decimal(&market.min_base_amount, "min_base_amount")?;
    ensure!(
        full_close || base_size + 1e-12 >= min_base,
        "requested base size {:.12} is below Lighter minimum {:.12}",
        base_size,
        min_base
    );
    let planned_notional_usd = base_size * reference_price;
    ensure!(
        planned_notional_usd.is_finite(),
        "planned order value must be finite"
    );
    let min_quote = market.min_quote_amount_f64()?;
    ensure!(
        full_close || planned_notional_usd + 1e-9 >= min_quote,
        "planned order value {:.6} is below Lighter minimum {:.6}",
        planned_notional_usd,
        min_quote
    );

    let raw_limit = match request.kind {
        LighterOrderKind::Limit => request
            .limit_price
            .context("limit_price is required for a limit order")?,
        LighterOrderKind::Market => {
            let slippage = request.max_slippage_bps / 10_000.0;
            match request.side {
                LighterSide::Buy => reference_price * (1.0 + slippage),
                LighterSide::Sell => reference_price * (1.0 - slippage),
            }
        }
    };
    ensure!(
        raw_limit.is_finite() && raw_limit > 0.0,
        "planned limit price must be positive"
    );
    let price_decimals = market.effective_price_decimals();
    let price_i64 = match request.side {
        LighterSide::Buy => decimal_to_scaled_i64_ceil(raw_limit, price_decimals, "price")?,
        LighterSide::Sell => decimal_to_scaled_i64_floor(raw_limit, price_decimals, "price")?,
    };
    let price = i32::try_from(price_i64).context("scaled Lighter price exceeds i32")?;
    ensure!(price > 0, "scaled price must be positive");
    let limit_price = scaled_i64_to_decimal(price_i64, price_decimals);

    Ok(LighterOrderPlan {
        symbol: market.symbol.clone(),
        market_index: market.market_id,
        side: request.side,
        kind: request.kind,
        reference_price,
        limit_price,
        requested_notional_usd: planned_notional_usd,
        planned_notional_usd,
        base_size,
        base_amount: request.base_amount,
        price,
        is_ask: i32::from(request.side == LighterSide::Sell),
        order_type: match request.kind {
            LighterOrderKind::Limit => 0,
            LighterOrderKind::Market => 1,
        },
        time_in_force: match request.kind {
            LighterOrderKind::Limit => 1,
            LighterOrderKind::Market => 0,
        },
        order_expiry: match request.kind {
            LighterOrderKind::Limit => -1,
            LighterOrderKind::Market => 0,
        },
        size_decimals,
        price_decimals,
        reduce_only: request.reduce_only,
        client_order_index: request.client_order_index,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedLighterTransaction {
    pub tx_type: i32,
    /// JSON object encoded as a string, exactly as returned by the official signer.
    pub tx_info: String,
    #[serde(default)]
    pub tx_hash: Option<String>,
}

/// A complete HTTP response from the sequencer that explicitly rejected the
/// transaction. Unlike a transport error, this outcome is not ambiguous.
#[derive(Debug)]
pub struct LighterSubmitRejected {
    pub status: u16,
    pub body: String,
}

impl fmt::Display for LighterSubmitRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Lighter sendTx rejected the transaction (HTTP {}): {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for LighterSubmitRejected {}

fn sanitize_submit_error_body(body: &str) -> String {
    const MAX_CHARS: usize = 2_048;
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }
    let mut truncated = compact.chars().take(MAX_CHARS).collect::<String>();
    truncated.push_str("…");
    truncated
}

impl SignedLighterTransaction {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.tx_type > 0, "tx_type must be positive");
        let info: Value = serde_json::from_str(&self.tx_info)
            .context("tx_info must be a JSON object encoded as a string")?;
        ensure!(info.is_object(), "tx_info must decode to a JSON object");
        ensure!(
            info.get("Sig")
                .and_then(Value::as_str)
                .is_some_and(|sig| !sig.is_empty()),
            "tx_info does not contain a non-empty Sig"
        );
        Ok(())
    }

    pub fn websocket_envelope(&self) -> Value {
        json!({
            "type": "jsonapi/sendtx",
            "data": {
                "tx_type": self.tx_type,
                "tx_info": self.tx_info,
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LighterTrade {
    pub trade_id: i64,
    #[serde(default)]
    pub tx_hash: String,
    pub market_id: i32,
    pub size: String,
    pub price: String,
    #[serde(default)]
    pub usd_amount: String,
    pub ask_account_id: i64,
    pub bid_account_id: i64,
    #[serde(default)]
    pub is_maker_ask: bool,
    #[serde(default)]
    pub block_height: i64,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub transaction_time: i64,
    #[serde(default)]
    pub taker_position_size_before: Option<String>,
    #[serde(default)]
    pub maker_position_size_before: Option<String>,
    #[serde(default)]
    pub taker_position_sign_changed: Option<bool>,
    #[serde(default)]
    pub maker_position_sign_changed: Option<bool>,
}

impl LighterTrade {
    pub fn dedupe_key(&self) -> String {
        if !self.tx_hash.trim().is_empty() {
            format!("{}:{}:{}", self.market_id, self.trade_id, self.tx_hash)
        } else {
            format!("{}:{}:{}", self.market_id, self.trade_id, self.timestamp)
        }
    }
}

/// Streams a public market trade channel and reconnects with bounded backoff.
/// The caller owns deduplication because reconnect snapshots may repeat trades.
pub async fn run_public_trade_stream(
    endpoints: LighterEndpoints,
    market_id: i32,
    sender: mpsc::Sender<std::result::Result<LighterTrade, String>>,
) -> Result<()> {
    endpoints.validate()?;
    ensure!(market_id >= 0, "market_id must be non-negative");
    let mut reconnect_attempt = 0_u32;

    loop {
        let result =
            stream_public_trades_once(&endpoints.public_readonly_ws_url(), market_id, &sender)
                .await;
        if sender.is_closed() {
            return Ok(());
        }
        reconnect_attempt = reconnect_attempt.saturating_add(1);
        let error = result
            .err()
            .unwrap_or_else(|| anyhow::anyhow!("Lighter websocket closed"));
        let _ = sender.send(Err(format!("{error:#}"))).await;
        let exponent = reconnect_attempt.saturating_sub(1).min(5);
        let delay = WS_RECONNECT_BASE_MS
            .saturating_mul(2_u64.saturating_pow(exponent))
            .min(WS_RECONNECT_MAX_MS);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

async fn stream_public_trades_once(
    ws_url: &str,
    market_id: i32,
    sender: &mpsc::Sender<std::result::Result<LighterTrade, String>>,
) -> Result<()> {
    let (stream, _) = connect_lighter_websocket(ws_url).await?;
    let (mut writer, mut reader) = stream.split();
    writer
        .send(Message::Text(
            json!({
                "type": "subscribe",
                "channel": format!("trade/{market_id}"),
            })
            .to_string(),
        ))
        .await
        .context("failed to subscribe to Lighter trade stream")?;

    while let Some(message) = reader.next().await {
        match message.context("Lighter websocket read failed")? {
            Message::Text(text) => {
                for trade in parse_trade_stream_message(&text)? {
                    if sender.send(Ok(trade)).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Message::Ping(payload) => writer
                .send(Message::Pong(payload))
                .await
                .context("failed to reply to Lighter websocket ping")?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    bail!("Lighter websocket closed")
}

pub fn parse_trade_stream_message(text: &str) -> Result<Vec<LighterTrade>> {
    let value: Value = serde_json::from_str(text).context("invalid Lighter websocket JSON")?;
    let message_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !message_type.eq_ignore_ascii_case("update/trade") {
        return Ok(Vec::new());
    }
    let Some(trades) = value.get("trades") else {
        return Ok(Vec::new());
    };
    match trades {
        Value::Array(items) => items
            .iter()
            .cloned()
            .map(|item| {
                serde_json::from_value(item).context("invalid Lighter trade in websocket batch")
            })
            .collect(),
        Value::Object(_) => Ok(vec![
            serde_json::from_value(trades.clone()).context("invalid Lighter websocket trade")?,
        ]),
        _ => bail!("Lighter trades field must be an object or array"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderAction {
    Open,
    Increase,
    Reduce,
    Close,
    Flip,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedLeaderTrade {
    pub event_id: String,
    pub leader_account_index: i64,
    pub market_id: i32,
    pub trade_id: i64,
    pub side: LighterSide,
    pub size: f64,
    pub price: f64,
    pub notional_usd: f64,
    pub position_before: Option<f64>,
    pub position_after: Option<f64>,
    pub action: LeaderAction,
    pub exchange_time_ms: i64,
}

pub fn normalize_leader_trade(
    trade: &LighterTrade,
    leader_account_index: i64,
) -> Result<Option<NormalizedLeaderTrade>> {
    let is_ask = trade.ask_account_id == leader_account_index;
    let is_bid = trade.bid_account_id == leader_account_index;
    if is_ask == is_bid {
        return Ok(None);
    }
    let side = if is_ask {
        LighterSide::Sell
    } else {
        LighterSide::Buy
    };
    let leader_is_maker = if is_ask {
        trade.is_maker_ask
    } else {
        !trade.is_maker_ask
    };
    let before_text = if leader_is_maker {
        trade.maker_position_size_before.as_deref()
    } else {
        trade.taker_position_size_before.as_deref()
    };
    let position_before = before_text
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_decimal(value, "position_size_before"))
        .transpose()?;
    let size = parse_positive_decimal(&trade.size, "trade size")?;
    let price = parse_positive_decimal(&trade.price, "trade price")?;
    let notional_usd = match parse_decimal(&trade.usd_amount, "usd_amount") {
        Ok(value) if value > 0.0 => value,
        _ => size * price,
    };
    let signed_delta = match side {
        LighterSide::Buy => size,
        LighterSide::Sell => -size,
    };
    let position_after = position_before.map(|before| before + signed_delta);
    let action = match (position_before, position_after) {
        (Some(before), Some(after)) => classify_position_change(before, after),
        _ => LeaderAction::Unknown,
    };
    Ok(Some(NormalizedLeaderTrade {
        event_id: trade.dedupe_key(),
        leader_account_index,
        market_id: trade.market_id,
        trade_id: trade.trade_id,
        side,
        size,
        price,
        notional_usd,
        position_before,
        position_after,
        action,
        exchange_time_ms: normalize_exchange_timestamp_ms(trade),
    }))
}

fn classify_position_change(before: f64, after: f64) -> LeaderAction {
    const EPSILON: f64 = 1e-12;
    if before.abs() <= EPSILON && after.abs() > EPSILON {
        return LeaderAction::Open;
    }
    if before.abs() > EPSILON && after.abs() <= EPSILON {
        return LeaderAction::Close;
    }
    if before.signum() != after.signum() {
        return LeaderAction::Flip;
    }
    if after.abs() > before.abs() {
        LeaderAction::Increase
    } else if after.abs() < before.abs() {
        LeaderAction::Reduce
    } else {
        LeaderAction::Unknown
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyPolicy {
    pub ratio: f64,
    pub min_order_notional_usd: f64,
    pub max_order_notional_usd: f64,
    #[serde(default = "default_true")]
    pub require_position_context: bool,
}

impl CopyPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.ratio.is_finite() && self.ratio > 0.0,
            "copy ratio must be positive"
        );
        ensure!(
            self.min_order_notional_usd.is_finite() && self.min_order_notional_usd > 0.0,
            "minimum copy notional must be positive"
        );
        ensure!(
            self.max_order_notional_usd.is_finite()
                && self.max_order_notional_usd >= self.min_order_notional_usd,
            "maximum copy notional must be at least the minimum"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyCandidate {
    pub source_event_id: String,
    pub leader_account_index: i64,
    pub market_id: i32,
    pub side: LighterSide,
    pub action: LeaderAction,
    pub leader_notional_usd: f64,
    pub local_notional_usd: f64,
    pub reduce_only: bool,
    pub accepted: bool,
    pub reason: String,
}

pub fn build_copy_candidate(
    event: &NormalizedLeaderTrade,
    policy: &CopyPolicy,
) -> Result<CopyCandidate> {
    policy.validate()?;
    let mut candidate = CopyCandidate {
        source_event_id: event.event_id.clone(),
        leader_account_index: event.leader_account_index,
        market_id: event.market_id,
        side: event.side,
        action: event.action,
        leader_notional_usd: event.notional_usd,
        local_notional_usd: (event.notional_usd * policy.ratio).min(policy.max_order_notional_usd),
        reduce_only: matches!(event.action, LeaderAction::Reduce | LeaderAction::Close),
        accepted: false,
        reason: String::new(),
    };
    if policy.require_position_context && event.action == LeaderAction::Unknown {
        candidate.reason = "missing reliable leader position context".to_string();
        return Ok(candidate);
    }
    if event.action == LeaderAction::Flip {
        candidate.reason =
            "flip must be split into close and open legs before submission".to_string();
        return Ok(candidate);
    }
    if candidate.local_notional_usd + 1e-9 < policy.min_order_notional_usd {
        candidate.reason = format!(
            "copy notional {:.6} is below minimum {:.6}",
            candidate.local_notional_usd, policy.min_order_notional_usd
        );
        return Ok(candidate);
    }
    candidate.accepted = true;
    candidate.reason = "accepted by first-version copy policy".to_string();
    Ok(candidate)
}

/// Derives a replay-stable client order index for one copied leader fill.
/// Robinhood Lighter currently accepts only positive 31-bit values.
pub fn deterministic_copy_client_order_index(
    trade_id: i64,
    leader_account_index: i64,
    market_index: i32,
    local_account_index: i64,
) -> i64 {
    let mut hash = 14_695_981_039_346_656_037_u64;
    for byte in trade_id
        .to_le_bytes()
        .into_iter()
        .chain(leader_account_index.to_le_bytes())
        .chain(market_index.to_le_bytes())
        .chain(local_account_index.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    i64::from((hash as u32) & 0x7fff_ffff).max(1)
}

#[derive(Debug)]
pub struct TradeDedupe {
    capacity: usize,
    order: VecDeque<String>,
    keys: HashSet<String>,
}

impl TradeDedupe {
    pub fn new(capacity: usize) -> Result<Self> {
        ensure!(capacity > 0, "dedupe capacity must be positive");
        Ok(Self {
            capacity,
            order: VecDeque::with_capacity(capacity),
            keys: HashSet::with_capacity(capacity),
        })
    }

    /// Returns true only for a key that was not already observed.
    pub fn insert(&mut self, key: String) -> bool {
        if self.keys.contains(&key) {
            return false;
        }
        self.keys.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(expired) = self.order.pop_front() {
                self.keys.remove(&expired);
            }
        }
        true
    }
}

fn ensure_api_success(code: i64, message: Option<&str>) -> Result<()> {
    ensure!(
        code == 0 || code == 200,
        "Lighter API error {code}: {}",
        message.unwrap_or("no message")
    );
    Ok(())
}

fn normalize_evm_address(address: &str) -> Result<String> {
    let trimmed = address.trim();
    ensure!(
        trimmed.len() == 42,
        "L1 address must contain 0x plus 40 hex digits"
    );
    ensure!(trimmed.starts_with("0x"), "L1 address must start with 0x");
    ensure!(
        trimmed[2..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "L1 address contains non-hex characters"
    );
    Ok(trimmed.to_ascii_lowercase())
}

fn parse_decimal(value: &str, label: &str) -> Result<f64> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("invalid {label}: {value}"))?;
    ensure!(parsed.is_finite(), "{label} must be finite");
    Ok(parsed)
}

fn parse_positive_decimal(value: &str, label: &str) -> Result<f64> {
    let parsed = parse_decimal(value, label)?;
    ensure!(parsed > 0.0, "{label} must be positive");
    Ok(parsed)
}

fn decimal_to_scaled_i64_floor(value: f64, decimals: u32, label: &str) -> Result<i64> {
    decimal_to_scaled_i64(value, decimals, false, label)
}

fn decimal_to_scaled_i64_ceil(value: f64, decimals: u32, label: &str) -> Result<i64> {
    decimal_to_scaled_i64(value, decimals, true, label)
}

fn decimal_to_scaled_i64(value: f64, decimals: u32, ceil: bool, label: &str) -> Result<i64> {
    ensure!(
        value.is_finite() && value >= 0.0,
        "{label} must be finite and non-negative"
    );
    ensure!(
        decimals <= 12,
        "{label} decimals exceed supported safety limit"
    );
    let scale = 10_f64.powi(decimals as i32);
    let scaled = value * scale;
    ensure!(
        scaled.is_finite() && scaled <= i64::MAX as f64,
        "{label} exceeds i64"
    );
    let rounded = if ceil {
        (scaled - 1e-9).ceil()
    } else {
        (scaled + 1e-9).floor()
    };
    Ok(rounded as i64)
}

fn scaled_i64_to_decimal(value: i64, decimals: u32) -> f64 {
    value as f64 / 10_f64.powi(decimals as i32)
}

fn normalize_exchange_timestamp_ms(trade: &LighterTrade) -> i64 {
    let raw = if trade.transaction_time > 0 {
        trade.transaction_time
    } else {
        trade.timestamp
    };
    if raw >= 1_000_000_000_000_000_000 {
        raw / 1_000_000
    } else if raw >= 1_000_000_000_000_000 {
        raw / 1_000
    } else if raw > 0 && raw < 10_000_000_000 {
        raw.saturating_mul(1_000)
    } else {
        raw
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn websocket_race_keeps_success_when_another_route_fails_or_hangs() {
        let attempts: Vec<(String, BoxFuture<'_, Result<u8>>)> = vec![
            ("failed".into(), async { bail!("TLS closed") }.boxed()),
            ("hung".into(), futures_util::future::pending().boxed()),
            ("healthy".into(), async { Ok(7) }.boxed()),
        ];
        assert_eq!(
            race_lighter_connections(attempts, Duration::from_millis(50))
                .await
                .unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn websocket_race_reports_each_failed_route_and_bounds_a_silent_peer() {
        let attempts: Vec<(String, BoxFuture<'_, Result<u8>>)> = vec![
            (
                "http://127.0.0.1:17891/".into(),
                async { bail!("TLS closed") }.boxed(),
            ),
            (
                "http://127.0.0.1:17893/".into(),
                futures_util::future::pending().boxed(),
            ),
        ];
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            race_lighter_connections(attempts, Duration::from_millis(10)),
        )
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
        assert!(error.contains("17891/: TLS closed"));
        assert!(error.contains("17893/: connection deadline exceeded (10ms)"));
    }

    #[test]
    fn lighter_proxy_accepts_only_explicit_local_http_endpoint() {
        let proxy = parse_local_http_proxy("http://127.0.0.1:17893").unwrap();
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, 17893);
        assert!(parse_local_http_proxy("https://127.0.0.1:17893").is_err());
        assert!(parse_local_http_proxy("http://example.com:17893").is_err());
        assert!(parse_local_http_proxy("http://user:pass@127.0.0.1:17893").is_err());
        assert!(parse_local_http_proxy("http://127.0.0.1").is_err());
    }

    #[test]
    fn lighter_websocket_proxy_list_is_bounded_local_and_unique() {
        let proxies = parse_local_http_proxy_list(
            "http://127.0.0.1:17891, http://localhost:17893,http://[::1]:17894",
        )
        .unwrap();
        assert_eq!(proxies.len(), 3);
        assert_eq!(proxies[0].port, 17891);
        assert_eq!(proxies[2].port, 17894);
        assert!(
            parse_local_http_proxy_list("http://127.0.0.1:17893,http://127.0.0.1:17893").is_err()
        );
        assert!(parse_local_http_proxy_list("http://example.com:17893").is_err());
        assert!(parse_local_http_proxy_list("").is_err());
    }

    fn market() -> LighterMarket {
        LighterMarket {
            symbol: "BTC".to_string(),
            market_id: 1,
            market_type: "perp".to_string(),
            status: "active".to_string(),
            taker_fee: "0.0000".to_string(),
            is_taker_fee_enabled: true,
            min_base_amount: "0.0001".to_string(),
            min_quote_amount: "10.0".to_string(),
            supported_size_decimals: 4,
            supported_price_decimals: 1,
            size_decimals: 4,
            price_decimals: 1,
            mark_price: "100000.0".to_string(),
            index_price: "100010.0".to_string(),
            last_trade_price: json!(100005.0),
            default_initial_margin_fraction: 500,
            min_initial_margin_fraction: 200,
            maintenance_margin_fraction: 120,
            market_config: Value::Null,
        }
    }

    fn trade() -> LighterTrade {
        LighterTrade {
            trade_id: 42,
            tx_hash: "0xabc".to_string(),
            market_id: 1,
            size: "0.1000".to_string(),
            price: "100.0".to_string(),
            usd_amount: "10.0".to_string(),
            ask_account_id: 7,
            bid_account_id: 8,
            is_maker_ask: true,
            block_height: 10,
            timestamp: 1_700_000_000,
            transaction_time: 0,
            taker_position_size_before: Some("0.0".to_string()),
            maker_position_size_before: Some("0.2".to_string()),
            taker_position_sign_changed: None,
            maker_position_sign_changed: None,
        }
    }

    #[test]
    fn full_reduce_only_close_preserves_dust_without_relaxing_entry_or_precision() {
        for (symbol, decimals) in [("OPENAI", 4), ("ANTHROPIC", 5)] {
            for side in [LighterSide::Buy, LighterSide::Sell] {
                let mut m = market();
                m.symbol = symbol.into();
                m.size_decimals = decimals; m.supported_size_decimals = decimals;
                m.min_base_amount = "0.001".into();
                let r = LighterExactBaseOrderRequest {
                    symbol: symbol.into(), side, base_amount: 1, size_decimals: decimals,
                    kind: LighterOrderKind::Market, limit_price: None, reduce_only: true,
                    max_slippage_bps: 0., client_order_index: 123,
                };
                let position = if side == LighterSide::Buy { -1 } else { 1 };
                assert!(build_exact_base_order_plan_with_reference(&m, &r, 2100.).is_err());
                let plan = build_exact_base_close_plan_with_reference(&m, &r, 2100., position).unwrap();
                assert_eq!(plan.base_amount, 1);
                assert!(plan.reduce_only);
                assert_eq!(plan.time_in_force, 0);
                assert_eq!(plan.limit_price, 2100.);
                for invalid_position in [0, -position, position * 2, i64::MIN] {
                    assert!(build_exact_base_close_plan_with_reference(&m, &r, 2100., invalid_position).is_err());
                }
                let mut bad = r.clone(); bad.reduce_only = false;
                assert!(build_exact_base_close_plan_with_reference(&m, &bad, 2100., position).is_err());
                bad = r.clone(); bad.size_decimals += 1;
                assert!(build_exact_base_close_plan_with_reference(&m, &bad, 2100., position).is_err());
                bad = r.clone(); bad.base_amount = 2;
                assert!(build_exact_base_close_plan_with_reference(&m, &bad, 2100., position).is_err());
                assert!(build_exact_base_close_plan_with_reference(&m, &r, 0., position).is_err());
            }
        }
    }

    #[test]
    fn official_endpoints_are_tls_only() {
        for environment in [
            LighterEnvironment::Mainnet,
            LighterEnvironment::Testnet,
            LighterEnvironment::Robinhood,
            LighterEnvironment::RobinhoodTestnet,
        ] {
            let endpoints = LighterEndpoints::official(environment);
            endpoints.validate().unwrap();
            assert!(
                endpoints
                    .public_readonly_ws_url()
                    .ends_with("?readonly=true")
            );
        }

        let robinhood = LighterEndpoints::official(LighterEnvironment::Robinhood);
        assert_eq!(robinhood.http_url, "https://api.rh.lighter.xyz");
        assert_eq!(robinhood.ws_url, "wss://api.rh.lighter.xyz/stream");
        assert_eq!(robinhood.chain_id, 466_324);

        let robinhood_testnet = LighterEndpoints::official(LighterEnvironment::RobinhoodTestnet);
        assert_eq!(
            robinhood_testnet.http_url,
            "https://api.rh-testnet.lighter.xyz"
        );
        assert_eq!(robinhood_testnet.chain_id, 300);
    }

    #[test]
    fn order_plan_rounds_size_down_and_buy_limit_up() {
        let plan = build_order_plan(
            &market(),
            &LighterOrderRequest {
                symbol: "BTC".to_string(),
                side: LighterSide::Buy,
                notional_usd: 101.0,
                kind: LighterOrderKind::Market,
                limit_price: None,
                reduce_only: false,
                max_slippage_bps: 25.0,
                client_order_index: 123,
            },
        )
        .unwrap();
        assert_eq!(plan.base_amount, 10);
        assert_eq!(plan.base_size, 0.001);
        assert_eq!(plan.price, 1_002_500);
        assert_eq!(plan.limit_price, 100_250.0);
        assert_eq!(plan.is_ask, 0);
        assert_eq!(plan.order_type, 1);
        assert_eq!(plan.time_in_force, 0);
        assert_eq!(plan.order_expiry, 0);
    }

    #[test]
    fn order_plan_fails_below_exchange_minimum() {
        let error = build_order_plan(
            &market(),
            &LighterOrderRequest {
                symbol: "BTC".to_string(),
                side: LighterSide::Sell,
                notional_usd: 9.0,
                kind: LighterOrderKind::Limit,
                limit_price: Some(100_000.0),
                reduce_only: false,
                max_slippage_bps: 0.0,
                client_order_index: 123,
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("rounds down to zero")
                || error.to_string().contains("below Lighter minimum")
        );
    }

    #[test]
    fn order_book_uses_ask_for_buy_and_bid_for_sell() {
        let book = LighterOrderBook {
            code: 200,
            message: None,
            total_asks: 1,
            asks: vec![LighterSimpleOrder {
                order_index: 1,
                order_id: "1".to_string(),
                owner_account_index: 10,
                initial_base_amount: "1".to_string(),
                remaining_base_amount: "1".to_string(),
                price: "101.5".to_string(),
                order_expiry: 0,
                transaction_time: 0,
            }],
            total_bids: 1,
            bids: vec![LighterSimpleOrder {
                order_index: 2,
                order_id: "2".to_string(),
                owner_account_index: 11,
                initial_base_amount: "1".to_string(),
                remaining_base_amount: "1".to_string(),
                price: "100.5".to_string(),
                order_expiry: 0,
                transaction_time: 0,
            }],
        };
        assert_eq!(book.best_price(LighterSide::Buy).unwrap(), 101.5);
        assert_eq!(book.best_price(LighterSide::Sell).unwrap(), 100.5);
    }

    #[test]
    fn websocket_parser_accepts_object_and_batch_shapes() {
        let value = serde_json::to_value(trade()).unwrap();
        let object = json!({"type":"update/trade", "trades": value});
        let batch =
            json!({"type":"update/trade", "trades": [serde_json::to_value(trade()).unwrap()]});
        assert_eq!(
            parse_trade_stream_message(&object.to_string())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parse_trade_stream_message(&batch.to_string())
                .unwrap()
                .len(),
            1
        );
        assert!(
            parse_trade_stream_message(r#"{"type":"subscribed/trade"}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn leader_side_and_reduce_action_use_account_role() {
        let leader = normalize_leader_trade(&trade(), 7).unwrap().unwrap();
        assert_eq!(leader.side, LighterSide::Sell);
        assert_eq!(leader.position_before, Some(0.2));
        assert!((leader.position_after.unwrap() - 0.1).abs() < 1e-12);
        assert_eq!(leader.action, LeaderAction::Reduce);

        let follower = normalize_leader_trade(&trade(), 8).unwrap().unwrap();
        assert_eq!(follower.side, LighterSide::Buy);
        assert_eq!(follower.action, LeaderAction::Open);
        let mut microsecond_trade = trade();
        microsecond_trade.transaction_time = 1_787_318_759_168_465;
        let normalized = normalize_leader_trade(&microsecond_trade, 7)
            .unwrap()
            .unwrap();
        assert_eq!(normalized.exchange_time_ms, 1_787_318_759_168);
    }

    #[test]
    fn copy_candidate_fails_closed_without_position_context() {
        let mut raw = trade();
        raw.maker_position_size_before = None;
        let event = normalize_leader_trade(&raw, 7).unwrap().unwrap();
        let candidate = build_copy_candidate(
            &event,
            &CopyPolicy {
                ratio: 0.5,
                min_order_notional_usd: 1.0,
                max_order_notional_usd: 100.0,
                require_position_context: true,
            },
        )
        .unwrap();
        assert!(!candidate.accepted);
        assert_eq!(candidate.action, LeaderAction::Unknown);
    }

    #[test]
    fn signed_transaction_requires_embedded_signature() {
        let unsigned = SignedLighterTransaction {
            tx_type: 14,
            tx_info: r#"{"Nonce":1}"#.to_string(),
            tx_hash: None,
        };
        assert!(unsigned.validate().is_err());
        let signed = SignedLighterTransaction {
            tx_type: 14,
            tx_info: r#"{"Nonce":1,"Sig":"0x1234"}"#.to_string(),
            tx_hash: None,
        };
        signed.validate().unwrap();
        assert_eq!(signed.websocket_envelope()["data"]["tx_type"], 14);
    }

    #[test]
    fn submit_error_body_is_compact_and_bounded() {
        assert_eq!(
            sanitize_submit_error_body("  bad\n request  "),
            "bad request"
        );
        let long = "x".repeat(2_100);
        let sanitized = sanitize_submit_error_body(&long);
        assert_eq!(sanitized.chars().count(), 2_049);
        assert!(sanitized.ends_with('…'));
    }

    #[test]
    fn dedupe_is_bounded() {
        let mut dedupe = TradeDedupe::new(2).unwrap();
        assert!(dedupe.insert("a".to_string()));
        assert!(!dedupe.insert("a".to_string()));
        assert!(dedupe.insert("b".to_string()));
        assert!(dedupe.insert("c".to_string()));
        assert!(dedupe.insert("a".to_string()));
    }
}
