use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use chacha20poly1305::{
    Key, XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    config::{AccountConfig, AppConfig},
    domain::now_ms,
};

const VAULT_AAD: &[u8] = b"trade_xyz_bot.secret_vault.v1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const DEFAULT_ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const DEFAULT_ARGON2_ITERATIONS: u32 = 3;
const DEFAULT_ARGON2_PARALLELISM: u32 = 1;
pub const VAULT_SESSION_CACHE_PATH: &str = ".codex-longrun/vault-session-cache.json";
#[cfg(all(target_os = "macos", not(test)))]
const MACOS_VAULT_KEYCHAIN_SERVICE: &str = "trade.xyz-vault-password";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultFile {
    pub version: u32,
    pub algorithm: String,
    pub kdf: VaultKdf,
    pub salt_b64: String,
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultKdf {
    pub name: String,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlainVault {
    version: u32,
    updated_at_ms: u64,
    entries: BTreeMap<String, VaultSecretEntry>,
}

#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct VaultSecretEntry {
    pub secret_id: String,
    pub account_id: String,
    pub address: String,
    pub api_wallet_private_key: String,
    #[serde(default)]
    pub lighter_api_private_key: Option<String>,
    #[serde(default)]
    pub lighter_account_index: Option<i64>,
    #[serde(default)]
    pub lighter_api_key_index: Option<u8>,
    pub updated_at_ms: u64,
}
impl fmt::Debug for VaultSecretEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VaultSecretEntry(<redacted>)")
    }
}

#[derive(Clone)]
pub struct SecretUpsert {
    pub secret_id: String,
    pub account_id: String,
    pub address: String,
    pub api_wallet_private_key: String,
}

