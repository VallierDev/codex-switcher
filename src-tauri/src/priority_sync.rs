//! Mini is authoritative for shared priority; clients poll even with phone anchors.
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use crate::{account::{self, AccountStore}, AppState};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PriorityState {
    pub protocol: u32,
    pub revision: u64,
    pub account_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct PriorityRequest {
    pub expected_revision: u64,
    pub account_id: Option<String>,
}

pub fn snapshot(store: &AccountStore) -> PriorityState {
    PriorityState { protocol: 1, revision: store.settings.priority_revision,
        account_id: store.settings.priority_account_id.clone() }
}

pub fn apply(store: &mut AccountStore, incoming: &PriorityState) -> Result<bool, String> {
    apply_with_save(store, incoming, AccountStore::save)
}

fn apply_with_save(store: &mut AccountStore, incoming: &PriorityState,
    save: impl FnOnce(&AccountStore) -> Result<(), String>) -> Result<bool, String> {
    if incoming.protocol != 1 { return Err("Unsupported shared priority protocol".into()); }
    if incoming.revision < store.settings.priority_revision {
        return Err("Shared priority revision moved backwards".into());
    }
    if let Some(id) = incoming.account_id.as_deref() {
        if !store.accounts.get(id).is_some_and(|a| a.is_chatgpt_oauth()) {
            return Err("Priority subscription account is missing locally; sync accounts first".into());
        }
    }
    if snapshot(store) == *incoming { return Ok(false); }
    let previous = snapshot(store);
    let current = store.current.clone();
    store.settings.priority_account_id = incoming.account_id.clone();
    store.settings.priority_revision = incoming.revision;
    let result = (|| {
        if let Some(id) = incoming.account_id.as_deref() {
            if crate::priority_subscription_account(store).is_some() {
                let hot = account::should_hot_switch(&store.settings, store.settings.proxy_enabled);
                store.switch_to(id, hot)?;
            }
        }
        save(store)
    })();
    if let Err(error) = result {
        store.settings.priority_account_id = previous.account_id;
        store.settings.priority_revision = previous.revision;
        store.current = current;
        return Err(error);
    }
    Ok(true)
}

pub fn update(store: &mut AccountStore, request: PriorityRequest) -> Result<PriorityState, String> {
    if store.settings.priority_revision != request.expected_revision {
        return Err("Priority changed on another machine; refresh accounts and retry".into());
    }
    if store.settings.priority_account_id == request.account_id {
        return Ok(snapshot(store));
    }
    let incoming = PriorityState { protocol: 1,
        revision: request.expected_revision.checked_add(1).ok_or("Priority revision overflow")?,
        account_id: request.account_id };
    apply(store, &incoming)?;
    Ok(incoming)
}

pub fn notify(app: &tauri::AppHandle) {
    crate::proxy::invalidate_remote_token_cache();
    app.state::<AppState>().ws_disconnect.notify_waiters();
    let _ = app.emit("accounts-updated", ());
    crate::tray::update_tray_menu(app);
}

pub async fn remote(app: &tauri::AppHandle) -> Result<Option<(String, String)>, String> {
    let settings = app.state::<AppState>().store.lock().map_err(|e| e.to_string())?.settings.clone();
    if !matches!(settings.remote_mode.as_str(), "client" | "solo") { return Ok(None); }
    if settings.remote_shared_secret.is_empty() { return Err("Shared priority requires a configured Server".into()); }
    let base = crate::remote_client::resolve_base_url(&settings.remote_server_url, &settings.remote_server_url_fallback).await?;
    Ok(Some((base, settings.remote_shared_secret)))
}

pub async fn apply_remote(app: &tauri::AppHandle, incoming: PriorityState, base: &str, secret: &str) -> Result<(), String> {
    let missing = incoming.account_id.as_ref().is_some_and(|id| {
        !app.state::<AppState>().store.lock().map(|s| s.accounts.contains_key(id)).unwrap_or(false)
    });
    if missing {
        let list = crate::remote_client::list_accounts(base, secret).await?;
        let account = list.into_iter().find(|a| Some(&a.id) == incoming.account_id.as_ref())
            .ok_or("Shared priority account is missing on Server")?;
        app.state::<AppState>().store.lock().map_err(|e| e.to_string())?.accounts.insert(account.id.clone(), account);
    }
    let changed = apply(&mut *app.state::<AppState>().store.lock().map_err(|e| e.to_string())?, &incoming)?;
    if changed { notify(app); }
    Ok(())
}

pub fn start(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let outcome = async {
                if let Some((base, secret)) = remote(&app).await? {
                    let state = crate::remote_client::get_priority(&base, &secret).await?;
                    apply_remote(&app, state, &base, &secret).await?;
                }
                Ok::<_, String>(())
            }.await;
            if outcome.is_err() {
                // Keep last confirmed priority on unavailable/old servers; never clear it.
                eprintln!("[PrioritySync] Sync unavailable; retaining last confirmed priority");
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_writer_and_legacy_server_cannot_clear_priority() {
        let mut store = AccountStore::default();
        store.settings.priority_revision = 4;
        store.settings.priority_account_id = Some("pinned".into());
        assert!(update(&mut store, PriorityRequest { expected_revision:3, account_id:None }).is_err());
        assert_eq!(store.settings.priority_account_id.as_deref(),Some("pinned"));
        assert!(serde_json::from_str::<PriorityState>("{\"current\":null}").is_err());
        assert!(apply(&mut store,&PriorityState { protocol:1,revision:3,account_id:None }).is_err());
        assert_eq!(store.settings.priority_revision,4);
    }

    #[test]
    fn pin_and_clear_propagate_to_independent_clients_without_touching_disk_auth() {
        let mut store = AccountStore::default();
        store.settings.proxy_enabled = true;
        let account = store.add_account("pro".into(), serde_json::json!({"tokens":{"access_token":"eyJ.test"}}), None);
        let id = account.id.clone();
        let account = store.accounts.get_mut(&id).unwrap();
        account.kind = account::AccountKind::ChatgptOauth;
        account.cached_quota = Some(serde_json::from_value(serde_json::json!({
            "five_hour_left":88.0,"weekly_left":100.0,"five_hour_reset":"later","weekly_reset":"later",
            "plan_type":"pro","updated_at":chrono::Utc::now()
        })).unwrap());
        let mut other = store.clone();
        let pin = PriorityState { protocol:1, revision:1, account_id:Some(id.clone()) };
        for client in [&mut store, &mut other] {
            assert!(apply_with_save(client,&pin, |_| Ok(())).unwrap());
            assert_eq!(client.current.as_deref(),Some(id.as_str()));
            assert_eq!(snapshot(client),pin);
            assert!(!apply_with_save(client,&pin, |_| panic!("unchanged state should not save")).unwrap());
        }
        let clear = PriorityState { protocol:1,revision:2,account_id:None };
        assert!(apply_with_save(&mut other,&clear, |_| Ok(())).unwrap());
        assert!(other.settings.priority_account_id.is_none());
        assert!(apply_with_save(&mut other,&pin, |_| Ok(())).is_err());
        assert_eq!(snapshot(&other),clear);
    }
}
