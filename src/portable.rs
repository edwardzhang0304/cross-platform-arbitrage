use anyhow::{Context, Result, ensure};
use std::{fs::{self, File, OpenOptions}, path::{Path, PathBuf}};
use serde::{Serialize, Deserialize};
use crate::{config::AppConfig, openai_inventory::{InventoryConfig, Snapshot}};

pub const VAULT: &str = "keys/trading.vault";
pub const DATABASE: &str = "runtime/openai-inventory/live.sqlite";
pub const SETTINGS: &str = "config/accounts.json";
pub const STRATEGY: &str = "config/strategy.json";

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings { pub accounts: AppConfig, pub strategy: InventoryConfig }
#[derive(Clone)]
pub struct ProfilePaths { pub accounts:PathBuf, pub strategy:PathBuf, pub vault:PathBuf, pub database:PathBuf, pub market:crate::openai_inventory::MarketPair, pub profile:crate::account_profiles::AccountProfile }
#[derive(Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
struct PublicBinding { market:crate::openai_inventory::MarketPair, lighter_alias:String, lighter_index:i64, lighter_address:String, entropy_alias:String, entropy_address:String }
impl ProfilePaths {
    pub fn new(root:&Path,market:crate::openai_inventory::MarketPair)->Self {
        Self::for_profile(root,crate::account_profiles::AccountProfile::legacy(market))
    }
    pub fn for_profile(root:&Path,profile:crate::account_profiles::AccountProfile)->Self {
        let market=profile.market;
        if profile.is_legacy() && market==crate::openai_inventory::MarketPair::Openai {return Self {accounts:root.join(SETTINGS),strategy:root.join(STRATEGY),vault:root.join(VAULT),database:root.join(DATABASE),market,profile};}
        let folder=root.join("profiles").join(format!("{}-live",profile.id));
        Self {accounts:folder.join("config/profile.json"),strategy:folder.join("config/profile.json"),vault:folder.join("keys/trading.vault"),database:folder.join("runtime/inventory.sqlite"),market,profile}
    }
    pub fn load(&self)->Result<Option<Settings>> {
        self.profile.validate()?;
        if !self.accounts.exists() && !self.strategy.exists() {return Ok(None);}
        let mut s:Settings=if self.profile.is_legacy() && self.market==crate::openai_inventory::MarketPair::Openai {
            Settings {accounts:serde_json::from_slice(&fs::read(&self.accounts)?)?,strategy:serde_json::from_slice(&fs::read(&self.strategy)?)?}
        } else {serde_json::from_slice(&fs::read(&self.accounts)?)?};
        self.profile.validate_config(&s.strategy)?;
        s.accounts.secrets.vault_path=self.vault.to_string_lossy().into_owned();s.accounts.secrets.allow_env_fallback=false;
        Ok(Some(s))
    }
    pub fn save(&self,s:&Settings)->Result<()> {
        self.profile.validate_config(&s.strategy)?;
        if self.profile.is_legacy() && self.market==crate::openai_inventory::MarketPair::Openai {
            atomic_json(&self.accounts,&s.accounts)?;atomic_json(&self.strategy,&s.strategy)
        } else {atomic_json(&self.accounts,s)}
    }
    pub fn upgrade_rules(&self, s: &mut Settings) -> Result<()> {
        let desired = crate::openai_inventory::rules_upgrade::current_config(&s.strategy)?;
        crate::openai_inventory::rules_upgrade::migrate_ledger(&self.database, &desired)?;
        if desired != s.strategy {
            let mut next = s.clone(); next.strategy = desired;
            // Commit the durable ledger first. If this atomic file replacement
            // fails, reloading the original config safely retries the same upgrade.
            self.save(&next)?;
            *s = next;
        }
        Ok(())
    }
    /// Keep legacy ledger/config bytes intact when learning the missing L1 address.
    pub fn bind_public_address(&self,c:&InventoryConfig,lighter_address:&str)->Result<()> {self.check_or_bind_public_address(c,lighter_address,true)}
    pub fn check_public_address(&self,c:&InventoryConfig,lighter_address:&str)->Result<()> {self.check_or_bind_public_address(c,lighter_address,false)}
    fn check_or_bind_public_address(&self,c:&InventoryConfig,lighter_address:&str,persist:bool)->Result<()> {
        self.profile.validate_config(c)?;
        ensure!(lighter_address.len()==42 && lighter_address.starts_with("0x") && lighter_address[2..].bytes().all(|b|b.is_ascii_hexdigit()),"invalid public Lighter address");
        if let Some(expected)=&c.lighter_address {ensure!(expected.eq_ignore_ascii_case(lighter_address),"Lighter public binding mismatch");}
        let binding=PublicBinding{market:self.market,lighter_alias:c.lighter_account.clone(),lighter_index:c.lighter_account_index.context("missing account index")?,lighter_address:lighter_address.to_ascii_lowercase(),entropy_alias:c.entropy_account.clone(),entropy_address:c.entropy_address.to_ascii_lowercase()};
        let path=self.strategy.with_file_name("account-bindings.json");
        if path.exists() {ensure!(serde_json::from_slice::<PublicBinding>(&fs::read(path)?)?==binding,"account binding changed; refuse to redirect an existing profile");}
        else if persist {atomic_json(&path,&binding)?;}
        Ok(())
    }
    pub fn bound_strategy(&self,c:&InventoryConfig)->Result<InventoryConfig> {
        self.profile.validate_config(c)?;
        let mut out=c.clone();
        if out.lighter_address.is_none() {
            let path=self.strategy.with_file_name("account-bindings.json");
            let b:PublicBinding=serde_json::from_slice(&fs::read(path).context("请先解锁原 OPENAI 账户，核对原 Lighter 地址后再配置其他账户")?)?;
            self.bind_public_address(c,&b.lighter_address)?;
            out.lighter_address=Some(b.lighter_address);
        }
        Ok(out)
    }
}
impl Settings {
    pub fn load() -> Result<Self> {
        let mut accounts: AppConfig = serde_json::from_slice(&fs::read(SETTINGS)?)?;
        // A package may never redirect secret loading to another path or environment.
        accounts.secrets.vault_path = VAULT.into();
        accounts.secrets.allow_env_fallback = false;
        let strategy = serde_json::from_slice(&fs::read(STRATEGY)?)?;
        Ok(Self { accounts, strategy })
    }
    pub fn save(&self) -> Result<()> {
        self.strategy.validate()?;
        self.strategy.validate_live_identity()?;
        atomic_json(Path::new(SETTINGS), &self.accounts)?;
        atomic_json(Path::new(STRATEGY), &self.strategy)
    }
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
        if moved == 0 { return Err(std::io::Error::last_os_error().into()); }
    }
    #[cfg(not(windows))]
    fs::rename(temp, path)?;
    Ok(())
}
pub fn prepare(path: &Path) -> Result<(PathBuf, File)> {
    fs::create_dir_all(path)?;
    let root = path.canonicalize()?;
    ensure!(!root.ancestors().any(|p|p.join("paper-only.json").exists()),"模拟数据目录不能用于实盘程序");
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(root.join("program.lock"))?;
    lock.try_lock().context("该数据目录已有程序运行")?;
    atomic_json(&root.join("live-only.json"),&serde_json::json!({"schema":1,"mode":"live"}))?;
    for folder in ["config", "keys", "runtime/openai-inventory"] { fs::create_dir_all(root.join(folder))?; }
    Ok((root, lock))
}
/// Makes a consistent SQLite backup. Does not read or copy the encrypted vault.
/// A stopped ledger is required; the user shuts down the old console separately.
pub fn export_legacy(source: &Path, account_config: &Path, output: &Path) -> Result<()> {
    use rusqlite::{Connection, OpenFlags};
    ensure!(!output.exists(), "目标目录已经存在，请使用新的目录名");
    let db_path = source.join(DATABASE);
    let lock = OpenOptions::new().read(true).write(true).open(db_path.with_extension("lock"))?;
    lock.try_lock().context("原程序仍占用账本；请先停止交易并关闭旧服务")?;
    let db = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "原账本完整性检查失败");
    let body: String = db.query_row("SELECT body FROM state WHERE id=1", [], |r| r.get(0))?;
    let state: Snapshot = serde_json::from_str(&body)?;
    ensure!(state.pending.is_none() && state.live_orphan.is_none() && state.stop_requested
        && state.status == crate::openai_inventory::Status::Stopped,
        "需要先停止交易并处理未完成的双腿操作");
    ensure!(state.positions[0].units == state.direction.sign() * state.paired_units()
        && state.positions[1].units == -state.direction.sign() * state.paired_units(),
        "原账本双腿数量与各组数量不一致，请先处理配平");
    let mut accounts: AppConfig = toml::from_str(&fs::read_to_string(account_config)?)?;
    accounts.accounts.retain(|a| a.account_id == state.config.entropy_account);
    ensure!(accounts.accounts.len() == 1, "找不到对应的 Entropy 账户配置");
    accounts.secrets.vault_path = VAULT.into();
    accounts.secrets.allow_env_fallback = false;
    fs::create_dir_all(output.join("runtime/openai-inventory"))?;
    fs::create_dir_all(output.join("keys"))?;
    let mut destination = Connection::open(output.join(DATABASE))?;
    rusqlite::backup::Backup::new(&db, &mut destination)?.run_to_completion(128, std::time::Duration::from_millis(10), None)?;
    atomic_json(&output.join(SETTINGS), &accounts)?;
    atomic_json(&output.join(STRATEGY), &state.config)?;
    fs::write(output.join("密钥请本人复制.txt"), "此目录不含密钥。请本人把原加密 trading.vault 复制到 keys/trading.vault，不复制密码缓存。把此目录作为 Windows EXE 旁的 data 目录。旧 Mac 必须保持关闭，Windows 由本人重新解锁、核对后启动。")?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn data_directory_lock_excludes_a_second_process_handle() {
        let path = std::env::temp_dir().join(format!("portable-lock-{}", std::process::id()));
        let (_, lock) = prepare(&path).unwrap();
        assert!(prepare(&path).is_err());
        drop(lock);
        drop(prepare(&path).unwrap());
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn replacing_settings_preserves_a_complete_json_document() {
        let dir = std::env::temp_dir().join(format!("portable-settings-{}", uuid::Uuid::new_v4()));
        let path = dir.join("settings.json");
        atomic_json(&path, &serde_json::json!({"version":1})).unwrap();
        atomic_json(&path, &serde_json::json!({"version":2})).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["version"], 2);
        assert!(!path.with_extension("tmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn export_requires_stopped_balanced_unlocked_ledger_and_never_copies_keys() {
        use crate::openai_inventory::{Status, store::Store};
        let dir = std::env::temp_dir().join(format!("portable-export-{}", uuid::Uuid::new_v4()));
        let source = dir.join("source");
        let cfg: InventoryConfig = serde_json::from_str(include_str!("../tests/fixtures/inventory/live-strategy.json")).unwrap();
        let (mut store, mut state) = Store::open(&source.join(DATABASE), &cfg).unwrap();
        let account_file = source.join("accounts.toml");
        fs::write(&account_file, format!("[[accounts]]\naccount_id = \"{}\"\naddress = \"{}\"\nsecret_id = \"original-reference\"\n", cfg.entropy_account, cfg.entropy_address)).unwrap();
        let output = dir.join("transfer");
        state.status = Status::Stopped;
        state.stop_requested = true;
        state.closed_groups = 4;
        store.commit(&state, 42, "synthetic-stopped").unwrap();
        assert!(export_legacy(&source, &account_file, &output).is_err());
        assert!(!output.exists());
        state.status = Status::Running;
        store.commit(&state, 43, "synthetic-running").unwrap();
        drop(store);
        assert!(export_legacy(&source, &account_file, &output).is_err());
        let (mut store, _) = Store::open(&source.join(DATABASE), &cfg).unwrap();
        state.status = Status::Stopped;
        state.positions[0].units = 90;
        store.commit(&state, 44, "synthetic-unpaired").unwrap();
        drop(store);
        assert!(export_legacy(&source, &account_file, &output).is_err());
        let (mut store, _) = Store::open(&source.join(DATABASE), &cfg).unwrap();
        state.positions[0].units = 0;
        store.commit(&state, 45, "synthetic-balanced").unwrap();
        drop(store);
        export_legacy(&source, &account_file, &output).unwrap();
        assert!(!output.join(VAULT).exists());
        let db = rusqlite::Connection::open(output.join(DATABASE)).unwrap();
        let body: String = db.query_row("SELECT body FROM state WHERE id=1", [], |r|r.get(0)).unwrap();
        let restored: Snapshot = serde_json::from_str(&body).unwrap();
        assert_eq!(restored.closed_groups, 4);
        assert_eq!(restored.config, cfg);
        let accounts: AppConfig = serde_json::from_slice(&fs::read(output.join(SETTINGS)).unwrap()).unwrap();
        assert_eq!(accounts.accounts[0].secret_id, "original-reference");
        assert_eq!(accounts.secrets.vault_path, VAULT);
        assert!(!accounts.secrets.allow_env_fallback);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn profile_paths_preserve_legacy_and_pin_public_wallets() {
        use crate::openai_inventory::MarketPair;
        let dir=std::env::temp_dir().join(format!("profile-paths-{}",uuid::Uuid::new_v4()));
        let openai=ProfilePaths::new(&dir,MarketPair::Openai);let anth=ProfilePaths::new(&dir,MarketPair::Anth);
        assert_eq!(openai.database,dir.join(DATABASE));assert_ne!(openai.vault,anth.vault);
        assert_ne!(openai.database,anth.database);assert_ne!(openai.accounts,anth.accounts);
        let c:InventoryConfig=serde_json::from_str(include_str!("../tests/fixtures/inventory/live-strategy.json")).unwrap();
        let address="0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(openai.bound_strategy(&c).is_err());
        openai.check_public_address(&c,address).unwrap();assert!(!openai.strategy.with_file_name("account-bindings.json").exists());
        openai.bind_public_address(&c,address).unwrap();
        assert_eq!(openai.bound_strategy(&c).unwrap().lighter_address.as_deref(),Some(address));
        assert!(openai.check_public_address(&c,"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").is_err());
        assert_eq!(openai.bound_strategy(&c).unwrap().lighter_address.as_deref(),Some(address));
        assert!(!openai.vault.exists());std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn live_program_rejects_simulation_data_and_nested_directories() {
        let dir=std::env::temp_dir().join(format!("mode-roots-{}",uuid::Uuid::new_v4()));
        let (_,lock)=crate::profiles::prepare_paper_root(&dir).unwrap();
        assert!(prepare(&dir).is_err());assert!(prepare(&dir.join("nested")).is_err());
        drop(lock);std::fs::remove_dir_all(dir).unwrap();
    }

}

#[cfg(test)]
mod multi_account_tests {
    use super::*;
    use crate::{account_profiles::AccountProfile,openai_inventory::{MarketPair,Status,store::Store}};
    #[test]
    fn same_market_settings_ledger_commands_and_notifications_remain_separate_after_restart() {
        let root=std::env::temp_dir().join(format!("cpa-isolation-{}",uuid::Uuid::new_v4()));
        let p1=AccountProfile::legacy(MarketPair::Openai);
        let p2=AccountProfile::new(MarketPair::Openai,"second".into(),&uuid::Uuid::new_v4().to_string()).unwrap();
        let paths=[ProfilePaths::for_profile(&root,p1),ProfilePaths::for_profile(&root,p2)];
        let mut configs=vec![];
        for (i,p) in paths.iter().enumerate() {
            let mut c:InventoryConfig=serde_json::from_str(include_str!("../tests/fixtures/inventory/live-strategy.json")).unwrap();
            c.lighter_account=p.profile.alias("lighter");c.entropy_account=p.profile.alias("entropy");
            c.lighter_account_index=Some(100+i as i64);c.lighter_address=Some(format!("0x{:040x}",11+i));c.entropy_address=format!("0x{:040x}",21+i);
            let settings=Settings{accounts:AppConfig::default(),strategy:c.clone()};p.save(&settings).unwrap();
            p.bind_public_address(&c,c.lighter_address.as_ref().unwrap()).unwrap();
            let (mut store,mut state)=Store::open(&p.database,&c).unwrap();state.status=Status::Stopped;state.closed_groups=17+i as u64;
            store.commit(&state,100,"synthetic-multi-account").unwrap();drop(store);configs.push(c);
        }
        crate::profiles::reject_shared_live_accounts(&configs[0],&configs[1..]).unwrap();
        for (i,p) in paths.iter().enumerate() {
            let settings=p.load().unwrap().unwrap();assert_eq!(settings.strategy,configs[i]);
            assert_eq!(settings.accounts.secrets.vault_path,p.vault.to_string_lossy());assert!(!settings.accounts.secrets.allow_env_fallback);
            let (store,state)=Store::open(&p.database,&settings.strategy).unwrap();assert_eq!(state.closed_groups,17+i as u64);drop(store);
            assert!(Store::open(&p.database,&configs[1-i]).is_err(),"cannot adopt a peer's ledger");
        }
        assert!(paths[1].save(&Settings{accounts:AppConfig::default(),strategy:configs[0].clone()}).is_err());
        for change in 0..4 {
            let mut c=configs[1].clone();match change {0=>c.lighter_account_index=configs[0].lighter_account_index,1=>c.entropy_address=configs[0].entropy_address.clone(),2=>c.lighter_address=configs[0].lighter_address.clone(),_=>c.entropy_account=configs[0].entropy_account.clone()}
            assert!(crate::profiles::reject_shared_live_accounts(&c,&configs[..1]).is_err(),"same-market duplicate identity must be rejected");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