impl fmt::Debug for SecretUpsert {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretUpsert")
            .field("secret_id", &self.secret_id)
            .field("account_id", &self.account_id)
            .field("address", &self.address)
            .field("api_wallet_private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct LighterSecretUpsert {
    pub secret_id: String,
    pub account_id: String,
    pub l1_address: String,
    pub account_index: i64,
    pub api_key_index: u8,
    pub api_private_key: String,
}

impl fmt::Debug for LighterSecretUpsert {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LighterSecretUpsert")
            .field("secret_id", &self.secret_id)
            .field("account_id", &self.account_id)
            .field("l1_address", &self.l1_address)
            .field("account_index", &self.account_index)
            .field("api_key_index", &self.api_key_index)
            .field("api_private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VaultSummary {
    pub exists: bool,
    pub unlocked: bool,
    pub path: String,
    pub entry_count: Option<usize>,
    pub entries: Vec<VaultEntrySummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VaultEntrySummary {
    pub secret_id: String,
    pub account_id: String,
    pub address: String,
    pub has_api_wallet_key: bool,
    pub has_lighter_api_key: bool,
    pub lighter_account_index: Option<i64>,
    pub lighter_api_key_index: Option<u8>,
    pub updated_at_ms: u64,
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct ApiWalletSecret {
    pub secret_id: String,
    pub account_id: String,
    pub private_key: String,
}
impl fmt::Debug for ApiWalletSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiWalletSecret(<redacted>)")
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct LighterApiKeySecret {
    pub secret_id: String,
    pub account_id: String,
    pub account_index: i64,
    pub api_key_index: u8,
    pub private_key: String,
}

impl fmt::Debug for LighterApiKeySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LighterApiKeySecret")
            .field("secret_id", &self.secret_id)
            .field("account_id", &self.account_id)
            .field("account_index", &self.account_index)
            .field("api_key_index", &self.api_key_index)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct TransferWalletSecret {
    pub secret_id: String,
    pub account_id: String,
    pub private_key: String,
    pub signer_address: String,
}
impl fmt::Debug for TransferWalletSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransferWalletSecret(<redacted>)")
    }
}

impl VaultKdf {
    pub fn default_interactive() -> Self {
        Self {
            name: "argon2id".to_string(),
            memory_kib: DEFAULT_ARGON2_MEMORY_KIB,
            iterations: DEFAULT_ARGON2_ITERATIONS,
            parallelism: DEFAULT_ARGON2_PARALLELISM,
        }
    }
}

pub fn vault_status(path: &Path) -> VaultSummary {
    VaultSummary {
        exists: path.exists(),
        unlocked: false,
        path: path.display().to_string(),
        entry_count: None,
        entries: Vec::new(),
    }
}

pub fn unlock_vault(path: &Path, password: &str) -> Result<VaultSummary> {
    let plain = decrypt_vault_file(path, password)?;
    Ok(summary_from_plain(path, &plain))
}

pub fn change_vault_password(
    path: &Path,
    current_password: &str,
    new_password: &str,
) -> Result<VaultSummary> {
    validate_password(new_password)?;
    anyhow::ensure!(path.exists(), "vault file does not exist");

    let mut plain = decrypt_vault_file(path, current_password)?;
    plain.updated_at_ms = now_ms();
    write_encrypted_vault(path, new_password, &plain, VaultKdf::default_interactive())?;
    Ok(summary_from_plain(path, &plain))
}

/// Rename account and secret references without changing key material or addresses.
/// The caller must quiesce users of these bindings and update their configurations.
pub fn rename_vault_accounts(
    path: &Path,
    password: &str,
    renames: &[(String, String)],
) -> Result<VaultSummary> {
    let original = fs::read(path)?;
    let mut plain = decrypt_vault_file(path, password)?;
    let mut entries = BTreeMap::new();
    for (old, new) in renames {
        anyhow::ensure!(
            !old.is_empty() && !new.is_empty() && old != new,
            "invalid rename"
        );
        anyhow::ensure!(
            plain.entries.values().any(|e| &e.account_id == old),
            "source account missing"
        );
        anyhow::ensure!(
            !plain.entries.values().any(|e| &e.account_id == new),
            "destination account exists"
        );
    }
    for (_, mut entry) in std::mem::take(&mut plain.entries) {
        if let Some((old, new)) = renames.iter().find(|(old, _)| *old == entry.account_id) {
            entry.secret_id = if let Some(suffix) = entry.secret_id.strip_prefix(&format!("{old}_"))
            {
                format!("{new}_{suffix}")
            } else {
                entry.secret_id.clone()
            };
            entry.account_id = new.clone();
            entry.updated_at_ms = now_ms();
        }
        anyhow::ensure!(
            !entries.contains_key(&entry.secret_id),
            "destination secret reference exists"
        );
        entries.insert(entry.secret_id.clone(), entry);
    }
    plain.entries = entries;
    plain.updated_at_ms = now_ms();
    let temp = path.with_extension(format!("rename-{}.tmp", uuid::Uuid::new_v4()));
    write_encrypted_vault(&temp, password, &plain, VaultKdf::default_interactive())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    anyhow::ensure!(
        fs::read(path)? == original,
        "vault changed during rename; retry after concurrent writes stop"
    );
    fs::rename(&temp, path)?;
    Ok(summary_from_plain(path, &plain))
}

pub fn upsert_secret(path: &Path, password: &str, input: SecretUpsert) -> Result<VaultSummary> {
    validate_password(password)?;
    validate_secret_input(&input)?;

    let mut plain = if path.exists() {
        decrypt_vault_file(path, password)?
    } else {
        PlainVault {
            version: 1,
            updated_at_ms: now_ms(),
            entries: BTreeMap::new(),
        }
    };

    if let Some(existing) = plain.entries.get(&input.secret_id) {
        anyhow::ensure!(
            existing.lighter_api_private_key.is_none(),
            "secret_id {} is already used by a Lighter key; choose a distinct Trade secret_id",
            input.secret_id
        );
    }
    let entry = VaultSecretEntry {
        secret_id: input.secret_id,
        account_id: input.account_id,
        address: input.address,
        api_wallet_private_key: normalize_private_key(&input.api_wallet_private_key)?,
        lighter_api_private_key: None,
        lighter_account_index: None,
        lighter_api_key_index: None,
        updated_at_ms: now_ms(),
    };
    plain.updated_at_ms = now_ms();
    plain.entries.insert(entry.secret_id.clone(), entry);

    write_encrypted_vault(path, password, &plain, VaultKdf::default_interactive())?;
    Ok(summary_from_plain(path, &plain))
}

/// Validates every row before replacing the encrypted file. A malformed row
/// therefore cannot leave a partially imported spreadsheet batch.
pub fn upsert_secrets(
    path: &Path,
    password: &str,
    inputs: Vec<SecretUpsert>,
) -> Result<VaultSummary> {
    validate_password(password)?;
    anyhow::ensure!(
        !inputs.is_empty(),
        "batch must contain at least one account"
    );
    anyhow::ensure!(inputs.len() <= 100, "batch cannot exceed 100 accounts");

    let mut account_ids = BTreeSet::new();
    let mut secret_ids = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    for (index, input) in inputs.iter().enumerate() {
        validate_secret_input(input)
            .with_context(|| format!("batch row {} is invalid", index + 1))?;
        anyhow::ensure!(
            account_ids.insert(input.account_id.trim().to_ascii_lowercase()),
            "batch row {} repeats account_id {}",
            index + 1,
            input.account_id
        );
        anyhow::ensure!(
            secret_ids.insert(input.secret_id.trim().to_string()),
            "batch row {} repeats secret_id {}",
            index + 1,
            input.secret_id
        );
        anyhow::ensure!(
            addresses.insert(input.address.trim().to_ascii_lowercase()),
            "batch row {} repeats EVM address {}",
            index + 1,
            input.address
        );
    }

    let original = path.exists().then(|| fs::read(path)).transpose()?;
    let mut plain = if path.exists() {
        decrypt_vault_file(path, password)?
    } else {
        PlainVault {
            version: 1,
            updated_at_ms: now_ms(),
            entries: BTreeMap::new(),
        }
    };
    let updated_at_ms = now_ms();
    for input in inputs {
        anyhow::ensure!(
            !plain.entries.values().any(|existing| {
                existing.account_id != input.account_id
                    && existing.address.eq_ignore_ascii_case(&input.address)
            }),
            "EVM address {} already belongs to another account",
            input.address
        );
        if let Some(existing) = plain.entries.get(&input.secret_id) {
            anyhow::ensure!(
                existing.lighter_api_private_key.is_none(),
                "secret_id {} is already used by a Lighter key",
                input.secret_id
            );
            anyhow::ensure!(
                existing.account_id == input.account_id,
                "secret_id {} belongs to another account",
                input.secret_id
            );
        }
        let entry = VaultSecretEntry {
            secret_id: input.secret_id,
            account_id: input.account_id,
            address: input.address,
            api_wallet_private_key: normalize_private_key(&input.api_wallet_private_key)?,
            lighter_api_private_key: None,
            lighter_account_index: None,
            lighter_api_key_index: None,
            updated_at_ms,
        };
        plain.entries.insert(entry.secret_id.clone(), entry);
    }
    plain.updated_at_ms = updated_at_ms;

    let temp = path.with_extension(format!("batch-{}.tmp", uuid::Uuid::new_v4()));
    write_encrypted_vault(&temp, password, &plain, VaultKdf::default_interactive())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    }
    let unchanged = match original {
        Some(original) => fs::read(path)? == original,
        None => !path.exists(),
    };
    if !unchanged {
        let _ = fs::remove_file(&temp);
        anyhow::bail!("vault changed during batch import; retry after concurrent writes stop");
    }
    fs::rename(&temp, path)?;
    Ok(summary_from_plain(path, &plain))
}

/// Stores a Lighter API key separately from the 32-byte Hyperliquid/EVM key.
/// Existing entries remain valid because all Lighter fields are optional.
pub fn upsert_lighter_secret(
    path: &Path,
    password: &str,
    input: LighterSecretUpsert,
) -> Result<VaultSummary> {
    validate_password(password)?;
    validate_lighter_secret_input(&input)?;
    let mut plain = if path.exists() {
        decrypt_vault_file(path, password)?
    } else {
        PlainVault {
            version: 1,
            updated_at_ms: now_ms(),
            entries: BTreeMap::new(),
        }
    };
    let normalized_key = normalize_lighter_private_key(&input.api_private_key)?;
    if let Some(existing) = plain.entries.get(&input.secret_id) {
        anyhow::ensure!(
            existing.api_wallet_private_key.is_empty(),
            "secret_id {} is already used by a Trade key; choose a distinct Lighter secret_id",
            input.secret_id
        );
    }
    let now = now_ms();
    let entry = plain
        .entries
        .entry(input.secret_id.clone())
        .or_insert_with(|| VaultSecretEntry {
            secret_id: input.secret_id.clone(),
            account_id: input.account_id.clone(),
            address: input.l1_address.clone(),
            api_wallet_private_key: String::new(),
            lighter_api_private_key: None,
            lighter_account_index: None,
            lighter_api_key_index: None,
            updated_at_ms: now,
        });
    anyhow::ensure!(
        entry.account_id == input.account_id,
        "secret_id {} belongs to account {}, not {}",
        input.secret_id,
        entry.account_id,
        input.account_id
    );
    entry.address = input.l1_address.clone();
    entry.lighter_api_private_key = Some(normalized_key);
    entry.lighter_account_index = Some(input.account_index);
    entry.lighter_api_key_index = Some(input.api_key_index);
    entry.updated_at_ms = now;
    plain.updated_at_ms = now;
    write_encrypted_vault(path, password, &plain, VaultKdf::default_interactive())?;
    Ok(summary_from_plain(path, &plain))
}

pub fn load_lighter_secret_by_id(
    path: &Path,
    password: &str,
    secret_id: &str,
    expected_account_id: Option<&str>,
) -> Result<LighterApiKeySecret> {
    let plain = decrypt_vault_file(path, password)?;
    let entry = plain
        .entries
        .get(secret_id)
        .with_context(|| format!("secret_id {secret_id} not found in vault"))?;
    if let Some(expected_account_id) = expected_account_id {
        anyhow::ensure!(
            entry.account_id == expected_account_id,
            "secret_id {} belongs to account {}, not {}",
            secret_id,
            entry.account_id,
            expected_account_id
        );
    }
    let private_key = entry
        .lighter_api_private_key
        .as_deref()
        .context("vault entry does not contain a Lighter API private key")?;
    Ok(LighterApiKeySecret {
        secret_id: secret_id.to_string(),
        account_id: entry.account_id.clone(),
        account_index: entry
            .lighter_account_index
            .context("vault entry is missing Lighter account_index")?,
        api_key_index: entry
            .lighter_api_key_index
            .context("vault entry is missing Lighter api_key_index")?,
        private_key: normalize_lighter_private_key(private_key)?,
    })
}

pub fn load_account_secret(
    config: &AppConfig,
    account: &AccountConfig,
    password: Option<&str>,
) -> Result<ApiWalletSecret> {
    if let Some(password) = password {
        let vault_path = PathBuf::from(&config.secrets.vault_path);
        let secret_id = account_secret_id(account);
        return load_secret_by_id(&vault_path, password, &secret_id, Some(&account.account_id));
    }

    if config.secrets.allow_env_fallback && !account.api_wallet_env.trim().is_empty() {
        let private_key = std::env::var(&account.api_wallet_env).with_context(|| {
            format!(
                "environment variable {} is not set for account {}",
                account.api_wallet_env, account.account_id
            )
        })?;
        return Ok(ApiWalletSecret {
            secret_id: account_secret_id(account),
            account_id: account.account_id.clone(),
            private_key: normalize_private_key(&private_key)?,
        });
    }

    anyhow::bail!(
        "account {} requires vault password for secret_id {}",
        account.account_id,
        account_secret_id(account)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedVaultSession {
    version: u32,
    vault_path: String,
    vault_modified_at_ms: u64,
    unlocked_at_ms: u64,
    expires_at_ms: u64,
    protected_password_b64: String,
}

pub fn read_cached_vault_password(vault_path: &Path, now_ms_value: u64) -> Result<Option<String>> {
    let cached = read_cached_vault_password_from_path(
        &PathBuf::from(VAULT_SESSION_CACHE_PATH),
        vault_path,
        now_ms_value,
    )?;
    if cached.is_some() {
        return Ok(cached);
    }
    read_vault_password_directly_from_macos_keychain(vault_path)
}

#[cfg(all(target_os = "macos", not(test)))]
fn read_vault_password_directly_from_macos_keychain(vault_path: &Path) -> Result<Option<String>> {
    if !vault_path.exists() {
        return Ok(None);
    }
    let mut plain = match unprotect_local_secret(&[]) {
        Ok(password) => password,
        Err(_) => return Ok(None),
    };
    let password = String::from_utf8(plain.clone())
        .context("macOS Keychain Vault session password is not UTF-8")?;
    plain.zeroize();
    Ok((!password.trim().is_empty()).then_some(password))
}

#[cfg(any(not(target_os = "macos"), test))]
fn read_vault_password_directly_from_macos_keychain(_vault_path: &Path) -> Result<Option<String>> {
    Ok(None)
}

pub fn read_cached_vault_password_from_path(
    cache_path: &Path,
    vault_path: &Path,
    now_ms_value: u64,
) -> Result<Option<String>> {
    if !cache_path.exists() || !vault_path.exists() {
        return Ok(None);
    }
    let raw = fs::read(cache_path).with_context(|| {
        format!(
            "failed to read Vault session cache {}",
            cache_path.display()
        )
    })?;
    let cache = serde_json::from_slice::<PersistedVaultSession>(&raw)
        .context("failed to parse Vault session cache")?;
    if cache.version != 1
        || !cached_vault_path_matches(cache_path, Path::new(&cache.vault_path), vault_path)
        || now_ms_value > cache.expires_at_ms
    {
        return Ok(None);
    }
    if file_modified_at_ms(vault_path).unwrap_or(0) != cache.vault_modified_at_ms {
        return Ok(None);
    }
    let protected = STANDARD_NO_PAD
        .decode(cache.protected_password_b64)
        .context("failed to decode cached Vault session password")?;
    let mut plain =
        unprotect_local_secret(&protected).context("failed to unprotect cached Vault session")?;
    let password =
        String::from_utf8(plain.clone()).context("cached Vault session password is not UTF-8")?;
    plain.zeroize();
    Ok(Some(password))
}

fn cached_vault_path_matches(cache_path: &Path, cached_path: &Path, vault_path: &Path) -> bool {
    let Ok(expected) = vault_path.canonicalize() else {
        return false;
    };
    let mut candidates = vec![cached_path.to_path_buf()];
    if cached_path.is_relative()
        && let Some(cache_directory) = cache_path.parent()
    {
        candidates.push(cache_directory.join(cached_path));
        if let Some(runtime_directory) = cache_directory.parent() {
            candidates.push(runtime_directory.join(cached_path));
        }
    }
    candidates.into_iter().any(|candidate| {
        candidate
            .canonicalize()
            .is_ok_and(|actual| actual == expected)
    })
}

fn file_modified_at_ms(path: &Path) -> Result<u64> {
    let modified = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .modified()
        .with_context(|| format!("failed to read modified time for {}", path.display()))?;
    let duration = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

#[cfg(windows)]
fn unprotect_local_secret(secret: &[u8]) -> Result<Vec<u8>> {
    use std::ptr;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: secret
            .len()
            .try_into()
            .context("secret is too large for DPAPI")?,
        pbData: secret.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points to `secret` for the duration of the call; output is freed with LocalFree.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        anyhow::bail!("CryptUnprotectData failed");
    }
    let bytes = unsafe { protected_blob_to_vec_and_free(output) };
    Ok(bytes)
}

#[cfg(windows)]
unsafe fn protected_blob_to_vec_and_free(
    blob: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
) -> Vec<u8> {
    use windows_sys::Win32::Foundation::LocalFree;

    let bytes = if blob.pbData.is_null() || blob.cbData == 0 {
        Vec::new()
    } else {
        // SAFETY: DPAPI returned `pbData` with `cbData` bytes; caller frees it below.
        unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec()
    };
    if !blob.pbData.is_null() {
        // SAFETY: DPAPI buffers must be released with LocalFree.
        unsafe {
            LocalFree(blob.pbData.cast());
        }
    }
    bytes
}

#[cfg(all(target_os = "macos", not(test)))]
fn unprotect_local_secret(_secret: &[u8]) -> Result<Vec<u8>> {
    let account = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "trade_xyz_vault".to_string());
    let output = run_macos_security_with_timeout(
        &[
            "find-generic-password".to_string(),
            "-a".to_string(),
            account,
            "-s".to_string(),
            MACOS_VAULT_KEYCHAIN_SERVICE.to_string(),
            "-w".to_string(),
        ],
        std::time::Duration::from_secs(5),
    )?;
    anyhow::ensure!(
        output.status.success(),
        "failed to read Vault session password from macOS Keychain"
    );
    let mut password = output.stdout;
    while matches!(password.last(), Some(b'\n' | b'\r')) {
        password.pop();
    }
    anyhow::ensure!(
        !password.is_empty(),
        "macOS Keychain returned an empty Vault session password"
    );
    Ok(password)
}

#[cfg(all(target_os = "macos", not(test)))]
fn run_macos_security_with_timeout(
    args: &[String],
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    let mut child = std::process::Command::new("/usr/bin/security")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to start macOS security command")?;
    let started = std::time::Instant::now();
    loop {
        if child
            .try_wait()
            .context("failed to poll macOS security command")?
            .is_some()
        {
            return child
                .wait_with_output()
                .context("failed to collect macOS security command output");
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("timed out reading Vault session password from macOS Keychain");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(any(windows, all(target_os = "macos", not(test)))))]
fn unprotect_local_secret(_secret: &[u8]) -> Result<Vec<u8>> {
    anyhow::bail!("persistent Vault session cache is only supported on Windows and macOS")
}

pub fn load_transfer_secret(
    config: &AppConfig,
    account: &AccountConfig,
    password: Option<&str>,
) -> Result<TransferWalletSecret> {
    let raw_secret = if let Some(password) = password {
        let vault_path = PathBuf::from(&config.secrets.vault_path);
        let secret_id = transfer_secret_id(account);
        load_secret_by_id(&vault_path, password, &secret_id, Some(&account.account_id))?
    } else if config.secrets.allow_env_fallback && !account.transfer_wallet_env.trim().is_empty() {
        let private_key = std::env::var(&account.transfer_wallet_env).with_context(|| {
            format!(
                "environment variable {} is not set for account {}",
                account.transfer_wallet_env, account.account_id
            )
        })?;
        ApiWalletSecret {
            secret_id: transfer_secret_id(account),
            account_id: account.account_id.clone(),
            private_key: normalize_private_key(&private_key)?,
        }
    } else {
        anyhow::bail!(
            "account {} requires vault password for transfer_secret_id {}",
            account.account_id,
            transfer_secret_id(account)
        );
    };

    let signer_address = private_key_address(&raw_secret.private_key)?;
    anyhow::ensure!(
        signer_address.eq_ignore_ascii_case(account.address.trim()),
        "transfer signer {} does not match configured EVM account address {} for {}; API wallets cannot be used for USDC funding transfers",
        signer_address,
        account.address,
        account.account_id
    );
    Ok(TransferWalletSecret {
        secret_id: raw_secret.secret_id.clone(),
        account_id: raw_secret.account_id.clone(),
        private_key: raw_secret.private_key.clone(),
        signer_address,
    })
}

pub fn load_secret_by_id(
    path: &Path,
    password: &str,
    secret_id: &str,
    expected_account_id: Option<&str>,
) -> Result<ApiWalletSecret> {
    let plain = decrypt_vault_file(path, password)?;
    let entry = plain
        .entries
        .get(secret_id)
        .with_context(|| format!("secret_id {secret_id} not found in vault"))?;
    if let Some(expected_account_id) = expected_account_id {
        anyhow::ensure!(
            entry.account_id == expected_account_id,
            "secret_id {} belongs to account {}, not {}",
            secret_id,
            entry.account_id,
            expected_account_id
        );
    }
    Ok(ApiWalletSecret {
        secret_id: secret_id.to_string(),
        account_id: entry.account_id.clone(),
        private_key: entry.api_wallet_private_key.clone(),
    })
}

pub fn account_secret_id(account: &AccountConfig) -> String {
    if account.secret_id.trim().is_empty() {
        account.account_id.clone()
    } else {
        account.secret_id.clone()
    }
}

pub fn transfer_secret_id(account: &AccountConfig) -> String {
    if account.transfer_secret_id.trim().is_empty() {
        account_secret_id(account)
    } else {
        account.transfer_secret_id.clone()
    }
}

pub fn private_key_address(private_key: &str) -> Result<String> {
    use ethers::signers::{LocalWallet, Signer};

    let wallet: LocalWallet = normalize_private_key(private_key)?
        .parse()
        .context("failed to parse private key for address derivation")?;
    Ok(format!("{:#x}", wallet.address()))
}

pub fn account_has_dedicated_transfer_secret(account: &AccountConfig) -> bool {
    !account.transfer_secret_id.trim().is_empty() || !account.transfer_wallet_env.trim().is_empty()
}

fn write_encrypted_vault(
    path: &Path,
    password: &str,
    plain: &PlainVault,
    kdf: VaultKdf,
) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("vault path {} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create vault directory {}", parent.display()))?;

    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);

