//! OAuth login Tauri commands

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

use crate::auth::oauth_server::{start_oauth_login, wait_for_oauth_login, OAuthLoginResult};
use crate::auth::{add_account, load_accounts, AUTH_OPERATION_LOCK};
use crate::types::{AccountInfo, OAuthLoginInfo};

struct PendingOAuth {
    flow_id: String,
    // Taking the receiver must leave the cancellation handle registered while waiting.
    rx: Option<oneshot::Receiver<anyhow::Result<OAuthLoginResult>>>,
    cancelled: Arc<AtomicBool>,
}

// Global state for pending OAuth login
static PENDING_OAUTH: Mutex<Option<PendingOAuth>> = Mutex::new(None);

fn register_oauth_flow() -> Result<String, String> {
    let flow_id = uuid::Uuid::new_v4().to_string();
    let mut pending = PENDING_OAUTH
        .lock()
        .map_err(|_| "登录流程状态不可用，请重新开始登录".to_string())?;
    if let Some(previous) = pending.take() {
        previous.cancelled.store(true, Ordering::SeqCst);
    }
    *pending = Some(PendingOAuth {
        flow_id: flow_id.clone(),
        rx: None,
        cancelled: Arc::new(AtomicBool::new(false)),
    });
    Ok(flow_id)
}

fn attach_oauth_receiver(
    flow_id: &str,
    rx: oneshot::Receiver<anyhow::Result<OAuthLoginResult>>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut pending = match PENDING_OAUTH.lock() {
        Ok(pending) => pending,
        Err(_) => {
            cancelled.store(true, Ordering::SeqCst);
            return Err("登录流程状态不可用，请重新开始登录".into());
        }
    };
    let current = pending
        .as_mut()
        .filter(|current| current.flow_id == flow_id && !current.cancelled.load(Ordering::SeqCst));
    let Some(current) = current else {
        cancelled.store(true, Ordering::SeqCst);
        return Err("登录已取消，或已开始新的登录流程".into());
    };
    current.rx = Some(rx);
    current.cancelled = cancelled;
    Ok(())
}

fn clear_oauth_flow(flow_id: &str) {
    if let Ok(mut pending) = PENDING_OAUTH.lock() {
        if pending
            .as_ref()
            .is_some_and(|current| current.flow_id == flow_id)
        {
            pending.take();
        }
    }
}

/// Start the OAuth login flow.
#[tauri::command]
pub async fn start_login(account_name: String) -> Result<OAuthLoginInfo, String> {
    // Register before starting the listener so cancellation/replacement also works
    // while startup is pending, without an older startup overwriting a newer one.
    let flow_id = register_oauth_flow()?;
    let (info, rx, cancelled) = match start_oauth_login(account_name.trim().to_string()).await {
        Ok(started) => started,
        Err(error) => {
            clear_oauth_flow(&flow_id);
            return Err(error.to_string());
        }
    };
    attach_oauth_receiver(&flow_id, rx, cancelled)?;
    Ok(info)
}

/// Wait for OAuth login and save the account; switching is a separate user action.
#[tauri::command]
pub async fn complete_login() -> Result<AccountInfo, String> {
    let (flow_id, rx, cancelled) = {
        let mut pending = PENDING_OAUTH
            .lock()
            .map_err(|_| "登录流程状态不可用，请重新开始登录".to_string())?;
        let current = pending
            .as_mut()
            .ok_or_else(|| "没有等待中的登录流程".to_string())?;
        let rx = current
            .rx
            .take()
            .ok_or_else(|| "登录流程尚未开始，或正在等待同一流程完成".to_string())?;
        (current.flow_id.clone(), rx, current.cancelled.clone())
    };

    let account = match wait_for_oauth_login(rx).await {
        Ok(account) => account,
        Err(error) => {
            clear_oauth_flow(&flow_id);
            return Err(error.to_string());
        }
    };
    if cancelled.load(Ordering::SeqCst) {
        clear_oauth_flow(&flow_id);
        return Err("登录已取消，或已开始新的登录流程".into());
    }

    let _auth_guard = AUTH_OPERATION_LOCK.lock().await;
    // Recheck after waiting for the auth lock. Hold the flow lock through the
    // synchronous save, so a cancel/replacement that wins first prevents saving.
    let mut pending = PENDING_OAUTH
        .lock()
        .map_err(|_| "登录流程状态不可用，请重新开始登录".to_string())?;
    if !pending
        .as_ref()
        .is_some_and(|current| current.flow_id == flow_id)
        || cancelled.load(Ordering::SeqCst)
    {
        if pending
            .as_ref()
            .is_some_and(|current| current.flow_id == flow_id)
        {
            pending.take();
        }
        return Err("登录已取消，或已开始新的登录流程".into());
    }

    let result = (|| {
        // Adding an account must not replace the credentials of a running Codex.
        let stored = add_account(account).map_err(|e| e.to_string())?;
        let store = load_accounts().map_err(|e| e.to_string())?;
        Ok(AccountInfo::from_stored(
            &stored,
            store.active_account_id.as_deref(),
        ))
    })();
    pending.take();
    result
}

