//! Profile identity is selected by the host, never inferred from a wallet or a ledger.
use crate::openai_inventory::{InventoryConfig, MarketPair, Mode, PAPER_ENTROPY_ADDRESS};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileId { pub market: MarketPair, pub mode: Mode }

impl ProfileId {
    pub fn new(market: MarketPair, mode: Mode) -> Self { Self { market, mode } }
    pub fn key(self) -> String {
        format!("{}-{}", self.market.id(), if self.mode == Mode::Live { "live" } else { "paper" })
    }
    pub fn directory(self, root: &Path) -> PathBuf { root.join("profiles").join(self.key()) }
    pub fn ledger(self, root: &Path) -> PathBuf { self.directory(root).join("runtime/inventory.sqlite") }
    pub fn vault(self, root: &Path) -> Result<PathBuf> {
        ensure!(self.mode == Mode::Live, "virtual profile cannot own a vault");
        Ok(self.directory(root).join("keys/trading.vault"))
    }
    pub fn validate(self, config: &InventoryConfig) -> Result<()> {
        ensure!(config.market == self.market && config.mode == self.mode, "profile market/mode mismatch");
        config.validate()?;
        if self.mode == Mode::Paper {
            ensure!(config.lighter_account == format!("{}:virtual:lighter", self.market.id())
                && config.entropy_account == format!("{}:virtual:entropy", self.market.id())
                && config.lighter_account_index.is_none() && config.lighter_address.is_none()
                && config.entropy_address == PAPER_ENTROPY_ADDRESS,
                "virtual profiles reject real account identifiers");
        } else {
            config.validate_live_identity()?;
            if self.market == MarketPair::Anth {
                ensure!(config.lighter_address.as_ref().is_some_and(|a|!a.eq_ignore_ascii_case(&config.entropy_address)),
                    "ANTH requires separate new wallets for the two platforms");
                ensure!(config.lighter_account == "anth:live:lighter"
                    && config.entropy_account == "anth:live:entropy", "ANTH live aliases must be namespaced");
            }
        }
        Ok(())
    }
}

/// Only public account identities are compared. Never unlock a vault to choose a profile.
pub fn reject_shared_live_accounts(candidate: &InventoryConfig, others: &[InventoryConfig]) -> Result<()> {
    if candidate.mode != Mode::Live { return Ok(()); }
    candidate.validate_live_identity()?;
    for other in others.iter().filter(|c| c.mode == Mode::Live && c.market != candidate.market) {
        other.validate_live_identity()?;
        ensure!(candidate.lighter_account_index != other.lighter_account_index,
            "Lighter account index is already bound to another market");
        let addresses = |c: &InventoryConfig| {
            let mut a = vec![c.entropy_address.to_ascii_lowercase()];
            if let Some(l) = &c.lighter_address { a.push(l.to_ascii_lowercase()); }
            a
        };
        let a = addresses(candidate); let b = addresses(other);
        ensure!(!a.iter().any(|x| b.contains(x)), "wallet is already bound to another market");
        ensure!(candidate.lighter_account != other.lighter_account && candidate.entropy_account != other.entropy_account,
            "account alias is already bound to another market");
    }
    Ok(())
}

pub fn paper_config(market: MarketPair) -> Result<InventoryConfig> {
    let mut c: InventoryConfig = serde_json::from_str(include_str!("../config/strategy.example.json"))?;
    c.market = market; c.mode = Mode::Paper;
    c.lighter_account = format!("{}:virtual:lighter", market.id());
    c.entropy_account = format!("{}:virtual:entropy", market.id());
    c.lighter_address = None; c.lighter_account_index = None;
    c.entropy_address = PAPER_ENTROPY_ADDRESS.into();
    ProfileId::new(market, Mode::Paper).validate(&c)?;
    Ok(c)
}

/// A separate root is mandatory. Refuse an existing unlabelled directory rather than
/// guessing whether the user's copied files are simulation data or real money.
pub fn prepare_paper_root(path: &Path) -> Result<(PathBuf, fs::File)> {
    fs::create_dir_all(path)?;
    let root = path.canonicalize()?;
    ensure!(!root.ancestors().any(|p|p.join("live-only.json").exists()), "live data directory cannot contain a simulation profile");
    let marker = root.join("paper-only.json");
    let expected = serde_json::json!({"schema":1,"mode":"paper","application":"paired-paper"});
    if marker.exists() {
        ensure!(serde_json::from_slice::<serde_json::Value>(&fs::read(&marker)?)? == expected,
            "data root belongs to another application/mode");
    } else {
        ensure!(fs::read_dir(&root)?.next().is_none(), "choose a new empty simulation directory; never reuse live data");
    }
    ensure!(!root.join("keys").exists() && !root.join("config/accounts.json").exists(), "live data is forbidden in paper root");
    let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(root.join("program.lock"))?;
    lock.try_lock().context("simulation directory already in use")?;
    crate::portable::atomic_json(&marker, &expected)?;
    Ok((root, lock))
}

pub fn load_paper_config(root: &Path, market: MarketPair) -> Result<InventoryConfig> {
    let id = ProfileId::new(market, Mode::Paper);
    let folder = id.directory(root);
    ensure!(!folder.join("keys").exists(), "paper profile must not contain keys");
    let file = folder.join("config/strategy.json");
    let config = if file.exists() { serde_json::from_slice(&fs::read(&file)?)? }
        else { let c = paper_config(market)?; crate::portable::atomic_json(&file, &c)?; c };
    id.validate(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_and_mode_cannot_be_swapped_or_given_real_accounts() {
        let a = paper_config(MarketPair::Anth).unwrap();
        assert_eq!(a.market.quantity(637).to_string(), "0.00637");
        assert!(ProfileId::new(MarketPair::Openai, Mode::Paper).validate(&a).is_err());
        assert!(ProfileId::new(MarketPair::Anth, Mode::Live).validate(&a).is_err());
        let mut real = a.clone(); real.lighter_account_index = Some(12345);
        assert!(ProfileId::new(MarketPair::Anth, Mode::Paper).validate(&real).is_err());
        assert!(ProfileId::new(MarketPair::Anth, Mode::Paper).vault(Path::new("data")).is_err());
    }
    #[test]
    fn copying_or_reusing_a_live_root_is_refused() {
        let root = std::env::temp_dir().join(format!("paper-root-{}",uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("keys")).unwrap();
        assert!(prepare_paper_root(&root).is_err());
        fs::remove_dir_all(&root).unwrap();
        let (_, lock) = prepare_paper_root(&root).unwrap();
        assert!(prepare_paper_root(&root).is_err());
        drop(lock); drop(prepare_paper_root(&root).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shared_live_wallet_or_index_is_rejected() {
        let original: InventoryConfig = serde_json::from_str(include_str!("../tests/fixtures/inventory/live-strategy.json")).unwrap();
        let mut anth = original.clone(); anth.market=MarketPair::Anth;
        anth.lighter_address=Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
        anth.lighter_account="anth:live:lighter".into(); anth.entropy_account="anth:live:entropy".into();
        assert!(reject_shared_live_accounts(&anth,&[original.clone()]).is_err());
        anth.lighter_account_index=Some(original.lighter_account_index.unwrap()+1);
        assert!(reject_shared_live_accounts(&anth,&[original.clone()]).is_err());
        anth.entropy_address="0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        reject_shared_live_accounts(&anth,&[original]).unwrap();
    }
}
