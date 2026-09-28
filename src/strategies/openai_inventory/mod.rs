//! OPENAI paired inventory. Strategy code never owns credentials or transports.
pub mod config;
pub mod market;
pub use market::MarketPair;
pub mod account_binding;
// Offline risk-regression fixtures are not part of the running trading program.
#[cfg(test)]
#[path = "test_support.rs"]
mod comparison;
pub mod charts;
pub mod entry_mean;
pub mod accounting;
pub mod alerts;
pub mod liquidation;
pub mod execution;
#[cfg(feature="openai-inventory-live")]
pub mod live;
pub mod live_orphan;
pub mod model;
pub mod rules_upgrade;
pub mod service;
pub mod store;
pub mod strategy;
pub mod venue;
#[cfg(any(test, feature="paper-runtime"))]
pub mod paper_funding;
pub use config::*;
pub use model::*;
pub use service::*;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod liquidation_tests;
#[cfg(test)]
mod live_limits_tests;
#[cfg(test)]
mod accounting_tests;

pub mod feed;
