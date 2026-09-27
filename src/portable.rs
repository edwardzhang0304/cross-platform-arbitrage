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
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(root.join("program.lock"))?;
    lock.try_lock().context("该数据目录已有程序运行")?;
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
}
