//! Account storage module - manages reading and writing accounts.json

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};

use crate::types::{
    parse_chatgpt_id_token_claims, AccountsStore, AppSettings, AuthData, AuthDotJson, StoredAccount,
};

pub fn sync_active_account_tokens(store: &mut AccountsStore, auth: &AuthDotJson) -> bool {
    let Some(active_id) = store.active_account_id.as_deref() else {
        return false;
    };
    let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == active_id)
    else {
        return false;
    };

    sync_account_tokens(account, auth)
}

/// Match the actual Codex login, independently of the switcher's selected row.
pub(crate) fn auth_matches_account(account: &StoredAccount, auth: &AuthDotJson) -> bool {
    match &account.auth_data {
        AuthData::ApiKey { key } => {
            !key.is_empty() && auth.tokens.is_none() && auth.openai_api_key.as_ref() == Some(key)
        }
        AuthData::ChatGPT {
            id_token,
            account_id,
            ..
        } => {
            if auth.openai_api_key.is_some() {
                return false;
            }
            let Some(tokens) = auth.tokens.as_ref() else {
                return false;
            };
            let Some((stored_id, stored_subject)) =
                chatgpt_identity(id_token, account_id.as_deref())
            else {
                return false;
            };
            let Some((live_id, live_subject)) =
                chatgpt_identity(&tokens.id_token, tokens.account_id.as_deref())
            else {
                return false;
            };
            stored_id == live_id
                && match (stored_subject, live_subject) {
                    (Some(stored), Some(live)) => stored == live,
                    _ => true,
                }
        }
    }
}

fn chatgpt_identity(id_token: &str, account_id: Option<&str>) -> Option<(String, Option<String>)> {
    let claimed_id = parse_chatgpt_id_token_claims(id_token)
        .account_id
        .filter(|id| !id.trim().is_empty());
    let account_id = account_id.filter(|id| !id.trim().is_empty());
    if let (Some(claimed), Some(explicit)) = (claimed_id.as_deref(), account_id) {
        if claimed != explicit {
            return None;
        }
    }
    let resolved_id = claimed_id.or_else(|| account_id.map(String::from))?;
    let parts: Vec<_> = id_token.split('.').collect();
    let subject = if parts.len() == 3 {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .ok()
            .and_then(|payload| serde_json::from_slice::<serde_json::Value>(&payload).ok())
            .and_then(|claims| {
                claims
                    .get("sub")
                    .and_then(|value| value.as_str())
                    .map(String::from)
            })
            .filter(|subject| !subject.trim().is_empty())
    } else {
        None
    };
    Some((resolved_id, subject))
}

/// Preserve live token rotations for every matching saved copy, without selecting it.
pub(crate) fn sync_matching_account_tokens(store: &mut AccountsStore, auth: &AuthDotJson) -> bool {
    let mut changed = false;
    for account in &mut store.accounts {
        changed |= sync_account_tokens(account, auth);
    }
    changed
}

fn sync_account_tokens(account: &mut StoredAccount, auth: &AuthDotJson) -> bool {
    if !auth_matches_account(account, auth) {
        return false;
    }
    let Some(tokens) = auth.tokens.as_ref() else {
        return false;
    };
    if tokens.id_token.is_empty()
        || tokens.access_token.is_empty()
        || tokens.refresh_token.is_empty()
    {
        return false;
    }
    let Some((current_account_id, _)) =
        chatgpt_identity(&tokens.id_token, tokens.account_id.as_deref())
    else {
        return false;
    };
    let AuthData::ChatGPT {
        id_token,
        access_token,
        refresh_token,
        account_id,
    } = &mut account.auth_data
    else {
        return false;
    };

    let changed = *id_token != tokens.id_token
        || *access_token != tokens.access_token
        || *refresh_token != tokens.refresh_token
        || account_id.as_ref() != Some(&current_account_id);
    if !changed {
        return false;
    }

    id_token.clone_from(&tokens.id_token);
    access_token.clone_from(&tokens.access_token);
    refresh_token.clone_from(&tokens.refresh_token);
    *account_id = Some(current_account_id);
    true
}

/// Get the path to the codex-switcher config directory
pub fn get_config_dir() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(directory) = std::env::var_os("CODEX_SWITCHER_TEST_CONFIG_DIR") {
        return Ok(PathBuf::from(directory));
    }
    let home = dirs::home_dir().context("无法找到用户主目录")?;
    Ok(home.join(".codex-switcher"))
}

/// Get the path to accounts.json
pub fn get_accounts_file() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("accounts.json"))
}

pub fn get_settings_file() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("settings.json"))
}

