//! ChatGPT Web Session 导入
//!
//! 把 chatgpt.com 网页登录拿到的 session JSON 转成我们内部 auth.json 并注册账号。
//! 移植自 https://github.com/gtxx3600/GPTSession2CPAandSub2API 的 convertSession 逻辑。
//!
//! Web session 没有 refresh_token —— access_token 过期（≈30 天）后账号失效，需要重新导入。
//! 我们走 `refresh_token = None` 这条既有路径，让 oauth refresh 链路自然跳过这个号。
//!
//! id_token 缺失时合成一个 Codex 解析器能吃下的 JWT（header + payload + "synthetic"），
//! claims 包含 chatgpt_account_id / chatgpt_plan_type / email，让 `extract_account_id` /
//! `extract_email` 在 UI / 路由层正常工作。

use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::account::{Account, AccountStore};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImportedSessionInfo {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub account_id: Option<String>,
    pub expires_at: Option<String>,
    pub has_refresh_token: bool,
    pub id_token_synthetic: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImportSessionResult {
    pub ok: Vec<ImportedAccount>,
    pub errors: Vec<ImportError>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImportedAccount {
    pub account: Account,
    pub info: ImportedSessionInfo,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImportError {
    pub source_path: String,
    pub reason: String,
}

/// 入口 Tauri 命令：粘贴的文本可能是单个 session 对象、对象数组，或者深层嵌套的容器
/// （比如某些导出工具会把 sessions 套在 `accounts: [...]` 里）。一律走 walker。
///
/// client / solo 模式下：导入后异步推到 Server（minimac），否则 UI 刷新调
/// `remote_refresh_account_quota` 时 Server 会找不到这个号。
#[tauri::command]
pub fn import_chatgpt_session(
    state: tauri::State<'_, crate::AppState>,
    app: tauri::AppHandle,
    session_json: String,
) -> Result<ImportSessionResult, String> {
    use tauri::Emitter;

    let trimmed = session_json.trim();
    if trimmed.is_empty() {
        return Err("session JSON 不能为空".to_string());
    }
    let parsed: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("JSON 解析失败: {}", e))?;

    let sessions = collect_session_like(&parsed);
    if sessions.is_empty() {
        return Err("未找到包含 accessToken 的 session 对象".to_string());
    }

    let mut ok = Vec::new();
    let mut errors = Vec::new();
    let mut newly_added_ids: Vec<String> = Vec::new();

    let (remote_mode, server_url, server_url_fallback, secret) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        (
            store.settings.remote_mode.clone(),
            store.settings.remote_server_url.clone(),
            store.settings.remote_server_url_fallback.clone(),
            store.settings.remote_shared_secret.clone(),
        )
    };

    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        for (path, value) in sessions {
            match convert_one(&value) {
                Ok((auth_json, info, name)) => {
                    // 找同邮箱 + 同 chatgpt_account_id 的已有账号（同 workspace 才能覆盖；
                    // 不同 workspace 哪怕同邮箱也算独立账号）。account_id 双方都为 None
                    // 时也算匹配，留兜底。
                    let incoming_aid = info.account_id.clone();
                    let now_ts = chrono::Utc::now().timestamp();
                    let same_workspace: Vec<(String, bool)> = store
                        .accounts
                        .values()
                        .filter(|a| a.name == name)
                        .filter(|a| {
                            let existing_aid = AccountStore::extract_account_id(&a.auth_json);
                            existing_aid == incoming_aid
                        })
                        .map(|a| {
                            // "可覆盖"的判定（任一为真即覆盖）：
                            //   1. flag 已标 invalid / logged_out / banned；
                            //   2. AT JWT exp 已过（codex 视角 token 死了，flag 没标只是因为
                            //      没人触发 refresh 或没 rt 没法试）；
                            //   3. **无 refresh_token**：典型 web session 导入账号，本来就
                            //      靠重导续命。强 rt OAuth 账号才走"活号不覆盖"路径。
                            let at_exp = extract_at_jwt_exp(&a.auth_json);
                            let jwt_expired = at_exp.map(|e| e <= now_ts).unwrap_or(false);
                            let no_rt = a.refresh_token.is_none();
                            let dead = a.is_token_invalid
                                || a.is_logged_out
                                || a.is_banned
                                || jwt_expired
                                || no_rt;
                            (a.id.clone(), dead)
                        })
                        .collect();

                    let dead_match = same_workspace.iter().find(|(_, dead)| *dead);
                    let alive_match = same_workspace.iter().find(|(_, dead)| !*dead);

                    match (dead_match, alive_match) {
                        // 同 workspace 死号 → 直接覆盖
                        (Some((existing_id, _)), _) => {
                            let existing_id = existing_id.clone();
                            let new_rt = AccountStore::extract_refresh_token(&auth_json);
                            if let Some(acc) = store.accounts.get_mut(&existing_id) {
                                acc.auth_json = auth_json;
                                acc.refresh_token = new_rt;
                                acc.is_token_invalid = false;
                                acc.is_logged_out = false;
                                acc.cached_quota = None; // plan 可能变了
                                acc.notes = Some(format!(
                                    "re-imported from ChatGPT session at {}",
                                    chrono::Utc::now().to_rfc3339()
                                ));
                                let acc_clone = acc.clone();
                                newly_added_ids.push(existing_id.clone());
                                ok.push(ImportedAccount {
                                    account: acc_clone,
                                    info,
                                });
                                println!(
                                    "[SessionImport] 覆盖死号 {} (id={})",
                                    name, existing_id
                                );
                            }
                        }
                        // 同 workspace 但活号 → 不覆盖
                        (None, Some(_)) => {
                            errors.push(ImportError {
                                source_path: path,
                                reason: format!(
                                    "已存在同 workspace 的活号 {}（跳过，活号不覆盖）",
                                    name
                                ),
                            });
                        }
                        // 不同 workspace 或全新 → add 新账号
                        (None, None) => {
                            let account = store.add_account(
                                name,
                                auth_json,
                                Some("imported from ChatGPT session".to_string()),
                            );
                            newly_added_ids.push(account.id.clone());
                            ok.push(ImportedAccount { account, info });
                        }
                    }
                }
                Err(e) => {
                    errors.push(ImportError {
                        source_path: path,
                        reason: e,
                    });
                }
            }
        }
        if !ok.is_empty() {
            if let Err(e) = store.save() {
                eprintln!("[SessionImport] 保存 store 失败: {}", e);
            }
        }
    }

    crate::tray::update_tray_menu(&app);

    // client / solo 模式：把新导入的账号推到 Server
    if crate::account::pushes_to_server(&remote_mode)
        && !secret.is_empty()
        && !newly_added_ids.is_empty()
    {
        let store_arc = state.store.clone();
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let base = match crate::remote_client::resolve_base_url(&server_url, &server_url_fallback)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[SessionImport] Server 不可达，跳过 push: {}", e);
                    return;
                }
            };
            let mut pushed = 0;
            for id in newly_added_ids {
                let account_clone = {
                    let s = match store_arc.lock() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    match s.accounts.get(&id) {
                        Some(a) => a.clone(),
                        None => continue,
                    }
                };
                match crate::remote_client::upsert_account(&base, &secret, &account_clone).await {
                    Ok(_) => pushed += 1,
                    Err(e) => eprintln!(
                        "[SessionImport] push {} 失败: {}",
                        account_clone.name, e
                    ),
                }
            }
            if pushed > 0 {
                println!("[SessionImport] 导入后已推 {} 个 session 账号到 Server", pushed);
                let _ = app_clone.emit("accounts-updated", ());
            }
        });
    }

    Ok(ImportSessionResult { ok, errors })
}