    let mut key = derive_key(password, &salt, &kdf)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = zeroize::Zeroizing::new(serde_json::to_vec(plain).context("failed to serialize vault plaintext")?);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: &plaintext,
                aad: VAULT_AAD,
            },
        )
        .map_err(|_| anyhow!("failed to encrypt vault"))?;
    key.zeroize();

    let vault_file = VaultFile {
        version: 1,
        algorithm: "xchacha20poly1305".to_string(),
        kdf,
        salt_b64: STANDARD_NO_PAD.encode(salt),
        nonce_b64: STANDARD_NO_PAD.encode(nonce),
        ciphertext_b64: STANDARD_NO_PAD.encode(ciphertext),
    };

    crate::portable::atomic_json(path, &vault_file)
        .with_context(|| format!("failed to write vault {}", path.display()))?;
    Ok(())
}

fn decrypt_vault_file(path: &Path, password: &str) -> Result<PlainVault> {
    validate_password(password)?;
    let raw = fs::read(path).with_context(|| format!("failed to read vault {}", path.display()))?;
    let vault_file =
        serde_json::from_slice::<VaultFile>(&raw).context("failed to parse vault file")?;
    anyhow::ensure!(vault_file.version == 1, "unsupported vault version");
    anyhow::ensure!(
        vault_file.algorithm == "xchacha20poly1305",
        "unsupported vault algorithm"
    );
    anyhow::ensure!(vault_file.kdf.name == "argon2id", "unsupported vault kdf");

    let salt = decode_fixed::<SALT_LEN>(&vault_file.salt_b64, "salt")?;
    let nonce = decode_fixed::<NONCE_LEN>(&vault_file.nonce_b64, "nonce")?;
    let ciphertext = STANDARD_NO_PAD
        .decode(vault_file.ciphertext_b64)
        .context("failed to decode vault ciphertext")?;
    let mut key = derive_key(password, &salt, &vault_file.kdf)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            chacha20poly1305::aead::Payload {
                msg: &ciphertext,
                aad: VAULT_AAD,
            },
        )
        .map_err(|_| {
            anyhow!("failed to decrypt vault; password may be wrong or file may be damaged")
        })?;
    key.zeroize();

    let plain = serde_json::from_slice::<PlainVault>(&plaintext)
        .context("failed to parse vault plaintext")?;
    Ok(plain)
}

