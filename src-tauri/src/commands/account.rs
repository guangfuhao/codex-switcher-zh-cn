//! Account management Tauri commands

use crate::auth::{
    add_account, auth_matches_account, create_chatgpt_account_from_refresh_token,
    ensure_chatgpt_tokens_fresh_locked, import_from_auth_json, import_from_auth_json_contents,
    load_accounts, read_current_auth, remove_account, save_accounts, set_active_account,
    switch_to_account, sync_matching_account_tokens, touch_account, AUTH_OPERATION_LOCK,
};
use crate::types::{
    AccountInfo, AccountsStore, AuthData, AuthDotJson, ImportAccountsSummary, StoredAccount,
};

use super::process::ensure_codex_not_running;

use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use futures::{stream, StreamExt};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::Sha256;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};

const SLIM_EXPORT_PREFIX: &str = "css1.";
const SLIM_FORMAT_VERSION: u8 = 1;
const SLIM_AUTH_API_KEY: u8 = 0;
const SLIM_AUTH_CHATGPT: u8 = 1;

const FULL_FILE_MAGIC: &[u8; 4] = b"CSWF";
const FULL_FILE_VERSION: u8 = 1;
const FULL_SALT_LEN: usize = 16;
const FULL_NONCE_LEN: usize = 24;
const FULL_KDF_ITERATIONS: u32 = 210_000;
const FULL_PRESET_PASSPHRASE: &str = "gT7kQ9mV2xN4pL8sR1dH6zW3cB5yF0uJ_aE7nK2tP9vM4rX1";

const MAX_IMPORT_JSON_BYTES: u64 = 2 * 1024 * 1024;
const MAX_IMPORT_FILE_BYTES: u64 = 8 * 1024 * 1024;
const SLIM_IMPORT_CONCURRENCY: usize = 6;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SlimPayload {
    #[serde(rename = "v")]
    version: u8,
    #[serde(rename = "a", skip_serializing_if = "Option::is_none")]
    active_name: Option<String>,
    #[serde(rename = "c")]
    accounts: Vec<SlimAccountPayload>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SlimAccountPayload {
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "t")]
    auth_type: u8,
    #[serde(rename = "k", skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(rename = "r", skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

// Display the actual official login, never a stale selection from another session.
pub(crate) fn load_accounts_for_display() -> anyhow::Result<AccountsStore> {
    let mut store = load_accounts()?;
    select_live_account(&mut store, read_current_auth().ok().flatten().as_ref());
    Ok(store)
}

fn select_live_account(store: &mut AccountsStore, auth: Option<&AuthDotJson>) {
    store.active_account_id = auth.and_then(|auth| {
        store
            .accounts
            .iter()
            .find(|account| auth_matches_account(account, auth))
            .map(|account| account.id.clone())
    });
}

/// List all accounts with their info
#[tauri::command]
pub async fn list_accounts() -> Result<Vec<AccountInfo>, String> {
    let store = load_accounts_for_display().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    let accounts: Vec<AccountInfo> = store
        .accounts
        .iter()
        .map(|a| {
            let mut info = AccountInfo::from_stored(a, active_id);
            super::usage::apply_cached_account_metadata(&mut info);
            info
        })
        .collect();

    Ok(accounts)
}

/// Get the currently active account
#[tauri::command]
pub async fn get_active_account_info() -> Result<Option<AccountInfo>, String> {
    let store = load_accounts_for_display().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    if let Some(active) = store
        .accounts
        .iter()
        .find(|account| Some(account.id.as_str()) == active_id)
    {
        let mut info = AccountInfo::from_stored(&active, active_id);
        super::usage::apply_cached_account_metadata(&mut info);
        Ok(Some(info))
    } else {
        Ok(None)
    }
}

/// Add an account from an auth.json file
#[tauri::command]
pub async fn add_account_from_file(path: String, name: String) -> Result<AccountInfo, String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    // Import from the file
    let account = import_from_auth_json(&path, name).map_err(|e| e.to_string())?;

    // Add to storage
    let stored = add_account(account).map_err(|e| e.to_string())?;

    let store = load_accounts().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    Ok(AccountInfo::from_stored(&stored, active_id))
}

