//! Kimi Coding usage only. Never probes Moonshot or substitutes credentials/UA.
use crate::account::{Account, AccountStore, RelayQuotaWindow, RelayUsageCache};
use chrono::Utc;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tauri::Emitter;

pub fn is_official_coding_url(base: &str) -> bool {
    reqwest::Url::parse(base).ok().is_some_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("api.kimi.com")
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && matches!(u.path().trim_end_matches('/'), "/coding" | "/coding/v1")
    })
}

pub fn is_coding_account(account: &Account) -> bool {
    account.is_relay()
        && match account.relay_usage_preset.as_deref() {
            Some("kimi_coding") => true,
            None | Some("auto") => account
                .relay_base_url
                .as_deref()
                .is_some_and(is_official_coding_url),
            _ => false,
        }
}

pub fn usage_url(base: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(base).map_err(|_| "Invalid Kimi Coding base URL")?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Kimi usage URL must be HTTP(S), without embedded credentials or query".into());
    }
    let path = url.path().trim_end_matches('/');
    let path = if is_official_coding_url(base) && path == "/coding" {
        "/coding/v1/usages".into()
    } else {
        format!("{path}/usages")
    };
    url.set_path(&path);
    Ok(url.to_string())
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
}
fn window(label: &str, value: &Value) -> RelayQuotaWindow {
    let remaining_percent = number(&value["limit"])
        .filter(|v| *v > 0.0)
        .and_then(|limit| {
            number(&value["remaining"])
                .or_else(|| number(&value["used"]).map(|used| limit - used))
                .map(|remaining| (remaining / limit * 100.0).clamp(0.0, 100.0))
        });
    RelayQuotaWindow {
        label: label.into(),
        remaining_percent,
        reset_at: value
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|v| v.timestamp()),
    }
}

pub fn parse(payload: &Value) -> Result<RelayUsageCache, String> {
    if payload.get("error").is_some_and(|v| !v.is_null()) {
        return Err("Kimi usage API returned an error".into());
    }
    let five = payload
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|v| {
            let duration = number(&v["window"]["duration"]).unwrap_or(-1.0);
            let multiplier = match v["window"]["timeUnit"].as_str().unwrap_or("") {
                "TIME_UNIT_MINUTE" => 60.0,
                "TIME_UNIT_HOUR" => 3600.0,
                "TIME_UNIT_SECOND" => 1.0,
                _ => 0.0,
            };
            duration * multiplier == 18000.0
        })
        .map(|v| &v["detail"])
        .unwrap_or(&Value::Null);
    let windows = vec![window("5H", five), window("7D", &payload["usage"])];
    let remaining = windows
        .iter()
        .filter_map(|w| w.remaining_percent)
        .reduce(f64::min)
        .ok_or("Kimi response has no usable quota windows")?;
    let next_reset_at = windows
        .iter()
        .filter(|w| w.remaining_percent == Some(0.0))
        .filter_map(|w| w.reset_at)
        .min();
    Ok(RelayUsageCache {
        remaining,
        unit: "% Kimi Code".into(),
        is_active: windows
            .iter()
            .all(|w| w.remaining_percent.is_some_and(|v| v > 0.0)),
        next_reset_at,
        updated_at: Utc::now(),
        windows,
    })
}

pub async fn fetch(base: &str, key: &str) -> Result<RelayUsageCache, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(usage_url(base)?)
        .bearer_auth(key)
        .header(
            "User-Agent",
            concat!("codex-switcher/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| "Kimi quota connection failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Kimi 编程套餐额度查询失败：HTTP {}",
            response.status().as_u16()
        ));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|_| "Invalid Kimi quota JSON")?;
    parse(&payload)
}