/// Cancel the current OAuth login, including a flow already being awaited.
#[tauri::command]
pub async fn cancel_login() -> Result<(), String> {
    let mut pending = PENDING_OAUTH
        .lock()
        .map_err(|_| "登录流程状态不可用，请重新开始登录".to_string())?;
    if let Some(pending_oauth) = pending.take() {
        pending_oauth.cancelled.store(true, Ordering::SeqCst);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        attach_oauth_receiver, cancel_login, complete_login, register_oauth_flow, OAuthLoginResult,
        PENDING_OAUTH,
    };
    use crate::auth::{
        add_account, get_accounts_file, get_codex_auth_file, load_accounts, refresh_chatgpt_tokens,
        remove_account, save_accounts, set_active_account,
    };
    use crate::types::{AuthData, StoredAccount};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::oneshot;

    const ISOLATED_ROOT_ENV: &str = "CODEX_SWITCHER_AUTH_REGRESSION_ROOT";

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn synthetic_account(name: &str) -> StoredAccount {
        StoredAccount::new_chatgpt(
            name.into(),
            Some(format!("{name}@example.invalid")),
            Some("pro".into()),
            None,
            format!("synthetic-id-{name}"),
            format!("synthetic-access-{name}"),
            format!("synthetic-refresh-{name}"),
            Some(format!("synthetic-account-{name}")),
        )
    }

    async fn finish_synthetic_login(name: &str) -> crate::types::AccountInfo {
        let (tx, rx) = oneshot::channel();
        let flow_id = register_oauth_flow().unwrap();
        attach_oauth_receiver(&flow_id, rx, Arc::new(AtomicBool::new(false))).unwrap();
        assert!(tx
            .send(Ok(OAuthLoginResult {
                account: synthetic_account(name),
            }))
            .is_ok());
        complete_login()
            .await
            .expect("save synthetic OAuth account")
    }

    // Run the real persistence path in a child process with test-only path
    // overrides. HOME, CODEX_HOME and parallel tests remain untouched.
    #[test]
    fn adding_oauth_accounts_preserves_current_codex_login() {
        let root = TestDirectory(std::env::temp_dir().join(format!(
            "codex-switcher-auth-regression-{}",
            uuid::Uuid::new_v4()
        )));
        fs::create_dir(&root.0).unwrap();
        let config_dir = root.0.join("config");
        let codex_home = root.0.join("codex-home");
        fs::create_dir(&config_dir).unwrap();
        fs::create_dir(&codex_home).unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::oauth::tests::isolated_account_additions",
                "--ignored",
                "--nocapture",
            ])
            .env("CODEX_SWITCHER_TEST_CONFIG_DIR", &config_dir)
            .env("CODEX_SWITCHER_TEST_CODEX_DIR", &codex_home)
            .env(ISOLATED_ROOT_ENV, &root.0)
            .output()
            .expect("run isolated account-addition regression test");
        assert!(
            output.status.success(),
            "isolated regression failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    #[ignore = "launched by the parent regression test with isolated test-only directories"]
    async fn isolated_account_additions() {
        // Verify isolation before calling any application persistence function.
        let root = PathBuf::from(std::env::var_os(ISOLATED_ROOT_ENV).expect("isolated test only"));
        let config_dir = PathBuf::from(std::env::var_os("CODEX_SWITCHER_TEST_CONFIG_DIR").unwrap());
        let codex_home = PathBuf::from(std::env::var_os("CODEX_SWITCHER_TEST_CODEX_DIR").unwrap());
        assert_eq!(config_dir, root.join("config"));
        assert_eq!(codex_home, root.join("codex-home"));
        assert_eq!(
            get_accounts_file().unwrap(),
            config_dir.join("accounts.json")
        );
        let auth_path = get_codex_auth_file().unwrap();
        assert_eq!(auth_path, codex_home.join("auth.json"));
        assert!(!auth_path.exists());

        let imported_first = add_account(synthetic_account("imported-first")).unwrap();
        assert_eq!(load_accounts().unwrap().active_account_id, None);
        assert!(!auth_path.exists());
        remove_account(&imported_first.id).unwrap();

        let first = finish_synthetic_login("first").await;
        let store = load_accounts().unwrap();
        assert!(!first.is_active);
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.active_account_id, None);
        assert_eq!(store.accounts[0].last_used_at, None);
        assert!(
            !auth_path.exists(),
            "adding must not create official auth.json"
        );

        // File import uses this same add_account entry point. It must also leave
        // the first saved account unselected until an explicit Switch action.
        add_account(synthetic_account("imported")).unwrap();
        assert_eq!(load_accounts().unwrap().active_account_id, None);
        assert!(!auth_path.exists());

        set_active_account(&first.id).unwrap();
        let before = load_accounts().unwrap();
        let existing_accounts = serde_json::to_value(&before.accounts).unwrap();
        let live_auth = br#"{"tokens":{"id_token":"synthetic-live-id","access_token":"synthetic-live-access","refresh_token":"synthetic-rotated-refresh","account_id":"synthetic-account-first"},"last_refresh":null}"#;
        fs::write(&auth_path, live_auth).unwrap();

        let third = finish_synthetic_login("third").await;
        let after = load_accounts().unwrap();
        assert!(!third.is_active);
        assert_eq!(after.active_account_id, Some(first.id.clone()));
        assert_eq!(after.accounts.len(), 3);
        assert_eq!(
            serde_json::to_value(&after.accounts[..2]).unwrap(),
            existing_accounts
        );
        assert_eq!(after.accounts[2].last_used_at, None);
        assert_eq!(fs::read(&auth_path).unwrap(), live_auth);

        // A rejected duplicate must preserve both stores as well.
        let stored_bytes = fs::read(get_accounts_file().unwrap()).unwrap();
        assert!(add_account(synthetic_account("third")).is_err());
        assert_eq!(
            fs::read(get_accounts_file().unwrap()).unwrap(),
            stored_bytes
        );
        assert_eq!(fs::read(&auth_path).unwrap(), live_auth);

        // A real current login is owned by Codex even if no row is selected.
        // The deliberately missing refresh token makes a regression fail before
        // any network request; this test never depends on live OAuth or PIDs.
        let before_refresh_check = load_accounts().unwrap();
        let mut unselected = before_refresh_check.clone();
        unselected.active_account_id = None;
        if let AuthData::ChatGPT { refresh_token, .. } = &mut unselected.accounts[0].auth_data {
            refresh_token.clear();
        }
        save_accounts(&unselected).unwrap();
        let current_without_refresh = br#"{"tokens":{"id_token":"synthetic-live-id","access_token":"synthetic-live-access","refresh_token":"","account_id":"synthetic-account-first"},"last_refresh":null}"#;
        fs::write(&auth_path, current_without_refresh).unwrap();
        let unchanged = refresh_chatgpt_tokens(&unselected.accounts[0])
            .await
            .expect("leave the real current login's refresh lifecycle to Codex");
        let AuthData::ChatGPT { refresh_token, .. } = unchanged.auth_data else {
            panic!("expected ChatGPT account");
        };
        assert!(refresh_token.is_empty());
        assert_eq!(load_accounts().unwrap().active_account_id, None);
        assert_eq!(fs::read(&auth_path).unwrap(), current_without_refresh);
        save_accounts(&before_refresh_check).unwrap();
        fs::write(&auth_path, live_auth).unwrap();

        remove_account(&third.id).unwrap();
        assert_eq!(
            load_accounts().unwrap().active_account_id,
            Some(first.id.clone())
        );
        remove_account(&first.id).unwrap();
        let remaining = load_accounts().unwrap();
        assert_eq!(remaining.accounts.len(), 1);
        assert_eq!(remaining.active_account_id, None);
        assert_eq!(fs::read(&auth_path).unwrap(), live_auth);

        let unchanged_accounts = fs::read(get_accounts_file().unwrap()).unwrap();
        let (cancelled_flow, cancelled_tx, cancelled_flag) = begin_synthetic_login();
        let cancelled_wait = tokio::spawn(complete_login());
        wait_until_receiver_is_taken(&cancelled_flow).await;
        cancel_login().await.unwrap();
        assert!(cancelled_flag.load(Ordering::SeqCst));
        assert!(cancelled_tx
            .send(Ok(OAuthLoginResult {
                account: synthetic_account("must-not-save"),
            }))
            .is_ok());
        assert!(cancelled_wait.await.unwrap().is_err());
        assert_eq!(
            fs::read(get_accounts_file().unwrap()).unwrap(),
            unchanged_accounts
        );
        assert_eq!(fs::read(&auth_path).unwrap(), live_auth);

        // A new flow cancels the old waiter. The old completion must neither save
        // its account nor clear/cancel the newer receiver.
        let (old_flow, old_tx, old_flag) = begin_synthetic_login();
        let old_wait = tokio::spawn(complete_login());
        wait_until_receiver_is_taken(&old_flow).await;
        let (new_flow, new_tx, new_flag) = begin_synthetic_login();
        assert!(old_flag.load(Ordering::SeqCst));
        assert!(old_tx
            .send(Ok(OAuthLoginResult {
                account: synthetic_account("replaced-must-not-save"),
            }))
            .is_ok());
        assert!(old_wait.await.unwrap().is_err());
        assert!(!new_flag.load(Ordering::SeqCst));
        assert_eq!(
            PENDING_OAUTH.lock().unwrap().as_ref().unwrap().flow_id,
            new_flow
        );
        assert_eq!(
            fs::read(get_accounts_file().unwrap()).unwrap(),
            unchanged_accounts
        );
        assert!(new_tx
            .send(Ok(OAuthLoginResult {
                account: synthetic_account("latest-flow"),
            }))
            .is_ok());
        let saved = complete_login().await.unwrap();
        assert_eq!(saved.name, "latest-flow");
        assert!(PENDING_OAUTH.lock().unwrap().is_none());
        assert_eq!(fs::read(&auth_path).unwrap(), live_auth);

        // Cancellation after the callback, while waiting for another auth operation,
        // must still prevent the eventual disk write.
        let before_waiting_cancel = fs::read(get_accounts_file().unwrap()).unwrap();
        let auth_guard = crate::auth::AUTH_OPERATION_LOCK.lock().await;
        let (waiting_flow, waiting_tx, _) = begin_synthetic_login();
        let waiting = tokio::spawn(complete_login());
        wait_until_receiver_is_taken(&waiting_flow).await;
        assert!(waiting_tx
            .send(Ok(OAuthLoginResult {
                account: synthetic_account("waiting-must-not-save"),
            }))
            .is_ok());
        tokio::task::yield_now().await;
        cancel_login().await.unwrap();
        drop(auth_guard);
        assert!(waiting.await.unwrap().is_err());
        assert_eq!(
            fs::read(get_accounts_file().unwrap()).unwrap(),
            before_waiting_cancel
        );
        assert_eq!(fs::read(&auth_path).unwrap(), live_auth);

        // A listener that finishes starting late cannot overwrite a newer flow.
        let obsolete_flow = register_oauth_flow().unwrap();
        let latest_flow = register_oauth_flow().unwrap();
        let (_late_tx, late_rx) = oneshot::channel();
        let late_cancelled = Arc::new(AtomicBool::new(false));
        assert!(attach_oauth_receiver(&obsolete_flow, late_rx, late_cancelled.clone()).is_err());
        assert!(late_cancelled.load(Ordering::SeqCst));
        assert_eq!(
            PENDING_OAUTH.lock().unwrap().as_ref().unwrap().flow_id,
            latest_flow
        );
        cancel_login().await.unwrap();
    }

    fn begin_synthetic_login() -> (
        String,
        oneshot::Sender<anyhow::Result<OAuthLoginResult>>,
        Arc<AtomicBool>,
    ) {
        let (tx, rx) = oneshot::channel();
        let flow_id = register_oauth_flow().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        attach_oauth_receiver(&flow_id, rx, cancelled.clone()).unwrap();
        (flow_id, tx, cancelled)
    }

    async fn wait_until_receiver_is_taken(flow_id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let waiting = PENDING_OAUTH
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|current| current.flow_id == flow_id && current.rx.is_none());
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("synthetic completion started waiting");
    }
}