/// Add an account from uploaded auth.json contents.
pub async fn add_account_from_auth_json_text(
    name: String,
    contents: String,
) -> Result<AccountInfo, String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    let account = import_from_auth_json_contents(&contents, name).map_err(|e| e.to_string())?;
    let stored = add_account(account).map_err(|e| e.to_string())?;

    let store = load_accounts().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    Ok(AccountInfo::from_stored(&stored, active_id))
}

/// Switch to a different account
#[tauri::command]
pub async fn switch_account(account_id: String) -> Result<(), String> {
    switch_account_by_id(&account_id).await
}

pub async fn switch_account_by_id(account_id: &str) -> Result<(), String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    let mut store = load_accounts().map_err(|e| e.to_string())?;

    let target_index = store
        .accounts
        .iter()
        .position(|account| account.id == account_id)
        .ok_or_else(|| format!("找不到账号：{account_id}"))?;

    // The on-disk login is authoritative. Importing the currently signed-in
    // account must preserve rotated tokens even before Switcher marks it active.
    if let Some(auth) = read_current_auth().map_err(|e| e.to_string())? {
        if sync_matching_account_tokens(&mut store, &auth) {
            save_accounts(&store).map_err(|e| e.to_string())?;
        }
        if auth_matches_account(&store.accounts[target_index], &auth) {
            set_active_account(account_id).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }

    ensure_codex_not_running()?;

    let account = ensure_chatgpt_tokens_fresh_locked(&store.accounts[target_index])
        .await
        .map_err(|e| e.to_string())?;

    // Refresh can take time; a desktop/CLI could have started in the meantime.
    ensure_codex_not_running()?;
    // Preserve any final rotation before replacing the current login.
    if let Some(auth) = read_current_auth().map_err(|e| e.to_string())? {
        let mut latest = load_accounts().map_err(|e| e.to_string())?;
        if sync_matching_account_tokens(&mut latest, &auth) {
            save_accounts(&latest).map_err(|e| e.to_string())?;
        }
    }

    // Write to ~/.codex/auth.json only after all consumers have exited.
    switch_to_account(&account).map_err(|e| e.to_string())?;

    // Update the active account in our store
    set_active_account(account_id).map_err(|e| e.to_string())?;

    // Update last_used_at
    touch_account(account_id).map_err(|e| e.to_string())?;

    Ok(())
}