/// Load the accounts store from disk
pub fn load_accounts() -> Result<AccountsStore> {
    let path = get_accounts_file()?;

    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取账号文件失败：{}", path.display()))?;

    let store: AccountsStore = serde_json::from_str(&content)
        .with_context(|| format!("解析账号文件失败：{}", path.display()))?;

    Ok(store)
}

pub fn load_app_settings() -> Result<AppSettings> {
    let path = get_settings_file()?;

    if !path.exists() {
        return Ok(AppSettings::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取设置文件失败：{}", path.display()))?;

    let settings: AppSettings = serde_json::from_str(&content)
        .with_context(|| format!("解析设置文件失败：{}", path.display()))?;

    Ok(settings)
}

pub fn save_app_settings(settings: &AppSettings) -> Result<()> {
    let path = get_settings_file()?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(settings).context("生成设置数据失败")?;
    fs::write(&path, content).with_context(|| format!("保存设置失败：{}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&path, perms)?;
    }

    Ok(())
}

/// Save the accounts store to disk
pub fn save_accounts(store: &AccountsStore) -> Result<()> {
    let path = get_accounts_file()?;

    // Ensure the config directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(store).context("生成账号数据失败")?;

    fs::write(&path, content).with_context(|| format!("保存账号文件失败：{}", path.display()))?;

    // Set restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&path, perms)?;
    }

    Ok(())
}

/// Save a new account without changing the account selected for Codex.
pub fn add_account(account: StoredAccount) -> Result<StoredAccount> {
    let mut store = load_accounts()?;

    // Check for duplicate names
    if store.accounts.iter().any(|a| a.name == account.name) {
        anyhow::bail!("已存在名为“{}”的账号", account.name);
    }

    let account_clone = account.clone();
    store.accounts.push(account);

    save_accounts(&store)?;
    Ok(account_clone)
}

/// Remove an account by ID
pub fn remove_account(account_id: &str) -> Result<()> {
    let mut store = load_accounts()?;

    let initial_len = store.accounts.len();
    store.accounts.retain(|a| a.id != account_id);

    if store.accounts.len() == initial_len {
        anyhow::bail!("找不到账号：{account_id}");
    }

    // Removing a saved login must not pretend another account was switched in.
    if store.active_account_id.as_deref() == Some(account_id) {
        store.active_account_id = None;
    }

    save_accounts(&store)?;
    Ok(())
}

/// Update the active account ID
pub fn set_active_account(account_id: &str) -> Result<()> {
    let mut store = load_accounts()?;

    // Verify the account exists
    if !store.accounts.iter().any(|a| a.id == account_id) {
        anyhow::bail!("找不到账号：{account_id}");
    }

    store.active_account_id = Some(account_id.to_string());
    save_accounts(&store)?;
    Ok(())
}

/// Get an account by ID
pub fn get_account(account_id: &str) -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    Ok(store.accounts.into_iter().find(|a| a.id == account_id))
}

/// Get the currently active account
pub fn get_active_account() -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    let active_id = match &store.active_account_id {
        Some(id) => id,
        None => return Ok(None),
    };
    Ok(store.accounts.into_iter().find(|a| a.id == *active_id))
}

/// Update an account's last_used_at timestamp
pub fn touch_account(account_id: &str) -> Result<()> {
    let mut store = load_accounts()?;

    if let Some(account) = store.accounts.iter_mut().find(|a| a.id == account_id) {
        account.last_used_at = Some(chrono::Utc::now());
        save_accounts(&store)?;
    }

    Ok(())
}

/// Update an account's metadata (name, email, plan_type, subscription expiry)
pub fn update_account_metadata(
    account_id: &str,
    name: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    subscription_expires_at: Option<Option<DateTime<Utc>>>,
) -> Result<StoredAccount> {
    let mut store = load_accounts()?;

    // Check for duplicate names first (if renaming)
    if let Some(ref new_name) = name {
        if store
            .accounts
            .iter()
            .any(|a| a.id != account_id && a.name == *new_name)
        {
            anyhow::bail!("已存在名为“{new_name}”的账号");
        }
    }

    // Now find and update the account
    let account = store
        .accounts
        .iter_mut()
        .find(|a| a.id == account_id)
        .context("找不到账号")?;

    let mut changed = false;

    if let Some(new_name) = name {
        if account.name != new_name {
            account.name = new_name;
            changed = true;
        }
    }

    if let Some(new_email) = email {
        if account.email.as_ref() != Some(&new_email) {
            account.email = Some(new_email);
            changed = true;
        }
    }

    if let Some(new_plan_type) = plan_type {
        if account.plan_type.as_ref() != Some(&new_plan_type) {
            account.plan_type = Some(new_plan_type);
            changed = true;
        }
    }

    if let Some(subscription_expires_at) = subscription_expires_at {
        if account.subscription_expires_at != subscription_expires_at {
            account.subscription_expires_at = subscription_expires_at;
            changed = true;
        }
    }

    let updated = account.clone();
    if changed {
        save_accounts(&store)?;
        println!("[Account] Saved updated metadata for: {}", updated.name);
    }
    Ok(updated)
}