/// 递归找所有 "看起来像 session" 的对象：必须有 accessToken + 至少一处身份字段。
/// 复刻 JS 版的 collectSessionLikeObjects。
fn collect_session_like(root: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    visit(root, "$", &mut out);
    out
}

fn visit(value: &Value, path: &str, out: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) => {
            let access = first_non_empty_str(value, &["accessToken", "access_token"]).or_else(|| {
                value.get("token").and_then(|t| {
                    first_non_empty_str(t, &["accessToken", "access_token"])
                })
            }).or_else(|| {
                value.get("credentials").and_then(|t| {
                    first_non_empty_str(t, &["accessToken", "access_token"])
                })
            });

            let has_identity = value.get("user").map(|u| u.is_object()).unwrap_or(false)
                || first_non_empty_str(value, &["email", "name", "id"]).is_some()
                || value
                    .get("providerSpecificData")
                    .and_then(|p| {
                        first_non_empty_str(p, &["chatgptAccountId", "chatgpt_account_id"])
                    })
                    .is_some();

            if access.is_some() && has_identity {
                out.push((path.to_string(), value.clone()));
                return;
            }

            for (k, v) in map {
                // 已经看过的 token 字段不必再下钻
                if matches!(k.as_str(), "accessToken" | "access_token" | "sessionToken") {
                    continue;
                }
                visit(v, &format!("{}.{}", path, k), out);
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                visit(v, &format!("{}[{}]", path, i), out);
            }
        }
        _ => {}
    }
}

