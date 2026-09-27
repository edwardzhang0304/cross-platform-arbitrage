use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppConfig { #[serde(default)] pub accounts: Vec<AccountConfig>, #[serde(default)] pub secrets: SecretsSection }
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecretsSection { pub vault_path: String, #[serde(default)] pub allow_env_fallback: bool }
impl Default for SecretsSection { fn default()->Self {Self{vault_path:"runtime/trading.vault".into(),allow_env_fallback:false}} }
fn default_true()->bool{true}
fn default_copy_ratio()->f64{1.0}
fn default_max_order_notional()->f64{15.0}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountConfig {
    pub account_id: String,
    pub address: String,
    #[serde(default)]
    pub secret_id: String,
    #[serde(default)]
    pub api_wallet_env: String,
    #[serde(default)]
    pub transfer_secret_id: String,
    #[serde(default)]
    pub transfer_wallet_env: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub worker_enabled: bool,
    #[serde(default = "default_copy_ratio")]
    pub copy_ratio: f64,
    #[serde(default = "default_max_order_notional")]
    pub max_order_notional_usd: f64,
    #[serde(default)]
    pub blocked_markets: Vec<String>,
}