fn derive_key(password: &str, salt: &[u8], kdf: &VaultKdf) -> Result<[u8; KEY_LEN]> {
    let params = Params::new(
        kdf.memory_kib,
        kdf.iterations,
        kdf.parallelism,
        Some(KEY_LEN),
    )
    .map_err(|error| anyhow!("invalid argon2 parameters: {error:?}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|error| anyhow!("failed to derive vault key: {error:?}"))?;
    Ok(key)
}

fn decode_fixed<const N: usize>(encoded: &str, label: &str) -> Result<[u8; N]> {
    let bytes = STANDARD_NO_PAD
        .decode(encoded)
        .with_context(|| format!("failed to decode vault {label}"))?;
    let fixed: [u8; N] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("vault {label} has wrong length"))?;
    Ok(fixed)
}

fn summary_from_plain(path: &Path, plain: &PlainVault) -> VaultSummary {
    VaultSummary {
        exists: path.exists(),
        unlocked: true,
        path: path.display().to_string(),
        entry_count: Some(plain.entries.len()),
        entries: plain
            .entries
            .values()
            .map(|entry| VaultEntrySummary {
                secret_id: entry.secret_id.clone(),
                account_id: entry.account_id.clone(),
                address: entry.address.clone(),
                has_api_wallet_key: !entry.api_wallet_private_key.is_empty(),
                has_lighter_api_key: entry.lighter_api_private_key.is_some(),
                lighter_account_index: entry.lighter_account_index,
                lighter_api_key_index: entry.lighter_api_key_index,
                updated_at_ms: entry.updated_at_ms,
            })
            .collect(),
    }
}

fn validate_password(password: &str) -> Result<()> {
    anyhow::ensure!(
        password.chars().count() >= 10,
        "vault password must be at least 10 characters"
    );
    Ok(())
}

fn validate_secret_input(input: &SecretUpsert) -> Result<()> {
    anyhow::ensure!(
        !input.secret_id.trim().is_empty(),
        "secret_id cannot be empty"
    );
    anyhow::ensure!(
        !input.account_id.trim().is_empty(),
        "account_id cannot be empty"
    );
    let address = input.address.trim();
    anyhow::ensure!(
        address.len() == 42
            && address.starts_with("0x")
            && address[2..]
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_hexdigit()),
        "EVM address must be 0x plus 40 hex digits"
    );
    normalize_private_key(&input.api_wallet_private_key)?;
    Ok(())
}

