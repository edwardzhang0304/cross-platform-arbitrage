//! Public diagnostics only. Account identities never enter the rule fingerprint.
use crate::openai_inventory::InventoryConfig;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn current() -> Value {
    serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/build-info.json"))).expect("embedded build identity")
}
pub fn rules_fingerprint(config: &InventoryConfig) -> String {
    let mut value = serde_json::to_value(config).expect("strategy parameters");
    let object = value.as_object_mut().unwrap();
    for field in ["mode", "lighter_account", "lighter_account_index", "lighter_address",
        "entropy_account", "entropy_address", "paper_capital_per_venue"] {
        object.remove(field);
    }
    format!("{:x}", Sha256::digest(serde_json::to_vec(&value).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_inventory::{Mode, MarketPair};
    #[test]
    fn rules_identity_ignores_accounts_and_mode_but_detects_strategy_changes() {
        let a = crate::profiles::paper_config(MarketPair::Openai).unwrap();
        let mut b = a.clone(); b.mode = Mode::Live;
        b.lighter_account = "another-account".into(); b.lighter_account_index = Some(123);
        b.entropy_address = format!("0x{}", "1".repeat(40));
        assert_eq!(rules_fingerprint(&a), rules_fingerprint(&b));
        b.grid += rust_decimal::Decimal::ONE;
        assert_ne!(rules_fingerprint(&a), rules_fingerprint(&b));
        assert_eq!(current()["strategy_core_sha256"].as_str().unwrap().len(), 64);
    }
}