/// 转一条 session → (auth.json, info, name)
fn convert_one(record: &Value) -> Result<(Value, ImportedSessionInfo, String), String> {
    let access_token = first_non_empty_str(
        record,
        &["accessToken", "access_token"],
    )
    .or_else(|| nested_str(record, "token", &["accessToken", "access_token"]))
    .or_else(|| nested_str(record, "credentials", &["accessToken", "access_token"]))
    .ok_or_else(|| "缺少 accessToken".to_string())?;

    let session_token = first_non_empty_str(record, &["sessionToken", "session_token"])
        .or_else(|| nested_str(record, "token", &["sessionToken", "session_token"]))
        .or_else(|| nested_str(record, "credentials", &["session_token"]));
    let refresh_token = first_non_empty_str(record, &["refreshToken", "refresh_token"])
        .or_else(|| nested_str(record, "token", &["refreshToken", "refresh_token"]))
        .or_else(|| nested_str(record, "credentials", &["refresh_token"]));
    let input_id_token = first_non_empty_str(record, &["idToken", "id_token"])
        .or_else(|| nested_str(record, "token", &["idToken", "id_token"]))
        .or_else(|| nested_str(record, "credentials", &["id_token"]));

    let at_payload = parse_jwt_payload(&access_token);
    let id_payload = input_id_token.as_deref().and_then(parse_jwt_payload);
    let at_auth = openai_auth_section(at_payload.as_ref());
    let id_auth = openai_auth_section(id_payload.as_ref());
    let profile = openai_profile_section(at_payload.as_ref());

    let expires_at = at_payload
        .as_ref()
        .and_then(|p| p.get("exp"))
        .and_then(|v| v.as_i64())
        .and_then(|exp| chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0))
        .map(|dt| dt.to_rfc3339())
        .or_else(|| {
            ["expires", "expiresAt", "expired", "expires_at"]
                .iter()
                .find_map(|k| {
                    record
                        .get(k)
                        .and_then(|v| v.as_str())
                        .and_then(parse_to_iso)
                })
        });

    let email = nested_str(record, "user", &["email"])
        .or_else(|| first_non_empty_str(record, &["email"]))
        .or_else(|| nested_str(record, "credentials", &["email"]))
        .or_else(|| nested_str(record, "providerSpecificData", &["email"]))
        .or_else(|| profile.get("email").and_then(|v| v.as_str()).map(String::from))
        .or_else(|| {
            id_payload
                .as_ref()
                .and_then(|p| p.get("email"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            at_payload
                .as_ref()
                .and_then(|p| p.get("email"))
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let account_id = nested_str(record, "account", &["id"])
        .or_else(|| first_non_empty_str(record, &["account_id", "chatgptAccountId"]))
        .or_else(|| {
            nested_str(
                record,
                "providerSpecificData",
                &["chatgptAccountId", "chatgpt_account_id"],
            )
        })
        .or_else(|| {
            at_auth
                .get("chatgpt_account_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            id_auth
                .get("chatgpt_account_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let user_id = nested_str(record, "user", &["id"])
        .or_else(|| first_non_empty_str(record, &["user_id", "chatgptUserId"]))
        .or_else(|| {
            nested_str(
                record,
                "providerSpecificData",
                &["chatgptUserId", "chatgpt_user_id"],
            )
        })
        .or_else(|| {
            at_auth
                .get("chatgpt_user_id")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let plan_type = nested_str(record, "account", &["planType", "plan_type"])
        .or_else(|| first_non_empty_str(record, &["planType", "plan_type"]))
        .or_else(|| {
            nested_str(
                record,
                "providerSpecificData",
                &["chatgptPlanType", "chatgpt_plan_type"],
            )
        })
        .or_else(|| {
            at_auth
                .get("chatgpt_plan_type")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let synthetic_id_token = if input_id_token.is_none() {
        account_id.as_ref().map(|aid| {
            build_synthetic_id_token(
                email.as_deref(),
                aid,
                plan_type.as_deref(),
                user_id.as_deref(),
                expires_at.as_deref(),
            )
        })
    } else {
        None
    };
    let id_token = input_id_token.clone().or(synthetic_id_token.clone());

    // 用 expires - 1h 作为 last_refresh，跟 AxonHub 输出对齐；没 expires 就拿现在。
    let last_refresh = expires_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| (dt - chrono::Duration::hours(1)).to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    let mut tokens = serde_json::Map::new();
    tokens.insert("access_token".to_string(), json!(access_token));
    // refresh_token 没有就存空 string，extract_refresh_token 会 filter empty → None
    tokens.insert(
        "refresh_token".to_string(),
        json!(refresh_token.clone().unwrap_or_default()),
    );
    if let Some(it) = &id_token {
        tokens.insert("id_token".to_string(), json!(it));
    }
    if let Some(aid) = &account_id {
        tokens.insert("account_id".to_string(), json!(aid));
    }
    if let Some(st) = &session_token {
        tokens.insert("session_token".to_string(), json!(st));
    }
    if let Some(exp) = &expires_at {
        tokens.insert("expires_at".to_string(), json!(exp));
    }

    let mut auth_json = serde_json::Map::new();
    auth_json.insert("tokens".to_string(), Value::Object(tokens));
    auth_json.insert("last_refresh".to_string(), json!(last_refresh));
    auth_json.insert("auth_mode".to_string(), json!("chatgpt"));
    auth_json.insert("source".to_string(), json!("chatgpt_web_session"));
    if refresh_token.is_none() {
        auth_json.insert(
            "session_import_no_refresh".to_string(),
            json!(true),
        );
    }

    let name = email
        .clone()
        .or_else(|| account_id.clone())
        .unwrap_or_else(|| "ChatGPT Session".to_string());

    let info = ImportedSessionInfo {
        email: email.clone(),
        plan_type: plan_type.clone(),
        account_id: account_id.clone(),
        expires_at: expires_at.clone(),
        has_refresh_token: refresh_token.is_some(),
        id_token_synthetic: synthetic_id_token.is_some(),
    };

    Ok((Value::Object(auth_json), info, name))
}

fn build_synthetic_id_token(
    email: Option<&str>,
    account_id: &str,
    plan_type: Option<&str>,
    user_id: Option<&str>,
    expires_at: Option<&str>,
) -> String {
    let now = chrono::Utc::now().timestamp();
    let exp = expires_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(now + 90 * 24 * 60 * 60);

    let mut auth_info = serde_json::Map::new();
    auth_info.insert("chatgpt_account_id".to_string(), json!(account_id));
    if let Some(p) = plan_type {
        auth_info.insert("chatgpt_plan_type".to_string(), json!(p));
    }
    if let Some(u) = user_id {
        auth_info.insert("chatgpt_user_id".to_string(), json!(u));
        auth_info.insert("user_id".to_string(), json!(u));
    }

    let mut payload = serde_json::Map::new();
    payload.insert("iat".to_string(), json!(now));
    payload.insert("exp".to_string(), json!(exp));
    payload.insert(
        "https://api.openai.com/auth".to_string(),
        Value::Object(auth_info),
    );
    if let Some(e) = email {
        payload.insert("email".to_string(), json!(e));
    }

    let header = json!({ "alg": "none", "typ": "JWT", "cpa_synthetic": true });
    let header_b64 = b64url(&serde_json::to_vec(&header).unwrap_or_default());
    let payload_b64 = b64url(&serde_json::to_vec(&Value::Object(payload)).unwrap_or_default());
    format!("{}.{}.synthetic", header_b64, payload_b64)
}

fn b64url(bytes: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn parse_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let raw = general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn openai_auth_section(payload: Option<&Value>) -> serde_json::Map<String, Value> {
    payload
        .and_then(|p| p.get("https://api.openai.com/auth"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

fn openai_profile_section(payload: Option<&Value>) -> serde_json::Map<String, Value> {
    payload
        .and_then(|p| p.get("https://api.openai.com/profile"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

fn first_non_empty_str(obj: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = obj.get(k).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn nested_str(obj: &Value, parent: &str, keys: &[&str]) -> Option<String> {
    obj.get(parent).and_then(|p| first_non_empty_str(p, keys))
}

/// 从账号 auth_json 里解 access_token JWT 的 `exp` claim（unix 秒）。
/// 失败/无 token 返回 None — 调用方按"无信息"处理（保守地继续视为活号）。
fn extract_at_jwt_exp(auth_json: &Value) -> Option<i64> {
    use base64::Engine;
    let at = auth_json
        .get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())?;
    let payload = at.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp").and_then(|v| v.as_i64())
}

fn parse_to_iso(s: &str) -> Option<String> {
    // 已经是 ISO 就原样
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
    }
    // 容错：try Date.parse 风格
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Some(naive.and_utc().to_rfc3339());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_minimal_chatgpt_session() {
        let raw = serde_json::json!({
            "user": {"id": "user-test", "email": "mark@example.com"},
            "expires": "2026-08-06T14:29:36.155Z",
            "account": {"id": "00000000-0000-4000-9000-000000000000", "planType": "plus"},
            "accessToken": "access-token",
            "sessionToken": "session-token",
        });
        let (auth, info, name) = convert_one(&raw).unwrap();
        assert_eq!(name, "mark@example.com");
        assert_eq!(info.email.as_deref(), Some("mark@example.com"));
        assert_eq!(info.plan_type.as_deref(), Some("plus"));
        assert_eq!(
            info.account_id.as_deref(),
            Some("00000000-0000-4000-9000-000000000000")
        );
        assert!(info.id_token_synthetic);
        assert!(!info.has_refresh_token);

        // auth.json 结构
        let tokens = auth.get("tokens").unwrap();
        assert_eq!(tokens.get("access_token").unwrap(), "access-token");
        assert_eq!(tokens.get("refresh_token").unwrap(), "");
        assert_eq!(tokens.get("session_token").unwrap(), "session-token");
        // 合成的 id_token 三段式
        let it = tokens.get("id_token").unwrap().as_str().unwrap();
        let parts: Vec<&str> = it.split('.').collect();
        assert_eq!(parts.len(), 3);

        // payload 能 base64-decode 出 chatgpt_account_id + email
        let payload: Value = serde_json::from_slice(
            &general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).unwrap(),
        )
        .unwrap();
        assert_eq!(payload.get("email").unwrap(), "mark@example.com");
        assert_eq!(
            payload
                .get("https://api.openai.com/auth")
                .unwrap()
                .get("chatgpt_account_id")
                .unwrap(),
            "00000000-0000-4000-9000-000000000000"
        );
    }

    #[test]
    fn keep_real_id_and_refresh_token_when_present() {
        let raw = serde_json::json!({
            "user": {"email": "x@y.com"},
            "account": {"id": "aid"},
            "accessToken": "at",
            "refreshToken": "rt",
            "idToken": "real.header.signature",
        });
        let (auth, info, _) = convert_one(&raw).unwrap();
        assert!(info.has_refresh_token);
        assert!(!info.id_token_synthetic);
        assert_eq!(
            auth.get("tokens").unwrap().get("refresh_token").unwrap(),
            "rt"
        );
        assert_eq!(
            auth.get("tokens").unwrap().get("id_token").unwrap(),
            "real.header.signature"
        );
    }

    #[test]
    fn reject_missing_access_token() {
        let raw = serde_json::json!({"user": {"email": "x@y.com"}});
        // walker 不会把它当 session（缺 accessToken），所以 collect 是空
        let collected = collect_session_like(&raw);
        assert!(collected.is_empty());
    }

    #[test]
    fn collect_session_from_array_of_records() {
        let raw = serde_json::json!([
            {"accessToken": "a", "user": {"email": "a@a"}},
            {"accessToken": "b", "user": {"email": "b@b"}},
        ]);
        let collected = collect_session_like(&raw);
        assert_eq!(collected.len(), 2);
    }
}
