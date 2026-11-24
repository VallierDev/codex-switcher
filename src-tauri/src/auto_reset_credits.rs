//! Redeem expiring coupons while the desktop app is running. No inference requests.
use std::{collections::HashMap, path::PathBuf, sync::{Arc, Mutex, OnceLock}};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use crate::{account::AccountStore, usage, AppState};

const LEAD_SECONDS: i64 = 600;

/// Serialize local manual and automatic redemption per account, including the
/// preceding credit-list query, so a manual click cannot race that snapshot.
pub(crate) async fn redemption_guard(id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let lock = LOCKS.get_or_init(Default::default).lock().unwrap()
        .entry(id.to_owned()).or_default().clone();
    lock.lock_owned().await
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Attempt {
    request_id: String,
    finished: bool,
    result: String,
    attempted_at: i64,
}

struct Ledger {
    path: PathBuf,
    attempts: HashMap<String, Attempt>,
}

impl Ledger {
    fn save(&self) -> Result<(), String> {
        let temporary = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&self.attempts).map_err(|e| e.to_string())?;
        std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(temporary, &self.path).map_err(|e| e.to_string())
    }

    fn begin(&mut self, key: &str, now: i64) -> Result<Option<String>, String> {
        let attempt = self.attempts.entry(key.to_owned()).or_insert_with(|| Attempt {
            request_id: uuid::Uuid::new_v4().to_string(),
            ..Attempt::default()
        });
        if attempt.finished { return Ok(None); }
        attempt.attempted_at = now;
        let id = attempt.request_id.clone();
        // Persist before POST: restart/timeout retries must use the same idempotency key.
        self.save()?;
        Ok(Some(id))
    }

    fn finish(&mut self, key: &str, code: &str) -> Result<(), String> {
        if let Some(attempt) = self.attempts.get_mut(key) {
            attempt.result = code.to_owned();
            attempt.finished = matches!(code, "reset" | "already_redeemed" | "no_credit");
            if code == "nothing_to_reset" {
                // This request was explicitly rejected without redemption. A later attempt
                // needs a fresh key after the user's normal usage reaches the quota cap.
                attempt.request_id = uuid::Uuid::new_v4().to_string();
            }
        }
        self.save()
    }
}

fn expiring(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|at| at > now && at - now <= LEAD_SECONDS)
}

fn enabled(app: &tauri::AppHandle) -> bool {
    app.state::<AppState>().store.lock()
        .map(|store| store.settings.auto_redeem_expiring_credits).unwrap_or(false)
}

async fn check_account(app: tauri::AppHandle, id: String, ledger: Arc<Mutex<Ledger>>) -> Result<i64, String> {
    let _guard = redemption_guard(&id).await;
    if !enabled(&app) { return Ok(chrono::Utc::now().timestamp() + 30); }
    let state = app.state::<AppState>();
    let remote = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let settings = &store.settings;
        if matches!(settings.remote_mode.as_str(), "client" | "solo") && !settings.remote_shared_secret.is_empty() {
            Some((settings.remote_server_url.clone(), settings.remote_server_url_fallback.clone(), settings.remote_shared_secret.clone()))
        } else { None }
    };
    let (token, account_id) = if let Some((primary, fallback, secret)) = remote {
        // Server owns refresh tokens in client/solo mode, including inactive accounts.
        let base = crate::remote_client::resolve_base_url(&primary, &fallback).await?;
        let fresh = crate::remote_client::fetch_token(&base, &secret, &id).await?;
        let token = AccountStore::extract_access_token(&fresh.auth_json).ok_or("Missing access token")?;
        (token, AccountStore::extract_account_id(&fresh.auth_json))
    } else {
        crate::resolve_account_access_token(&state, &id, "Unsupported account").await?
    };
    let credits = usage::list_reset_credits(&token, account_id.as_deref()).await?;
    let now = chrono::Utc::now().timestamp();
    let next = credits.iter().filter_map(|c| c.expires_at)
        .filter(|at| *at > now + LEAD_SECONDS)
        .map(|at| at - LEAD_SECONDS).min().unwrap_or(now + 300).min(now + 300);
    let Some(credit) = credits.iter().find(|credit| expiring(credit.expires_at, now)) else {
        return Ok(next);
    };
    if !enabled(&app) { return Ok(now + 30); }
    let key = format!("{}:{}", id, credit.id);
    let request_id = ledger.lock().map_err(|e| e.to_string())?.begin(&key, now)?;
    let Some(request_id) = request_id else { return Ok(now + 30); };
    let result = usage::consume_reset_credit_with_request_id(&token, account_id.as_deref(), &request_id).await?;
    ledger.lock().map_err(|e| e.to_string())?.finish(&key, &result.code)?;
    println!("[AutoResetCredits] account={} result={} windows={}", id, result.code, result.windows_reset);
    if result.ok || result.code == "already_redeemed" {
        // Read actual quota back; never fabricate fresh quota from a POST response.
        let _ = crate::get_quota_by_id(app.state::<AppState>(), app.clone(), id).await;
        let _ = app.emit("accounts-updated", ());
    }
    Ok(now + 30)
}