fn validate_lighter_secret_input(input: &LighterSecretUpsert) -> Result<()> {
    anyhow::ensure!(
        !input.secret_id.trim().is_empty(),
        "secret_id cannot be empty"
    );
    anyhow::ensure!(
        !input.account_id.trim().is_empty(),
        "account_id cannot be empty"
    );
    anyhow::ensure!(
        input.account_index >= 0,
        "Lighter account_index must be non-negative"
    );
    anyhow::ensure!(
        input.api_key_index <= 254,
        "Lighter api_key_index must be between 0 and 254"
    );
    let address = input.l1_address.trim();
    anyhow::ensure!(
        address.len() == 42
            && address.starts_with("0x")
            && address[2..]
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Lighter L1 address must be 0x plus 40 hex digits"
    );
    normalize_lighter_private_key(&input.api_private_key)?;
    Ok(())
}

fn normalize_private_key(private_key: &str) -> Result<String> {
    let trimmed = private_key.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    anyhow::ensure!(hex.len() == 64, "private key must be 32 bytes hex");
    anyhow::ensure!(
        hex.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit()),
        "private key contains non-hex characters"
    );
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

fn normalize_lighter_private_key(private_key: &str) -> Result<String> {
    let trimmed = private_key.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    anyhow::ensure!(
        hex.len() == 80,
        "Lighter API private key must be 40 bytes hex"
    );
    anyhow::ensure!(
        hex.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit()),
        "Lighter API private key contains non-hex characters"
    );
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::config::{AccountConfig, AppConfig};

    use super::{
        LighterSecretUpsert, PlainVault, SecretUpsert, VaultKdf, cached_vault_path_matches,
        change_vault_password, decrypt_vault_file, load_account_secret, load_lighter_secret_by_id,
        load_secret_by_id, load_transfer_secret, private_key_address, summary_from_plain,
        upsert_lighter_secret, upsert_secret, upsert_secrets, write_encrypted_vault,
    };

    #[test]
    fn frontend_relative_cached_vault_path_matches_same_absolute_file() {
        let runtime = std::env::temp_dir().join(format!(
            "trade_xyz_vault_path_match_test_{}",
            crate::domain::now_ms()
        ));
        let cache_directory = runtime.join(".codex-longrun");
        let vault = runtime.join("secrets/trade_xyz.vault");
        fs::create_dir_all(&cache_directory).unwrap();
        fs::create_dir_all(vault.parent().unwrap()).unwrap();
        fs::write(&vault, b"test").unwrap();

        assert!(cached_vault_path_matches(
            &cache_directory.join("vault-session-cache.json"),
            std::path::Path::new("secrets/trade_xyz.vault"),
            &vault,
        ));
        assert!(!cached_vault_path_matches(
            &cache_directory.join("vault-session-cache.json"),
            std::path::Path::new("secrets/other.vault"),
            &vault,
        ));

        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn lighter_secret_round_trip_is_separate_and_encrypted() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_lighter_vault_test_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "lighter vault password";
        let private_key =
            "0b8e0f63c24d8baacd9d29ad4e9a4b73c4a8d2bb8b16dc4fa9d7c2e1d3a8b1f0e8d3a4c5b6e7f001";
        upsert_lighter_secret(
            &path,
            password,
            LighterSecretUpsert {
                secret_id: "lighter-a".to_string(),
                account_id: "local-a".to_string(),
                l1_address: "0x0000000000000000000000000000000000000009".to_string(),
                account_index: 123,
                api_key_index: 7,
                api_private_key: private_key.to_string(),
            },
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(private_key));
        let secret =
            load_lighter_secret_by_id(&path, password, "lighter-a", Some("local-a")).unwrap();
        assert_eq!(secret.account_index, 123);
        assert_eq!(secret.api_key_index, 7);
        assert_eq!(secret.private_key, format!("0x{private_key}"));
        assert!(!format!("{secret:?}").contains(private_key));
        assert!(load_secret_by_id(&path, password, "lighter-a", Some("other")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lighter_secret_id_cannot_reuse_trade_key_entry() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_lighter_secret_id_isolation_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "isolated key modules";
        upsert_secret(
            &path,
            password,
            SecretUpsert {
                secret_id: "shared-id".to_string(),
                account_id: "same-account".to_string(),
                address: "0x0000000000000000000000000000000000000009".to_string(),
                api_wallet_private_key:
                    "0x0000000000000000000000000000000000000000000000000000000000000003"
                        .to_string(),
            },
        )
        .expect("trade key upsert");

        let error = upsert_lighter_secret(
            &path,
            password,
            LighterSecretUpsert {
                secret_id: "shared-id".to_string(),
                account_id: "same-account".to_string(),
                l1_address: "0x0000000000000000000000000000000000000009".to_string(),
                account_index: 123,
                api_key_index: 7,
                api_private_key:
                    "0b8e0f63c24d8baacd9d29ad4e9a4b73c4a8d2bb8b16dc4fa9d7c2e1d3a8b1f0e8d3a4c5b6e7f001"
                        .to_string(),
            },
        )
        .expect_err("shared secret id must be rejected");
        assert!(error.to_string().contains("already used by a Trade key"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upsert_and_unlock_vault_without_plaintext_leak() {
        let dir =
            std::env::temp_dir().join(format!("trade_xyz_vault_test_{}", crate::domain::now_ms()));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "correct horse battery";
        let private_key = "0x0000000000000000000000000000000000000000000000000000000000000003";

        let summary = upsert_secret(
            &path,
            password,
            SecretUpsert {
                secret_id: "addr_a_api_wallet".to_string(),
                account_id: "addr_a".to_string(),
                address: "0x0000000000000000000000000000000000000009".to_string(),
                api_wallet_private_key: private_key.to_string(),
            },
        )
        .expect("vault upsert");

        assert!(summary.exists);
        assert_eq!(summary.entry_count, Some(1));
        let raw = fs::read_to_string(&path).expect("vault file");
        assert!(!raw.contains(private_key));

        let unlocked = decrypt_vault_file(&path, password).expect("unlock vault");
        let summary = summary_from_plain(&path, &unlocked);
        assert_eq!(summary.entries[0].secret_id, "addr_a_api_wallet");
        assert!(decrypt_vault_file(&path, "wrong password").is_err());
    }

    #[test]
    fn batch_upsert_is_all_or_nothing_and_never_writes_plaintext() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_vault_batch_test_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "correct horse battery";
        let key_a = format!("0x{}", "1".repeat(64));
        let key_b = format!("0x{}", "2".repeat(64));
        let rows = vec![
            SecretUpsert {
                secret_id: "o11_api_wallet".into(),
                account_id: "o11".into(),
                address: format!("0x{}", "a".repeat(40)),
                api_wallet_private_key: key_a.clone(),
            },
            SecretUpsert {
                secret_id: "o12_api_wallet".into(),
                account_id: "o12".into(),
                address: format!("0x{}", "b".repeat(40)),
                api_wallet_private_key: key_b.clone(),
            },
        ];
        let summary = upsert_secrets(&path, password, rows).expect("batch import");
        assert_eq!(summary.entry_count, Some(2));
        let encrypted = fs::read_to_string(&path).expect("encrypted vault");
        assert!(!encrypted.contains(&key_a));
        assert!(!encrypted.contains(&key_b));
        let before = fs::read(&path).unwrap();
        let invalid = vec![
            SecretUpsert {
                secret_id: "o13_api_wallet".into(),
                account_id: "o13".into(),
                address: format!("0x{}", "c".repeat(40)),
                api_wallet_private_key: format!("0x{}", "3".repeat(64)),
            },
            SecretUpsert {
                secret_id: "o14_api_wallet".into(),
                account_id: "o14".into(),
                address: "not-an-address".into(),
                api_wallet_private_key: format!("0x{}", "4".repeat(64)),
            },
        ];
        assert!(upsert_secrets(&path, password, invalid).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn low_cost_kdf_round_trip_for_format_stability() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_vault_kdf_test_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let plain = PlainVault {
            version: 1,
            updated_at_ms: crate::domain::now_ms(),
            entries: Default::default(),
        };

        write_encrypted_vault(
            &path,
            "format password",
            &plain,
            VaultKdf {
                name: "argon2id".to_string(),
                memory_kib: 1024,
                iterations: 1,
                parallelism: 1,
            },
        )
        .expect("write low cost vault");

        let unlocked = decrypt_vault_file(&path, "format password").expect("unlock");
        assert_eq!(unlocked.version, 1);
    }

    #[test]
    fn load_account_secret_uses_configured_secret_id() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_vault_lookup_test_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "lookup password";
        let private_key = "0x0000000000000000000000000000000000000000000000000000000000000004";

        upsert_secret(
            &path,
            password,
            SecretUpsert {
                secret_id: "addr_a_api_wallet".to_string(),
                account_id: "addr_a".to_string(),
                address: "0x0000000000000000000000000000000000000009".to_string(),
                api_wallet_private_key: private_key.to_string(),
            },
        )
        .expect("vault upsert");

        let mut config = AppConfig::default();
        config.secrets.vault_path = path.to_string_lossy().into_owned();
        let account = AccountConfig {
            account_id: "addr_a".to_string(),
            address: "0x0000000000000000000000000000000000000009".to_string(),
            secret_id: "addr_a_api_wallet".to_string(),
            api_wallet_env: String::new(),
            transfer_secret_id: String::new(),
            transfer_wallet_env: String::new(),
            enabled: true,
            worker_enabled: true,
            copy_ratio: 0.1,
            max_order_notional_usd: 100.0,
            blocked_markets: Vec::new(),
        };

        let secret =
            load_account_secret(&config, &account, Some(password)).expect("load account secret");
        assert_eq!(secret.secret_id, "addr_a_api_wallet");
        assert_eq!(secret.account_id, "addr_a");
        assert_eq!(secret.private_key, private_key);
    }

    #[test]
    fn transfer_secret_requires_evm_signer_matching_account_address() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_transfer_secret_test_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "transfer lookup password";
        let api_private_key = "0x0000000000000000000000000000000000000000000000000000000000000004";
        let evm_private_key = "0x0000000000000000000000000000000000000000000000000000000000000002";
        let evm_address = private_key_address(evm_private_key).expect("derive evm address");

        upsert_secret(
            &path,
            password,
            SecretUpsert {
                secret_id: "addr_a_api_wallet".to_string(),
                account_id: "addr_a".to_string(),
                address: evm_address.clone(),
                api_wallet_private_key: api_private_key.to_string(),
            },
        )
        .expect("api vault upsert");
        upsert_secret(
            &path,
            password,
            SecretUpsert {
                secret_id: "addr_a_transfer_wallet".to_string(),
                account_id: "addr_a".to_string(),
                address: evm_address.clone(),
                api_wallet_private_key: evm_private_key.to_string(),
            },
        )
        .expect("transfer vault upsert");

        let mut config = AppConfig::default();
        config.secrets.vault_path = path.to_string_lossy().into_owned();
        let mut account = AccountConfig {
            account_id: "addr_a".to_string(),
            address: evm_address.clone(),
            secret_id: "addr_a_api_wallet".to_string(),
            api_wallet_env: String::new(),
            transfer_secret_id: String::new(),
            transfer_wallet_env: String::new(),
            enabled: true,
            worker_enabled: true,
            copy_ratio: 0.1,
            max_order_notional_usd: 100.0,
            blocked_markets: Vec::new(),
        };

        let legacy_error = load_transfer_secret(&config, &account, Some(password))
            .expect_err("api wallet fallback must not pass transfer signer check")
            .to_string();
        assert!(legacy_error.contains("API wallets cannot be used"));

        account.transfer_secret_id = "addr_a_transfer_wallet".to_string();
        let secret =
            load_transfer_secret(&config, &account, Some(password)).expect("load transfer secret");
        assert_eq!(secret.secret_id, "addr_a_transfer_wallet");
        assert_eq!(secret.signer_address, evm_address.to_ascii_lowercase());
    }

    #[test]
    fn load_secret_by_id_supports_vault_only_accounts() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_vault_custom_lookup_test_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let password = "custom lookup password";
        let private_key = "0x0000000000000000000000000000000000000000000000000000000000000002";

        upsert_secret(
            &path,
            password,
            SecretUpsert {
                secret_id: "addr_c_api_wallet".to_string(),
                account_id: "addr_c".to_string(),
                address: "0x0000000000000000000000000000000000000022".to_string(),
                api_wallet_private_key: private_key.to_string(),
            },
        )
        .expect("vault upsert");

        let secret = load_secret_by_id(&path, password, "addr_c_api_wallet", Some("addr_c"))
            .expect("load custom secret");
        assert_eq!(secret.secret_id, "addr_c_api_wallet");
        assert_eq!(secret.account_id, "addr_c");
        assert_eq!(secret.private_key, private_key);
        assert!(load_secret_by_id(&path, password, "addr_c_api_wallet", Some("addr_a")).is_err());
    }

    #[test]
    fn change_vault_password_preserves_entries_and_rotates_key() {
        let dir = std::env::temp_dir().join(format!(
            "trade_xyz_vault_password_change_test_{}",
            crate::domain::now_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join("trade_xyz.vault");
        let old_password = "old password value";
        let new_password = "new password value";
        let private_key = "0x0000000000000000000000000000000000000000000000000000000000000005";

        upsert_secret(
            &path,
            old_password,
            SecretUpsert {
                secret_id: "addr_d_api_wallet".to_string(),
                account_id: "addr_d".to_string(),
                address: "0x0000000000000000000000000000000000000027".to_string(),
                api_wallet_private_key: private_key.to_string(),
            },
        )
        .expect("vault upsert");

        let summary =
            change_vault_password(&path, old_password, new_password).expect("change password");
        assert_eq!(summary.entry_count, Some(1));
        assert!(decrypt_vault_file(&path, old_password).is_err());

        let secret = load_secret_by_id(&path, new_password, "addr_d_api_wallet", Some("addr_d"))
            .expect("load with new password");
        assert_eq!(secret.private_key, private_key);
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;
    #[test]
    fn rename_preserves_material_and_rejects_collisions() {
        let path =
            std::env::temp_dir().join(format!("vault-rename-{}.vault", uuid::Uuid::new_v4()));
        let password = "test-only-password";
        let input = SecretUpsert {
            secret_id: "co5_api_wallet".into(),
            account_id: "co5".into(),
            address: format!("0x{}", "1".repeat(40)),
            api_wallet_private_key: format!("0x{}", "1".repeat(64)),
        };
        upsert_secret(&path, password, input).unwrap();
        let old = load_secret_by_id(&path, password, "co5_api_wallet", Some("co5")).unwrap();
        rename_vault_accounts(&path, password, &[("co5".into(), "o5".into())]).unwrap();
        let new = load_secret_by_id(&path, password, "o5_api_wallet", Some("o5")).unwrap();
        assert!(old.private_key == new.private_key);
        assert_eq!(
            unlock_vault(&path, password).unwrap().entries[0].address,
            format!("0x{}", "1".repeat(40))
        );
        assert!(load_secret_by_id(&path, password, "co5_api_wallet", None).is_err());
        assert!(
            rename_vault_accounts(&path, password, &[("missing".into(), "o5".into())]).is_err()
        );
        std::fs::remove_file(path).unwrap();
    }
}