/// Remove an account
#[tauri::command]
pub async fn delete_account(account_id: String) -> Result<(), String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    remove_account(&account_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Rename an account
#[tauri::command]
pub async fn rename_account(account_id: String, new_name: String) -> Result<(), String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    crate::auth::storage::update_account_metadata(&account_id, Some(new_name), None, None, None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Export minimal account config as a compact text string.
/// For ChatGPT accounts, only refresh token is exported.
#[tauri::command]
pub async fn export_accounts_slim_text() -> Result<String, String> {
    let store = load_accounts().map_err(|e| e.to_string())?;
    encode_slim_payload_from_store(&store).map_err(|e| e.to_string())
}

/// Import minimal account config from a compact text string, skipping existing accounts.
#[tauri::command]
pub async fn import_accounts_slim_text(payload: String) -> Result<ImportAccountsSummary, String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    let slim_payload = decode_slim_payload(&payload).map_err(|e| format!("{e:#}"))?;
    let total_in_payload = slim_payload.accounts.len();

    let current = load_accounts().map_err(|e| e.to_string())?;
    let existing_names: HashSet<String> = current.accounts.iter().map(|a| a.name.clone()).collect();

    let imported = build_store_from_slim_payload(slim_payload, &existing_names)
        .await
        .map_err(|e| {
            format!(
                "{e:#}\n提示：精简导入需要联网刷新 ChatGPT 登录凭据；离线时可导入完整加密备份文件。"
            )
        })?;
    validate_imported_store(&imported).map_err(|e| format!("{e:#}"))?;

    let (merged, summary) = merge_accounts_store(current, imported);
    save_accounts(&merged).map_err(|e| e.to_string())?;
    Ok(ImportAccountsSummary {
        total_in_payload,
        imported_count: summary.imported_count,
        skipped_count: total_in_payload.saturating_sub(summary.imported_count),
    })
}

/// Export full account config as an encrypted file.
#[tauri::command]
pub async fn export_accounts_full_encrypted_file(path: String) -> Result<(), String> {
    let store = load_accounts().map_err(|e| e.to_string())?;
    let encrypted =
        encode_full_encrypted_store(&store, FULL_PRESET_PASSPHRASE).map_err(|e| e.to_string())?;
    write_encrypted_file(&path, &encrypted).map_err(|e| e.to_string())?;
    Ok(())
}

/// Export full account config as encrypted bytes for browser clients.
pub async fn export_accounts_full_encrypted_bytes() -> Result<Vec<u8>, String> {
    let store = load_accounts().map_err(|e| e.to_string())?;
    encode_full_encrypted_store(&store, FULL_PRESET_PASSPHRASE).map_err(|e| e.to_string())
}

/// Import full account config from an encrypted file, skipping existing accounts.
#[tauri::command]
pub async fn import_accounts_full_encrypted_file(
    path: String,
) -> Result<ImportAccountsSummary, String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    let encrypted = read_encrypted_file(&path).map_err(|e| e.to_string())?;
    let imported = decode_full_encrypted_store(&encrypted, FULL_PRESET_PASSPHRASE)
        .map_err(|e| e.to_string())?;
    validate_imported_store(&imported).map_err(|e| e.to_string())?;

    let current = load_accounts().map_err(|e| e.to_string())?;
    let (merged, summary) = merge_accounts_store(current, imported);
    save_accounts(&merged).map_err(|e| e.to_string())?;
    Ok(summary)
}

/// Import full account config from encrypted bytes uploaded through the browser UI.
pub async fn import_accounts_full_encrypted_bytes(
    bytes: Vec<u8>,
) -> Result<ImportAccountsSummary, String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    let imported =
        decode_full_encrypted_store(&bytes, FULL_PRESET_PASSPHRASE).map_err(|e| e.to_string())?;
    validate_imported_store(&imported).map_err(|e| e.to_string())?;

    let current = load_accounts().map_err(|e| e.to_string())?;
    let (merged, summary) = merge_accounts_store(current, imported);
    save_accounts(&merged).map_err(|e| e.to_string())?;
    Ok(summary)
}

fn encode_slim_payload_from_store(store: &AccountsStore) -> anyhow::Result<String> {
    let active_name = store.active_account_id.as_ref().and_then(|active_id| {
        store
            .accounts
            .iter()
            .find(|account| account.id == *active_id)
            .map(|account| account.name.clone())
    });

    let slim_accounts = store
        .accounts
        .iter()
        .map(|account| match &account.auth_data {
            AuthData::ApiKey { key } => SlimAccountPayload {
                name: account.name.clone(),
                auth_type: SLIM_AUTH_API_KEY,
                api_key: Some(key.clone()),
                refresh_token: None,
            },
            AuthData::ChatGPT { refresh_token, .. } => SlimAccountPayload {
                name: account.name.clone(),
                auth_type: SLIM_AUTH_CHATGPT,
                api_key: None,
                refresh_token: Some(refresh_token.clone()),
            },
        })
        .collect();

    let payload = SlimPayload {
        version: SLIM_FORMAT_VERSION,
        active_name,
        accounts: slim_accounts,
    };

    let json = serde_json::to_vec(&payload).context("生成精简账号备份失败")?;
    let compressed = compress_bytes(&json).context("压缩精简账号备份失败")?;

    Ok(format!(
        "{SLIM_EXPORT_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(compressed)
    ))
}

fn decode_slim_payload(payload: &str) -> anyhow::Result<SlimPayload> {
    let normalized: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    if normalized.is_empty() {
        anyhow::bail!("导入内容为空");
    }

    let encoded = normalized
        .strip_prefix(SLIM_EXPORT_PREFIX)
        .unwrap_or(&normalized);

    let compressed = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("精简备份格式无效（Base64 解码失败）")?;

    let decompressed = decompress_bytes_with_limit(&compressed, MAX_IMPORT_JSON_BYTES)
        .context("精简备份格式无效（解压失败）")?;

    let parsed: SlimPayload =
        serde_json::from_slice(&decompressed).context("精简备份格式无效（JSON 解析失败）")?;

    validate_slim_payload(&parsed)?;
    Ok(parsed)
}

