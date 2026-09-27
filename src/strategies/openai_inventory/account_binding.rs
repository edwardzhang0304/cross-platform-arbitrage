//! Resolve only public account metadata. Never reads a vault or handles private keys.
use super::InventoryConfig;
use crate::{config::AccountConfig, secrets::{VaultEntrySummary, account_secret_id}};
use anyhow::{Result, ensure};

/// Live launcher binding; requires the configured account worker to be enabled.
pub fn resolve<'a>(
    config: &InventoryConfig,
    accounts: &'a [AccountConfig],
    entries: &'a [VaultEntrySummary],
) -> Result<(&'a AccountConfig, &'a VaultEntrySummary)> {
    resolve_with_worker_requirement(config, accounts, entries, true)
}

/// Read-only diagnostics may inspect a deliberately disabled account worker.
pub fn resolve_read_only<'a>(
    config: &InventoryConfig,
    accounts: &'a [AccountConfig],
    entries: &'a [VaultEntrySummary],
) -> Result<(&'a AccountConfig, &'a VaultEntrySummary)> {
    resolve_with_worker_requirement(config, accounts, entries, false)
}

fn resolve_with_worker_requirement<'a>(
    config: &InventoryConfig,
    accounts: &'a [AccountConfig],
    entries: &'a [VaultEntrySummary],
    require_worker: bool,
) -> Result<(&'a AccountConfig, &'a VaultEntrySummary)> {
    config.validate()?;
    config.validate_live_identity()?;
    let entropy: Vec<_> = accounts.iter()
        .filter(|a| a.account_id == config.entropy_account).collect();
    ensure!(entropy.len() == 1, "expected exactly one configured Entropy account");
    let entropy = entropy[0];
    ensure!(entropy.address.eq_ignore_ascii_case(&config.entropy_address),
        "configured Entropy master address mismatch");
    ensure!(entropy.enabled && (!require_worker || entropy.worker_enabled),
        "configured Entropy account is disabled");
    let entropy_id = account_secret_id(entropy);
    let entropy_entries: Vec<_> = entries.iter().filter(|e| e.secret_id == entropy_id).collect();
    ensure!(entropy_entries.len() == 1, "expected exactly one configured Entropy API credential");
    let entry = entropy_entries[0];
    ensure!(entry.has_api_wallet_key && entry.account_id == config.entropy_account
        && entry.address.eq_ignore_ascii_case(&config.entropy_address),
        "Entropy credential account/address mismatch");
    let lighter: Vec<_> = entries.iter()
        .filter(|e| e.account_id == config.lighter_account && e.has_lighter_api_key).collect();
    ensure!(lighter.len() == 1, "expected exactly one Lighter API credential for configured account");
    ensure!(lighter[0].lighter_account_index == config.lighter_account_index,
        "Lighter credential account index mismatch");
    Ok((entropy, lighter[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_inventory::{Mode, PAPER_ENTROPY_ADDRESS};
    const ADDRESS: &str = "0x1234567890abcdef1234567890abcdef12345678";

    fn fixture() -> (InventoryConfig, Vec<AccountConfig>, Vec<VaultEntrySummary>) {
        let config = InventoryConfig {
            mode: Mode::Live,
            lighter_account: "my_lighter_rh".into(),
            lighter_account_index: Some(12345),
            entropy_account: "my_entropy".into(),
            entropy_address: ADDRESS.into(),
            ..Default::default()
        };
        let account = serde_json::from_value(serde_json::json!({
            "account_id":"my_entropy", "address":ADDRESS, "secret_id":"entropy_trading"
        })).unwrap();
        let entry = |secret: &str, alias: &str, lighter: bool| VaultEntrySummary {
            secret_id: secret.into(), account_id: alias.into(), address: ADDRESS.into(),
            has_api_wallet_key: !lighter, has_lighter_api_key: lighter,
            lighter_account_index: lighter.then_some(12345),
            lighter_api_key_index: lighter.then_some(4), updated_at_ms: 0,
        };
        (config, vec![account], vec![entry("entropy_trading", "my_entropy", false),
            entry("lighter_trading", "my_lighter_rh", true)])
    }

    #[test]
    fn custom_accounts_resolve_without_o1_and_address_case_is_insensitive() {
        let (config, mut accounts, entries) = fixture();
        accounts[0].address = format!("0x{}", ADDRESS[2..].to_uppercase());
        let (entropy, lighter) = resolve(&config, &accounts, &entries).unwrap();
        assert_eq!(entropy.account_id, "my_entropy");
        assert_eq!(lighter.secret_id, "lighter_trading");
    }

    #[test]
    fn live_never_inherits_paper_identity_or_unpinned_lighter_account() {
        let (mut config, _, _) = fixture();
        for address in ["", "0x1", "0x1234567890abcdef1234567890abcdef1234567z",
            "0x0000000000000000000000000000000000000000", PAPER_ENTROPY_ADDRESS] {
            config.entropy_address = address.into();
            assert!(config.validate().is_err(), "accepted {address}");
        }
        config.entropy_address = ADDRESS.into();
        for index in [None, Some(-1)] {
            config.lighter_account_index = index;
            assert!(config.validate().is_err());
        }
        config.lighter_account_index = Some(12345);
        for alias in ["", " my_entropy", "my_entropy\n"] {
            config.entropy_account = alias.into();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn legacy_paper_round_trip_stays_valid_but_is_not_real_account_preflight() {
        let paper = InventoryConfig::default();
        paper.validate().unwrap();
        assert!(paper.validate_live_identity().is_err());
        let value = serde_json::to_value(&paper).unwrap();
        assert!(value.get("lighter_account_index").is_none());
        let mut restored: InventoryConfig = serde_json::from_value(value).unwrap();
        assert_eq!(restored, paper);
        restored.mode = Mode::Live;
        assert!(restored.validate().is_err());
    }

    #[test]
    fn rejects_wrong_alias_address_index_disabled_or_ambiguous_credentials() {
        let (config, accounts, entries) = fixture();
        let rejected = |a: &[AccountConfig], e: &[VaultEntrySummary]| {
            assert!(resolve(&config, a, e).is_err());
        };
        rejected(&[], &entries);
        rejected(&[accounts[0].clone(), accounts[0].clone()], &entries);
        let mut a = accounts.clone(); a[0].enabled = false; rejected(&a, &entries);
        let mut a = accounts.clone(); a[0].worker_enabled = false; rejected(&a, &entries);
        let mut a = accounts.clone(); a[0].address = PAPER_ENTROPY_ADDRESS.into(); rejected(&a, &entries);
        let mut a = accounts.clone(); a[0].secret_id = "missing".into(); rejected(&a, &entries);
        let mut e = entries.clone(); e[0].account_id = "different".into(); rejected(&accounts, &e);
        let mut e = entries.clone(); e[0].address = PAPER_ENTROPY_ADDRESS.into(); rejected(&accounts, &e);
        let mut e = entries.clone(); e[0].has_api_wallet_key = false; rejected(&accounts, &e);
        let mut e = entries.clone(); e[1].account_id = "different".into(); rejected(&accounts, &e);
        let mut e = entries.clone(); e[1].lighter_account_index = Some(999); rejected(&accounts, &e);
        let mut e = entries.clone(); e[1].lighter_account_index = None; rejected(&accounts, &e);
        let mut e = entries.clone(); e.push(e[0].clone()); rejected(&accounts, &e);
        let mut e = entries.clone(); e.push(e[1].clone()); rejected(&accounts, &e);
    }

    #[test]
    fn configured_live_identity_accepts_current_inventory_rules() {
        let (config, _, _) = fixture();
        config.validate().unwrap();
        let mut c = config.clone(); c.direction_policy = crate::openai_inventory::DirectionPolicy::Both;
        assert!(c.validate().is_ok());
        let mut c = config.clone(); c.exit_policy = crate::openai_inventory::ExitPolicy::PerGroup;
        assert!(c.validate().is_ok());
        let mut c = config; c.decision_ms = Some(1000); c.entry_confirmation_ms = Some(5000);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn read_only_diagnostics_allow_disabled_worker_but_live_does_not() {
        let (config, mut accounts, entries) = fixture();
        accounts[0].worker_enabled = false;
        assert!(resolve(&config, &accounts, &entries).is_err());
        assert!(resolve_read_only(&config, &accounts, &entries).is_ok());
        accounts[0].enabled = false;
        assert!(resolve_read_only(&config, &accounts, &entries).is_err());
    }
}
