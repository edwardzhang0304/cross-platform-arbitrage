mod market;
mod adapters;
pub use market::*;
pub use adapters::*;
use serde::{Serialize, Deserialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggressorSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowTradePrint {
    pub trade_id: String,
    pub aggressor: AggressorSide,
    pub price: f64,
    pub size: f64,
    pub exchange_timestamp_ms: u64,
    pub received_timestamp_ms: u64,
}