fn validate_slim_payload(payload: &SlimPayload) -> anyhow::Result<()> {
    if payload.version != SLIM_FORMAT_VERSION {
        anyhow::bail!("不支持此精简备份版本：{}", payload.version);
    }

    let mut names = HashSet::new();

    for account in &payload.accounts {
        if account.name.trim().is_empty() {
            anyhow::bail!("精简备份中有账号名称为空");
        }

        if !names.insert(account.name.clone()) {
            anyhow::bail!("精简备份中存在重复账号名称：{}", account.name);
        }

        match account.auth_type {
            SLIM_AUTH_API_KEY => {
                if account
                    .api_key
                    .as_ref()
                    .map_or(true, |key| key.trim().is_empty())
                {
                    anyhow::bail!("账号 {} 缺少 API 密钥", account.name);
                }
            }
            SLIM_AUTH_CHATGPT => {
                if account
                    .refresh_token
                    .as_ref()
                    .map_or(true, |token| token.trim().is_empty())
                {
                    anyhow::bail!("账号 {} 缺少刷新凭据", account.name);
                }
            }
            _ => {
                anyhow::bail!(
                    "不支持认证类型 {}（账号：{}）",
                    account.auth_type,
                    account.name
                );
            }
        }
    }

    if let Some(active_name) = &payload.active_name {
        if !names.contains(active_name) {
            anyhow::bail!("精简备份引用了不存在的当前账号：{active_name}");
        }
    }

    Ok(())
}

async fn build_store_from_slim_payload(
    payload: SlimPayload,
    existing_names: &HashSet<String>,
) -> anyhow::Result<AccountsStore> {
    let active_name = payload.active_name;
    let import_candidates: Vec<SlimAccountPayload> = payload
        .accounts
        .into_iter()
        .filter(|entry| !existing_names.contains(&entry.name))
        .collect();

    let accounts = restore_slim_accounts(import_candidates).await?;
    let mut active_account_id = None;

    if let Some(active) = active_name {
        active_account_id = accounts
            .iter()
            .find(|account| account.name == active)
            .map(|account| account.id.clone());
    }

    if active_account_id.is_none() {
        active_account_id = accounts.first().map(|a| a.id.clone());
    }

    Ok(AccountsStore {
        version: 1,
        accounts,
        active_account_id,
        masked_account_ids: Vec::new(),
    })
}

async fn restore_slim_accounts(
    entries: Vec<SlimAccountPayload>,
) -> anyhow::Result<Vec<StoredAccount>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let mut restored = Vec::with_capacity(entries.len());
    let mut tasks = stream::iter(entries.into_iter().map(|entry| async move {
        let account_name = entry.name;
        let account = match entry.auth_type {
            SLIM_AUTH_API_KEY => StoredAccount::new_api_key(
                account_name.clone(),
                entry.api_key.context("导入数据缺少 API 密钥")?,
            ),
            SLIM_AUTH_CHATGPT => {
                let refresh_token = entry.refresh_token.context("导入数据缺少刷新凭据")?;
                create_chatgpt_account_from_refresh_token(account_name.clone(), refresh_token)
                    .await
                    .with_context(|| format!("无法通过刷新凭据恢复 ChatGPT 账号“{account_name}”"))?
            }
            _ => anyhow::bail!("精简备份中的认证类型不受支持"),
        };
        Ok::<StoredAccount, anyhow::Error>(account)
    }))
    .buffered(SLIM_IMPORT_CONCURRENCY);

    while let Some(account_result) = tasks.next().await {
        restored.push(account_result?);
    }

    Ok(restored)
}