pub async fn refresh_account(
    store: &Arc<Mutex<AccountStore>>,
    id: &str,
) -> Result<RelayUsageCache, String> {
    let (base, key) = {
        let s = store.lock().map_err(|e| e.to_string())?;
        let a = s
            .accounts
            .get(id)
            .filter(|a| is_coding_account(a))
            .ok_or("Not a Kimi Coding quota account")?;
        (
            a.relay_base_url.clone().ok_or("Missing Kimi API URL")?,
            AccountStore::extract_access_token(&a.auth_json).ok_or("Missing Kimi API key")?,
        )
    };
    let cache = fetch(&base, &key).await?;
    let mut s = store.lock().map_err(|e| e.to_string())?;
    let a = s
        .accounts
        .get_mut(id)
        .ok_or("Account removed during quota refresh")?;
    if a.relay_base_url.as_deref() != Some(&base)
        || AccountStore::extract_access_token(&a.auth_json).as_deref() != Some(&key)
        || !is_coding_account(a)
    {
        return Err("Account changed during quota refresh".into());
    }
    // Never write OpenAI cached_quota or change the active account.
    a.relay_usage_cache = Some(cache.clone());
    a.relay_usage_preset = Some("kimi_coding".into());
    s.save()?;
    Ok(cache)
}

pub fn start_refresh(store: Arc<Mutex<AccountStore>>, app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let ids = store
                .lock()
                .map(|s| {
                    s.accounts
                        .values()
                        .filter(|a| is_coding_account(a) && !a.is_logged_out && !a.is_banned)
                        .filter(|a| {
                            a.relay_usage_cache.as_ref().is_none_or(|c| {
                                Utc::now().signed_duration_since(c.updated_at).num_seconds() >= 55
                            })
                        })
                        .map(|a| a.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for id in ids {
                match refresh_account(&store, &id).await {
                    Ok(_) => {
                        let _ = app.emit("accounts-updated", ());
                    }
                    Err(error) => eprintln!("[KimiQuota] {id}: {error}"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn independent_windows_strings_and_zero() {
        let p = json!({"usage":{"limit":"100","used":"58","remaining":"42","resetTime":"2026-09-05T10:16:39Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","used":"100","resetTime":"2026-09-03T07:16:39Z"}}]});
        let c = parse(&p).unwrap();
        assert_eq!(c.windows[0].remaining_percent, Some(0.0));
        assert_eq!(c.windows[1].remaining_percent, Some(42.0));
        assert!(!c.is_active);
        assert_eq!(c.windows[0].reset_at, Some(1788419799));
    }
    #[test]
    fn unknown_is_not_full_or_empty() {
        let c = parse(&json!({"usage":{"limit":100,"used":0}})).unwrap();
        assert_eq!(c.windows[0].remaining_percent, None);
        assert_eq!(c.windows[1].remaining_percent, Some(100.0));
        assert!(parse(&json!({"usage":{"limit":0,"used":0}})).is_err());
        assert!(parse(&json!({})).is_err());
    }
    #[test]
    fn quota_url_and_provider_isolation() {
        assert!(is_official_coding_url("https://api.kimi.com/coding"));
        assert!(!is_official_coding_url("https://api.moonshot.cn/v1"));
        assert!(!is_official_coding_url(
            "https://api.kimi.com.evil.test/coding/v1"
        ));
        assert_eq!(
            usage_url("https://api.kimi.com/coding").unwrap(),
            "https://api.kimi.com/coding/v1/usages"
        );
        assert_eq!(
            usage_url("https://relay.example/custom/v1/").unwrap(),
            "https://relay.example/custom/v1/usages"
        );
    }
    #[tokio::test]
    #[ignore = "explicit read-only live account probe"]
    async fn live_account_quota_probe() {
        let path =
            std::env::var("KIMI_QUOTA_ACCOUNT_FILE").expect("explicit account file required");
        let id = std::env::var("KIMI_QUOTA_ACCOUNT_ID").expect("explicit account id required");
        let root: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let account: Account = serde_json::from_value(root["accounts"][&id].clone()).unwrap();
        assert!(is_coding_account(&account));
        let key = AccountStore::extract_access_token(&account.auth_json).unwrap();
        let result = fetch(account.relay_base_url.as_deref().unwrap(), &key)
            .await
            .unwrap();
        assert_eq!(result.windows.len(), 2);
        println!("{}", serde_json::to_string(&result).unwrap());
    }
}