/// Update ChatGPT OAuth tokens for an account and return the updated account.
pub fn update_account_chatgpt_tokens(
    account_id: &str,
    id_token: String,
    access_token: String,
    refresh_token: String,
    chatgpt_account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    subscription_expires_at: Option<DateTime<Utc>>,
) -> Result<StoredAccount> {
    let mut store = load_accounts()?;

    let account = store
        .accounts
        .iter_mut()
        .find(|a| a.id == account_id)
        .context("找不到账号")?;

    match &mut account.auth_data {
        AuthData::ChatGPT {
            id_token: stored_id_token,
            access_token: stored_access_token,
            refresh_token: stored_refresh_token,
            account_id: stored_account_id,
        } => {
            *stored_id_token = id_token;
            *stored_access_token = access_token;
            *stored_refresh_token = refresh_token;
            if let Some(new_account_id) = chatgpt_account_id {
                *stored_account_id = Some(new_account_id);
            }
        }
        AuthData::ApiKey { .. } => {
            anyhow::bail!("API 密钥账号不支持更新 OAuth 登录凭据");
        }
    }

    if let Some(new_email) = email {
        account.email = Some(new_email);
    }

    if let Some(new_plan_type) = plan_type {
        account.plan_type = Some(new_plan_type);
    }

    if let Some(subscription_expires_at) = subscription_expires_at {
        account.subscription_expires_at = Some(subscription_expires_at);
    }

    let updated = account.clone();
    save_accounts(&store)?;
    Ok(updated)
}

/// Get the list of masked account IDs
pub fn get_masked_account_ids() -> Result<Vec<String>> {
    let store = load_accounts()?;
    Ok(store.masked_account_ids.clone())
}