fn encode_full_encrypted_store(store: &AccountsStore, passphrase: &str) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(store).context("生成账号备份数据失败")?;
    let compressed = compress_bytes(&json).context("压缩账号备份失败")?;

    let mut salt = [0u8; FULL_SALT_LEN];
    rand::rng().fill_bytes(&mut salt);

    let mut nonce = [0u8; FULL_NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);

    let key = derive_encryption_key(passphrase, &salt);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), compressed.as_slice())
        .map_err(|_| anyhow::anyhow!("加密账号备份失败"))?;

    let mut out = Vec::with_capacity(4 + 1 + FULL_SALT_LEN + FULL_NONCE_LEN + ciphertext.len());
    out.extend_from_slice(FULL_FILE_MAGIC);
    out.push(FULL_FILE_VERSION);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);

    Ok(out)
}

fn decode_full_encrypted_store(
    file_bytes: &[u8],
    passphrase: &str,
) -> anyhow::Result<AccountsStore> {
    if file_bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        anyhow::bail!("加密备份文件过大");
    }

    let header_len = 4 + 1 + FULL_SALT_LEN + FULL_NONCE_LEN;
    if file_bytes.len() <= header_len {
        anyhow::bail!("加密备份文件无效或不完整");
    }

    if &file_bytes[..4] != FULL_FILE_MAGIC {
        anyhow::bail!("加密备份文件头无效");
    }

    let version = file_bytes[4];
    if version != FULL_FILE_VERSION {
        anyhow::bail!("不支持此加密备份版本：{version}");
    }

    let salt_start = 5;
    let nonce_start = salt_start + FULL_SALT_LEN;
    let ciphertext_start = nonce_start + FULL_NONCE_LEN;

    let salt = &file_bytes[salt_start..nonce_start];
    let nonce = &file_bytes[nonce_start..ciphertext_start];
    let ciphertext = &file_bytes[ciphertext_start..];

    let key = derive_encryption_key(passphrase, salt);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let compressed = cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| anyhow::anyhow!("解密备份失败（密码错误或文件已损坏）"))?;

    let json = decompress_bytes_with_limit(&compressed, MAX_IMPORT_JSON_BYTES)
        .context("解压已解密的备份失败")?;

    let store: AccountsStore = serde_json::from_slice(&json).context("解析已解密的账号备份失败")?;

    Ok(store)
}

fn derive_encryption_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, FULL_KDF_ITERATIONS, &mut key);
    key
}

fn compress_bytes(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(input)?;
    encoder.finish().context("完成压缩失败")
}

fn decompress_bytes_with_limit(input: &[u8], max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(input);
    let mut limited = decoder.by_ref().take(max_bytes + 1);
    let mut decompressed = Vec::new();
    limited.read_to_end(&mut decompressed)?;

    if decompressed.len() as u64 > max_bytes {
        anyhow::bail!("导入数据过大");
    }

    Ok(decompressed)
}

fn write_encrypted_file(path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    fs::write(path, bytes).with_context(|| format!("写入文件失败：{path}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置文件权限失败：{path}"))?;
    }

    Ok(())
}

fn read_encrypted_file(path: &str) -> anyhow::Result<Vec<u8>> {
    let metadata = fs::metadata(path).with_context(|| format!("读取文件信息失败：{path}"))?;
    if metadata.len() > MAX_IMPORT_FILE_BYTES {
        anyhow::bail!("加密备份文件过大");
    }

    fs::read(path).with_context(|| format!("读取文件失败：{path}"))
}

fn validate_imported_store(store: &AccountsStore) -> anyhow::Result<()> {
    let mut ids = HashSet::new();
    let mut names = HashSet::new();

    for account in &store.accounts {
        if account.id.trim().is_empty() {
            anyhow::bail!("导入数据中有账号标识为空");
        }
        if account.name.trim().is_empty() {
            anyhow::bail!("导入数据中有账号名称为空");
        }
        if !ids.insert(account.id.clone()) {
            anyhow::bail!("导入数据中存在重复账号标识：{}", account.id);
        }
        if !names.insert(account.name.clone()) {
            anyhow::bail!("导入数据中存在重复账号名称：{}", account.name);
        }
    }

    if let Some(active_id) = &store.active_account_id {
        if !ids.contains(active_id) {
            anyhow::bail!("导入数据引用了不存在的当前账号：{active_id}");
        }
    }

    Ok(())
}

