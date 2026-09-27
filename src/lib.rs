#[cfg(all(feature="paper-runtime", feature="openai-inventory-live"))]
compile_error!("Build paper and live executables separately; do not enable both features");
pub mod config;
pub mod domain;
pub mod hyperliquid;
pub mod lighter;
pub mod lighter_manual;
pub mod lighter_runtime;
pub mod lighter_reconcile;
pub mod ws_post;
pub mod secrets;
pub mod crossvenue_a;
#[path="strategies/openai_inventory/mod.rs"]
pub mod openai_inventory;
pub mod power;
pub mod portable;
#[cfg(feature="openai-inventory-live")]
pub mod server;
pub mod profiles;
pub mod monitor;
#[cfg(feature="paper-runtime")]
pub mod paper_server;
