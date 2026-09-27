use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub type TimestampMs = u64;
pub type AccountId = String;
pub type WorkerId = String;
pub type SignalId = String;
pub type IntentId = String;
pub type StrategyId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorSignal {
    pub signal_id: SignalId,
    pub source: SignalSource,
    pub created_at_ms: TimestampMs,
    pub dispatch_at_ms: TimestampMs,
    pub expires_at_ms: TimestampMs,
    pub target_accounts: Vec<AccountId>,
    pub dedupe_key: String,
    pub order: SignalOrder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSource {
    DryRun,
    Manual,
    Fib,
    SmartMoney,
    BtcEthScalper,
}

impl SignalSource {
    pub fn strategy_id(&self) -> StrategyId {
        match self {
            Self::DryRun => "dry_run".to_string(),
            Self::Manual => "manual_ops".to_string(),
            Self::Fib => "fib_retracement".to_string(),
            Self::SmartMoney => "smart_money_copy".to_string(),
            Self::BtcEthScalper => "btc_eth_scalper".to_string(),
        }
    }

    pub fn intent_source(&self) -> IntentSource {
        match self {
            Self::Manual => IntentSource::Manual,
            Self::DryRun => IntentSource::System,
            Self::Fib | Self::SmartMoney | Self::BtcEthScalper => IntentSource::Strategy,
        }
    }

    pub fn module_scope(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Fib => "fib",
            Self::SmartMoney => "copy",
            Self::BtcEthScalper => "scalper",
            Self::DryRun => "manual",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalOrder {
    #[serde(default)]
    pub market: Option<String>,
    #[serde(default)]
    pub dex: Option<String>,
    pub coin: String,
    pub side: OrderSide,
    pub notional_usd: f64,
    pub reduce_only: bool,
    pub execution_mode: ExecutionMode,
    pub max_slippage_bps: f64,
    #[serde(default)]
    pub limit_price: Option<f64>,
    #[serde(default)]
    pub apply_account_ratio: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Taker,
    Maker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeIntent {
    pub intent_id: IntentId,
    pub signal_id: Option<SignalId>,
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub target_accounts: Vec<AccountId>,
    pub strategy_id: StrategyId,
    pub created_at_ms: TimestampMs,
    pub market: Option<String>,
    pub dex: Option<String>,
    pub coin: String,
    pub side: OrderSide,
    pub intent_kind: IntentKind,
    pub sizing: SizingRequest,
    pub price_policy: PricePolicy,
    pub execution_policy: ExecutionPolicy,
    pub reduce_only: bool,
    pub reason: String,
    pub source: IntentSource,
    pub source_event_id: Option<String>,
    pub expires_at_ms: Option<TimestampMs>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Open,
    Increase,
    Reduce,
    Close,
    StopLoss,
    TakeProfit,
    Cancel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SizingRequest {
    pub notional_usd: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricePolicy {
    MarketWithSlippageLimit { max_slippage_bps: f64 },
    Limit { price: f64 },
    MakerOnly { price: f64 },
    PegBestBidAsk,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPolicy {
    Taker,
    Maker,
    Alo,
    Ioc,
    Gtc,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource {
    Strategy,
    Manual,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedOrder {
    pub risk_decision_id: String,
    pub intent_id: IntentId,
    pub signal_id: Option<SignalId>,
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub strategy_id: StrategyId,
    pub market: Option<String>,
    pub dex: Option<String>,
    pub coin: String,
    pub side: OrderSide,
    pub notional_usd: f64,
    pub exact_size: Option<f64>,
    pub price: Option<f64>,
    pub execution_mode: ExecutionMode,
    pub execution_policy: ExecutionPolicy,
    pub max_slippage_bps: f64,
    pub reduce_only: bool,
    pub cloid: String,
    pub expires_at_ms: Option<TimestampMs>,
}

impl CoordinatorSignal {
    pub fn to_trade_intent(
        &self,
        account_id: &str,
        worker_id: &str,
        account_copy_ratio: f64,
    ) -> TradeIntent {
        let notional_usd = if self.order.apply_account_ratio {
            self.order.notional_usd * account_copy_ratio.max(0.0001)
        } else {
            self.order.notional_usd
        };
        let price_policy = match self.order.limit_price {
            Some(price) => match self.order.execution_mode {
                ExecutionMode::Maker => PricePolicy::MakerOnly { price },
                ExecutionMode::Taker => PricePolicy::Limit { price },
            },
            None => PricePolicy::MarketWithSlippageLimit {
                max_slippage_bps: self.order.max_slippage_bps,
            },
        };
        TradeIntent {
            intent_id: format!("intent-{account_id}-{}", self.signal_id),
            signal_id: Some(self.signal_id.clone()),
            worker_id: worker_id.to_string(),
            account_id: account_id.to_string(),
            target_accounts: self.target_accounts.clone(),
            strategy_id: self.source.strategy_id(),
            created_at_ms: now_ms(),
            market: self.order.market.clone(),
            dex: self.order.dex.clone(),
            coin: self.order.coin.clone(),
            side: self.order.side,
            intent_kind: if self.order.reduce_only {
                IntentKind::Reduce
            } else {
                IntentKind::Open
            },
            sizing: SizingRequest { notional_usd },
            price_policy,
            execution_policy: match self.order.execution_mode {
                ExecutionMode::Taker => ExecutionPolicy::Taker,
                ExecutionMode::Maker => ExecutionPolicy::Maker,
            },
            reduce_only: self.order.reduce_only,
            reason: format!("{:?} signal {}", self.source, self.signal_id),
            source: self.source.intent_source(),
            source_event_id: Some(self.dedupe_key.clone()),
            expires_at_ms: Some(self.expires_at_ms),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub address: String,
    pub pid: u32,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAck {
    pub signal_id: SignalId,
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub received_at_ms: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSubmitted {
    pub signal_id: SignalId,
    pub intent_id: IntentId,
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub cloid: String,
    pub coin: String,
    pub side: OrderSide,
    pub notional_usd: f64,
    pub submitted_price: Option<f64>,
    pub submitted_size: Option<f64>,
    pub exchange_status: Option<String>,
    pub oid: Option<u64>,
    pub filled_size: Option<f64>,
    pub avg_fill_price: Option<f64>,
    pub dry_run: bool,
    pub submitted_at_ms: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedIntent {
    pub signal_id: SignalId,
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub reason_code: String,
    pub message: String,
    pub rejected_at_ms: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHealth {
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub healthy: bool,
    pub message: String,
    pub reported_at_ms: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerError {
    pub worker_id: WorkerId,
    pub account_id: AccountId,
    pub message: String,
    pub error_at_ms: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerReport {
    Ack(WorkerAck),
    Rejected(RejectedIntent),
    Submitted(OrderSubmitted),
    Health(WorkerHealth),
    Error(WorkerError),
}

pub fn now_ms() -> TimestampMs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before unix epoch")
        .as_millis()
        .try_into()
        .expect("millisecond timestamp does not fit into u64")
}