fn merge_accounts_store(
    mut current: AccountsStore,
    imported: AccountsStore,
) -> (AccountsStore, ImportAccountsSummary) {
    let imported_version = imported.version;
    let total_in_payload = imported.accounts.len();
    let mut imported_count = 0usize;
    let mut existing_ids: HashSet<String> = current.accounts.iter().map(|a| a.id.clone()).collect();
    let mut existing_names: HashSet<String> =
        current.accounts.iter().map(|a| a.name.clone()).collect();

    for account in imported.accounts {
        if existing_ids.contains(&account.id) || existing_names.contains(&account.name) {
            continue;
        }
        existing_ids.insert(account.id.clone());
        existing_names.insert(account.name.clone());
        current.accounts.push(account);
        imported_count += 1;
    }

    current.version = current.version.max(imported_version).max(1);

    let current_active_is_valid = current
        .active_account_id
        .as_ref()
        .is_some_and(|id| current.accounts.iter().any(|a| &a.id == id));

    // Importing a backup adds accounts only. Its selected account belongs to
    // another session and must never become our claimed active login.
    if !current_active_is_valid {
        current.active_account_id = None;
    }

    (
        current,
        ImportAccountsSummary {
            total_in_payload,
            imported_count,
            skipped_count: total_in_payload.saturating_sub(imported_count),
        },
    )
}

/// Get the list of masked account IDs
#[tauri::command]
pub async fn get_masked_account_ids() -> Result<Vec<String>, String> {
    crate::auth::storage::get_masked_account_ids().map_err(|e| e.to_string())
}

/// Set the list of masked account IDs
#[tauri::command]
pub async fn set_masked_account_ids(ids: Vec<String>) -> Result<(), String> {
    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    crate::auth::storage::set_masked_account_ids(ids).map_err(|e| e.to_string())
}

#[cfg(test)]
mod safe_import_tests {
    use super::*;

    #[test]
    fn displayed_account_tracks_official_login_after_external_switch() {
        let old = StoredAccount::new_api_key("old".into(), "synthetic-old".into());
        let live = StoredAccount::new_api_key("live".into(), "synthetic-live".into());
        let live_id = live.id.clone();
        let mut store = AccountsStore {
            active_account_id: Some(old.id.clone()),
            accounts: vec![old, live],
            ..AccountsStore::default()
        };
        let auth = AuthDotJson {
            openai_api_key: Some("synthetic-live".into()),
            tokens: None,
            last_refresh: None,
        };
        select_live_account(&mut store, Some(&auth));
        assert_eq!(store.active_account_id, Some(live_id));
        select_live_account(&mut store, None);
        assert!(store.active_account_id.is_none());
    }

    #[test]
    fn backup_import_does_not_activate_its_selected_account() {
        let account = StoredAccount::new_api_key("imported".into(), "synthetic-key".into());
        let imported = AccountsStore {
            active_account_id: Some(account.id.clone()),
            accounts: vec![account],
            ..AccountsStore::default()
        };
        let (merged, summary) = merge_accounts_store(AccountsStore::default(), imported);
        assert_eq!(summary.imported_count, 1);
        assert!(merged.active_account_id.is_none());
    }

    #[test]
    fn backup_import_preserves_existing_selection() {
        let active = StoredAccount::new_api_key("existing".into(), "synthetic-old".into());
        let another = StoredAccount::new_api_key("imported".into(), "synthetic-new".into());
        let active_id = active.id.clone();
        let current = AccountsStore {
            active_account_id: Some(active_id.clone()),
            accounts: vec![active],
            ..AccountsStore::default()
        };
        let imported = AccountsStore {
            active_account_id: Some(another.id.clone()),
            accounts: vec![another],
            ..AccountsStore::default()
        };
        let (merged, _) = merge_accounts_store(current, imported);
        assert_eq!(
            merged.active_account_id.as_deref(),
            Some(active_id.as_str())
        );
        assert_eq!(merged.accounts.len(), 2);
    }
}