pub fn start(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let path = AccountStore::config_path().with_file_name("auto-reset-credits.json");
        let attempts = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => { eprintln!("[AutoResetCredits] Invalid ledger; worker disabled"); return; }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(_) => { eprintln!("[AutoResetCredits] Cannot read ledger; worker disabled"); return; }
        };
        let ledger = Arc::new(Mutex::new(Ledger { path, attempts }));
        let mut next_checks: HashMap<String, i64> = HashMap::new();
        loop {
            if enabled(&app) {
                let now = chrono::Utc::now().timestamp();
                let ids: Vec<String> = app.state::<AppState>().store.lock().map(|store| {
                    store.accounts.values().filter(|account| account.is_chatgpt_oauth()
                        && !account.is_banned && !account.is_logged_out && !account.is_token_invalid
                        && next_checks.get(&account.id).copied().unwrap_or(0) <= now)
                        .map(|account| account.id.clone()).collect()
                }).unwrap_or_default();
                let results = stream::iter(ids.into_iter().map(|id| {
                    let app = app.clone();
                    let ledger = ledger.clone();
                    async move {
                        let result = check_account(app, id.clone(), ledger).await;
                        (id, result)
                    }
                })).buffer_unordered(4).collect::<Vec<_>>().await;
                for (id, result) in results {
                    match result {
                        Ok(next) => { next_checks.insert(id, next); }
                        Err(_) => {
                            // Log no upstream body/token/account details on failures.
                            eprintln!("[AutoResetCredits] Check failed; retry in 60s");
                            next_checks.insert(id, chrono::Utc::now().timestamp() + 60);
                        }
                    }
                }
            } else {
                next_checks.clear();
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_window_excludes_expired_and_future_coupons() {
        assert!(!expiring(None, 1000));
        assert!(!expiring(Some(1000), 1000));
        assert!(!expiring(Some(999), 1000));
        assert!(expiring(Some(1001), 1000));
        assert!(expiring(Some(1600), 1000));
        assert!(!expiring(Some(1601), 1000));
    }

    #[test]
    fn retries_survive_restart_without_double_redemption() {
        let path = std::env::temp_dir().join(format!("auto-reset-test-{}.json", uuid::Uuid::new_v4()));
        let mut ledger = Ledger { path: path.clone(), attempts: HashMap::new() };
        let original = ledger.begin("account:credit", 1000).unwrap().unwrap();
        let mut restarted = Ledger { path: path.clone(), attempts: serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap() };
        assert_eq!(restarted.begin("account:credit", 1030).unwrap(), Some(original.clone()));
        restarted.finish("account:credit", "nothing_to_reset").unwrap();
        assert_ne!(restarted.begin("account:credit", 1060).unwrap().unwrap(), original);
        restarted.finish("account:credit", "reset").unwrap();
        assert!(restarted.begin("account:credit", 1090).unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }
}