/// Set the list of masked account IDs
pub fn set_masked_account_ids(ids: Vec<String>) -> Result<()> {
    let mut store = load_accounts()?;
    store.masked_account_ids = ids;
    save_accounts(&store)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{auth_matches_account, sync_active_account_tokens, sync_matching_account_tokens};
    use crate::types::{AccountsStore, AuthData, AuthDotJson, StoredAccount, TokenData};
    use base64::Engine;

    fn account(name: &str, account_id: &str, suffix: &str) -> StoredAccount {
        StoredAccount::new_chatgpt(
            name.into(),
            None,
            None,
            None,
            format!("id-{suffix}"),
            format!("access-{suffix}"),
            format!("refresh-{suffix}"),
            Some(account_id.into()),
        )
    }

    fn auth(account_id: &str, suffix: &str) -> AuthDotJson {
        AuthDotJson {
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: format!("id-{suffix}"),
                access_token: format!("access-{suffix}"),
                refresh_token: format!("refresh-{suffix}"),
                account_id: Some(account_id.into()),
            }),
            last_refresh: None,
        }
    }

    fn refresh_token(account: &StoredAccount) -> &str {
        match &account.auth_data {
            AuthData::ChatGPT { refresh_token, .. } => refresh_token,
            AuthData::ApiKey { .. } => panic!("expected ChatGPT account"),
        }
    }

    fn id_token_with_account_id(account_id: &str, suffix: &str) -> String {
        let payload =
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.{suffix}")
    }

    fn id_token_with_subject(account_id: &str, subject: &str) -> String {
        let payload = serde_json::json!({
            "sub": subject,
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("header.{encoded}.synthetic-signature")
    }

    #[test]
    fn matches_live_account_without_relying_on_selected_row() {
        let first = account("First copy", "workspace-a", "old");
        let second = account("Second copy", "workspace-a", "older");
        let unrelated = account("Other", "workspace-b", "other");
        let mut store = AccountsStore {
            accounts: vec![first, second, unrelated],
            active_account_id: None,
            ..AccountsStore::default()
        };

        assert!(sync_matching_account_tokens(
            &mut store,
            &auth("workspace-a", "rotated")
        ));
        assert_eq!(store.active_account_id, None);
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-rotated");
        assert_eq!(refresh_token(&store.accounts[1]), "refresh-rotated");
        assert_eq!(refresh_token(&store.accounts[2]), "refresh-other");
    }

    #[test]
    fn shared_workspace_does_not_mix_different_jwt_subjects() {
        let mut saved = account("Saved", "shared-workspace", "saved");
        let AuthData::ChatGPT { id_token, .. } = &mut saved.auth_data else {
            panic!("expected ChatGPT account");
        };
        *id_token = id_token_with_subject("shared-workspace", "user-a");
        let mut live = auth("shared-workspace", "live");
        live.tokens.as_mut().unwrap().id_token =
            id_token_with_subject("shared-workspace", "user-b");

        assert!(!auth_matches_account(&saved, &live));
        live.tokens.as_mut().unwrap().id_token =
            id_token_with_subject("shared-workspace", "user-a");
        assert!(auth_matches_account(&saved, &live));
    }

    #[test]
    fn conflicting_account_ids_cannot_match_or_replace_stored_tokens() {
        let saved = account("Saved", "workspace-a", "saved");
        let mut live = auth("workspace-b", "live");
        live.tokens.as_mut().unwrap().id_token = id_token_with_account_id("workspace-a", "live");
        let mut store = AccountsStore {
            accounts: vec![saved],
            ..AccountsStore::default()
        };

        assert!(!auth_matches_account(&store.accounts[0], &live));
        assert!(!sync_matching_account_tokens(&mut store, &live));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-saved");
    }

    #[test]
    fn incomplete_live_tokens_do_not_erase_saved_credentials() {
        let mut live = auth("workspace-a", "live");
        live.tokens.as_mut().unwrap().refresh_token.clear();
        let mut store = AccountsStore {
            accounts: vec![account("Saved", "workspace-a", "saved")],
            ..AccountsStore::default()
        };
        assert!(!sync_matching_account_tokens(&mut store, &live));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-saved");
    }

    #[test]
    fn preserves_rotated_tokens_before_switching_away_and_back() {
        let account_a = account("A", "workspace-a", "a1");
        let account_a_id = account_a.id.clone();
        let account_b = account("B", "workspace-b", "b1");
        let account_b_id = account_b.id.clone();
        let mut store = AccountsStore {
            accounts: vec![account_a, account_b],
            active_account_id: Some(account_a_id.clone()),
            ..AccountsStore::default()
        };

        assert!(!sync_active_account_tokens(
            &mut store,
            &auth("workspace-b", "wrong-account")
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        let mut auth_without_top_level_id = auth("workspace-b", "missing-id");
        let tokens = auth_without_top_level_id.tokens.as_mut().unwrap();
        tokens.id_token = id_token_with_account_id("workspace-b", "signature");
        tokens.account_id = None;
        assert!(!sync_active_account_tokens(
            &mut store,
            &auth_without_top_level_id
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        let mut auth_without_identity = auth("workspace-a", "unknown");
        auth_without_identity.tokens.as_mut().unwrap().account_id = None;
        assert!(!sync_active_account_tokens(
            &mut store,
            &auth_without_identity
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        assert!(sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        store.active_account_id = Some(account_b_id);

        let restored_a = store
            .accounts
            .iter()
            .find(|account| account.id == account_a_id)
            .unwrap();
        let AuthData::ChatGPT { refresh_token, .. } = &restored_a.auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(refresh_token, "refresh-a2");
    }

    #[test]
    fn rejects_live_tokens_when_stored_account_identity_is_unknown() {
        let mut account = account("A", "workspace-a", "a1");
        let account_id = account.id.clone();
        let AuthData::ChatGPT {
            id_token,
            account_id: chatgpt_account_id,
            ..
        } = &mut account.auth_data
        else {
            panic!("expected ChatGPT account");
        };
        *id_token = "opaque-id-token".into();
        *chatgpt_account_id = None;

        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some(account_id),
            ..AccountsStore::default()
        };

        assert!(!sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");
    }

    #[test]
    fn derives_stored_identity_from_id_token_and_backfills_account_id() {
        let mut account = account("A", "workspace-a", "a1");
        let account_id = account.id.clone();
        let AuthData::ChatGPT {
            id_token,
            account_id: chatgpt_account_id,
            ..
        } = &mut account.auth_data
        else {
            panic!("expected ChatGPT account");
        };
        *id_token = id_token_with_account_id("workspace-a", "stored");
        *chatgpt_account_id = None;

        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some(account_id),
            ..AccountsStore::default()
        };

        assert!(sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        let AuthData::ChatGPT { account_id, .. } = &store.accounts[0].auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(account_id.as_deref(), Some("workspace-a"));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a2");
    }
}
