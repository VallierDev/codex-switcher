//! Remote Mode — Server 侧 HTTP API 服务器
//!
//! 提供账号 CRUD 和 token 拉取接口，供本机 client 模式调用。
//! 认证：X-Auth-Token 头必须匹配 settings.remote_shared_secret。
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::Emitter;
use tokio::net::TcpListener;

use crate::account::{Account, AccountStore};

type ResponseBody = Full<Bytes>;

/// solo 模式的活跃心跳时间戳（unix seconds）。大于 now 时 Server 侧跳过本地保活，避免
/// 和 solo 客户端双端 refresh 撞 rotate。初始 0 = 无活跃 solo。
fn active_solo_until() -> &'static AtomicI64 {
    static V: OnceLock<AtomicI64> = OnceLock::new();
    V.get_or_init(|| AtomicI64::new(0))
}

/// Server 侧定时任务使用：是否有活跃 solo client？有则应跳过本地保活 / quota 刷新。
pub fn solo_is_active() -> bool {
    active_solo_until().load(Ordering::Relaxed) > chrono::Utc::now().timestamp()
}

/// 把 store.save() 丢到 tokio 的 blocking 线程池，避免 ~200KB JSON 序列化 + fs::write
/// 占着 std::sync::Mutex 卡死 worker pool。
///
/// 背景：之前 client 模式批量 refresh-all（30+ 账号并发）每个 handler 都在 worker
/// 线程里 lock → save → unlock，30 把 lock 串行 + 每次 fs::write 几十毫秒，叠加
/// oauth refresh 无 timeout，最终把 Tokio worker 全部卡死，admin HTTP server 的
/// accept loop 都跑不了 — 表现就是「点刷新无响应、TCP timeout」。
///
/// 这里只把"持久化"动作搬到 blocking 池：调用者在 worker 线程上完成 mutation 后
/// 立刻 drop lock，然后 schedule_save 拿 Arc clone 在另一个线程里 lock+save，
/// 即便 save 那一刻还会短暂持锁，blocking 池被卡住也不会影响 admin server。
pub(crate) fn schedule_save(store: Arc<Mutex<AccountStore>>) {
    tokio::task::spawn_blocking(move || {
        if let Ok(s) = store.lock() {
            if let Err(e) = s.save() {
                eprintln!("[Store] schedule_save 失败: {}", e);
            }
        } else {
            eprintln!("[Store] schedule_save 拿不到锁（poisoned）");
        }
    });
}

struct ApiState {
    store: Arc<Mutex<AccountStore>>,
    secret: String,
    version: String,
    app_handle: tauri::AppHandle,
}

pub fn spawn_remote_server(
    store: Arc<Mutex<AccountStore>>,
    bind: String,
    port: u16,
    secret: String,
    version: String,
    app_handle: tauri::AppHandle,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let ip: std::net::IpAddr = match bind.parse() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[RemoteServer] bind 地址解析失败 ({}): {}", bind, e);
                return;
            }
        };
        let addr = SocketAddr::new(ip, port);
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[RemoteServer] 绑定 {} 失败: {}", addr, e);
                return;
            }
        };
        println!("[RemoteServer] Server HTTP API 已启动: http://{}", addr);

        let state = Arc::new(ApiState {
            store,
            secret,
            version,
            app_handle,
        });

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[RemoteServer] accept 失败: {}", e);
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let state = state.clone();
                    async move { Ok::<_, Infallible>(route(state, req, peer).await) }
                });
                if let Err(e) = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(io, service)
                    .await
                {
                    eprintln!("[RemoteServer] 连接错误: {}", e);
                }
            });
        }
    })
}

async fn route(
    state: Arc<ApiState>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Response<ResponseBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // /health 不需要鉴权
    if path == "/health" && method == Method::GET {
        return handle_health(&state);
    }

    // 其余路径统一鉴权
    if !check_auth(&state, req.headers()) {
        eprintln!("[RemoteServer] {} {} 401 from {}", method, path, peer);
        return json_resp(StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"}));
    }

    // 路由
    if path == "/quotas" && method == Method::GET {
        return handle_list_quota(&state);
    }

    if path == "/skills" && method == Method::GET {
        return handle_list_skills();
    }

    if path == "/skills/upload" && method == Method::POST {
        let query = req.uri().query().unwrap_or("").to_string();
        return handle_upload_skill(req, &query).await;
    }

    if path == "/current" && method == Method::GET {
        return handle_get_current(&state);
    }

    if path == "/switch" && method == Method::POST {
        return handle_switch(&state, req).await;
    }

    if path == "/solo/heartbeat" && method == Method::POST {
        return handle_solo_heartbeat(req).await;
    }

    if path == "/solo/current" && method == Method::POST {
        return handle_solo_current(&state, req).await;
    }

    if path == "/accounts" {
        match method {
            Method::GET => return handle_list(&state),
            Method::POST => return handle_upsert(&state, req).await,
            _ => {}
        }
    }

    // /accounts/:id  /accounts/:id/token
    if let Some(rest) = path.strip_prefix("/accounts/") {
        let mut parts = rest.splitn(2, '/');
        let id = parts.next().unwrap_or("");
        let sub = parts.next();
        if !id.is_empty() {
            match (method.clone(), sub) {
                (Method::GET, Some("token")) => return handle_get_token(&state, id),
                (Method::POST, Some("refresh")) => {
                    return handle_refresh_account(&state, id).await;
                }
                (Method::GET, None) => return handle_get_account(&state, id),
                (Method::DELETE, None) => return handle_delete(&state, id),
                _ => {}
            }
        }
    }

    json_resp(StatusCode::NOT_FOUND, json!({"error": "not found"}))
}

fn check_auth(state: &ApiState, headers: &hyper::HeaderMap) -> bool {
    if state.secret.is_empty() {
        return false; // 未配置密钥时拒绝所有请求（避免误暴露）
    }
    match headers.get("X-Auth-Token").and_then(|v| v.to_str().ok()) {
        Some(v) if v == state.secret => true,
        _ => false,
    }
}

fn handle_health(state: &ApiState) -> Response<ResponseBody> {
    let count = state
        .store
        .lock()
        .map(|s| s.list_accounts().len())
        .unwrap_or(0);
    json_resp(
        StatusCode::OK,
        json!({
            "mode": "server",
            "version": state.version,
            "account_count": count,
        }),
    )
}

fn handle_list(state: &ApiState) -> Response<ResponseBody> {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => return err_resp(format!("锁获取失败: {}", e)),
    };
    let accounts: Vec<Account> = store.list_accounts().into_iter().cloned().collect();
    json_resp(StatusCode::OK, json!({ "accounts": accounts }))
}

fn handle_list_quota(state: &ApiState) -> Response<ResponseBody> {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => return err_resp(format!("锁获取失败: {}", e)),
    };
    let quotas: Vec<Value> = store
        .accounts
        .values()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "cached_quota": a.cached_quota,
                "is_banned": a.is_banned,
                "is_token_invalid": a.is_token_invalid,
                "is_logged_out": a.is_logged_out,
            })
        })
        .collect();
    json_resp(StatusCode::OK, json!({ "quotas": quotas }))
}

fn handle_get_account(state: &ApiState, id: &str) -> Response<ResponseBody> {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => return err_resp(format!("锁获取失败: {}", e)),
    };
    match store.list_accounts().into_iter().find(|a| a.id == id) {
        Some(a) => json_resp(StatusCode::OK, json!({ "account": a })),
        None => json_resp(StatusCode::NOT_FOUND, json!({"error": "account not found"})),
    }
}

fn handle_get_token(state: &ApiState, id: &str) -> Response<ResponseBody> {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => return err_resp(format!("锁获取失败: {}", e)),
    };
    match store.list_accounts().into_iter().find(|a| a.id == id) {
        Some(a) => json_resp(
            StatusCode::OK,
            json!({
                "auth_json": a.auth_json,
                "refresh_token": a.refresh_token,
            }),
        ),
        None => json_resp(StatusCode::NOT_FOUND, json!({"error": "account not found"})),
    }
}

#[derive(Deserialize)]
struct UpsertPayload {
    account: Account,
}

#[derive(Serialize)]
struct UpsertResult {
    ok: bool,
    id: String,
    upserted: &'static str,
    quota_refreshed: bool,
    quota_error: Option<String>,
}

async fn handle_upsert(state: &ApiState, req: Request<Incoming>) -> Response<ResponseBody> {
    let body = match req.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return err_resp(format!("读取 body 失败: {}", e)),
    };
    let payload: UpsertPayload = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("JSON 解析失败: {}", e)}),
            );
        }
    };

    let incoming = payload.account;
    let incoming_id = incoming.id.clone();
    // 去重匹配规则：
    //   1) 按 id 命中 → updated（常规更新）
    //   2) 邮箱相同 + auth_identity_matches（tokens.account_id / openai user id 一致）→ merged
    //      这样同邮箱的 team / plus / free 因 account_id 不同，会被识别为不同账号，不会误合
    //   3) 其它 → created
    let email_key = incoming.name.trim().to_lowercase();
    let (final_id, action): (String, &'static str) = {
        let mut store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => return err_resp(format!("锁获取失败: {}", e)),
        };
        let id_hit = store.accounts.contains_key(&incoming_id);
        let identity_hit: Option<String> = if !id_hit && email_key.contains('@') {
            store
                .list_accounts()
                .into_iter()
                .find(|a| {
                    a.name.trim().to_lowercase() == email_key
                        && AccountStore::auth_identity_matches(&a.auth_json, &incoming.auth_json)
                })
                .map(|a| a.id.clone())
        } else {
            None
        };
        let (final_id, action) = match (id_hit, identity_hit) {
            (true, _) => (incoming_id.clone(), "updated"),
            (false, Some(existing_id)) => (existing_id, "merged"),
            (false, None) => (incoming_id.clone(), "created"),
        };
        let mut to_write = incoming;
        to_write.id = final_id.clone();
        if action == "merged" {
            if let Some(old) = store.accounts.get(&final_id) {
                to_write.created_at = old.created_at.clone();
                if to_write.notes.is_none() {
                    to_write.notes = old.notes.clone();
                }
            }
        }
        if let Err(e) = upsert_account(&mut store, to_write) {
            return err_resp(e);
        }
        if let Err(e) = store.save() {
            return err_resp(format!("保存失败: {}", e));
        }
        (final_id, action)
    };
    let id = final_id;

    // upsert 完成后：服务端主动刷新一次该账号的额度
    let (access_token_opt, account_id, refresh_token) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => {
                let body = UpsertResult {
                    ok: true,
                    id,
                    upserted: action,
                    quota_refreshed: false,
                    quota_error: Some("锁获取失败".to_string()),
                };
                return match serde_json::to_vec(&body) {
                    Ok(v) => resp_with_body(StatusCode::OK, v),
                    Err(e) => err_resp(format!("序列化响应失败: {}", e)),
                };
            }
        };
        match store.accounts.get(&id) {
            Some(a) => (
                AccountStore::extract_access_token(&a.auth_json),
                AccountStore::extract_account_id(&a.auth_json),
                a.refresh_token
                    .clone()
                    .or_else(|| AccountStore::extract_refresh_token(&a.auth_json)),
            ),
            None => (None, None, None),
        }
    };

    let mut quota_refreshed = false;
    let mut quota_error: Option<String> = None;

    // Relay 账号不走 OpenAI usage 接口（必然失败 + 错误地标 is_token_invalid）。
    // Relay 余额由各 client 主动 refresh_relay_usage 拉取并展示，Server 不参与。
    let is_relay = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.accounts.get(&id).map(|a| a.is_relay()))
        .unwrap_or(false);

    let access_token = if is_relay {
        None // 跳过下面的 fetch_usage_direct 分支
    } else {
        match access_token_opt {
            Some(t) => Some(t),
            None => {
                if let Some(ref rt) = refresh_token {
                    match crate::oauth::refresh_access_token_locked(&id, rt).await {
                        Ok(tok) => {
                            let mutated = if let Ok(mut s) = state.store.lock() {
                                if let Some(acc) = s.accounts.get_mut(&id) {
                                    AccountStore::apply_refreshed_tokens(
                                        acc,
                                        tok.access_token.clone(),
                                        tok.refresh_token.clone(),
                                        tok.id_token,
                                        tok.expires_in,
                                    );
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            if mutated {
                                schedule_save(state.store.clone());
                            }
                            Some(tok.access_token)
                        }
                        Err(e) => {
                            quota_error = Some(format!("刷新 token 失败: {}", e));
                            None
                        }
                    }
                } else {
                    quota_error = Some("无 access_token 且无 refresh_token".to_string());
                    None
                }
            }
        }
    };

    if let Some(at) = access_token {
        match crate::usage::UsageFetcher::fetch_usage_direct(at, account_id, refresh_token, true)
            .await
        {
            Ok((usage, _)) => {
                let mutated = if let Ok(mut s) = state.store.lock() {
                    if let Some(acc) = s.accounts.get_mut(&id) {
                        acc.cached_quota = Some(crate::account::CachedQuota {
                            five_hour_left: usage.five_hour_left as f64,
                            five_hour_reset: usage.five_hour_reset.clone(),
                            five_hour_reset_at: usage.five_hour_reset_at,
                            five_hour_label: usage.five_hour_label.clone(),
                            weekly_left: usage.weekly_left as f64,
                            weekly_reset: usage.weekly_reset.clone(),
                            weekly_reset_at: usage.weekly_reset_at,
                            weekly_label: usage.weekly_label.clone(),
                            plan_type: usage.plan_type.clone(),
                            is_valid_for_cli: usage.is_valid_for_cli,
                            updated_at: chrono::Utc::now(),
                        });
                        acc.is_banned = false;
                        acc.is_token_invalid = false;
                        acc.is_logged_out = false;
                        quota_refreshed = true;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if mutated {
                    schedule_save(state.store.clone());
                }
                let _ = state.app_handle.emit("accounts-updated", ());
            }
            Err(e) => {
                let mut mutated = false;
                if e.contains("ACCOUNT_BANNED") {
                    if let Ok(mut s) = state.store.lock() {
                        if let Some(a) = s.accounts.get_mut(&id) {
                            a.is_banned = true;
                            mutated = true;
                        }
                    }
                } else if e.contains("TOKEN_INVALID") {
                    if let Ok(mut s) = state.store.lock() {
                        if let Some(a) = s.accounts.get_mut(&id) {
                            a.is_token_invalid = true;
                            mutated = true;
                        }
                    }
                }
                if mutated {
                    schedule_save(state.store.clone());
                }
                quota_error = Some(e);
            }
        }
    }

    // 不论 quota 刷新是否成功，upsert 本身已落盘；保证 Server UI 也能看到新账号/状态变更
    let _ = state.app_handle.emit("accounts-updated", ());
    crate::tray::update_tray_menu(&state.app_handle);

    let body = UpsertResult {
        ok: true,
        id,
        upserted: action,
        quota_refreshed,
        quota_error,
    };
    match serde_json::to_vec(&body) {
        Ok(v) => resp_with_body(StatusCode::OK, v),
        Err(e) => err_resp(format!("序列化响应失败: {}", e)),
    }
}

/// 直接 upsert 到 accounts HashMap
fn upsert_account(store: &mut AccountStore, incoming: Account) -> Result<(), String> {
    store.accounts.insert(incoming.id.clone(), incoming);
    Ok(())
}

fn handle_delete(state: &ApiState, id: &str) -> Response<ResponseBody> {
    {
        let mut store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => return err_resp(format!("锁获取失败: {}", e)),
        };
        if let Err(e) = store.delete_account(id) {
            return json_resp(StatusCode::BAD_REQUEST, json!({"error": e}));
        }
        if let Err(e) = store.save() {
            return err_resp(format!("保存失败: {}", e));
        }
    }
    // 通知 UI 刷新（client 通过 remote API 触发的变更也需要让 Server 本机 UI 同步）
    let _ = state.app_handle.emit("accounts-updated", ());
    crate::tray::update_tray_menu(&state.app_handle);
    json_resp(StatusCode::OK, json!({"ok": true}))
}

/// 服务端对某个账号执行一次 access_token 刷新 + usage 拉取，
/// 并回写 cached_quota。供 client 模式下本机刷新按钮使用（本机不持 token）。
async fn handle_refresh_account(state: &ApiState, id: &str) -> Response<ResponseBody> {
    let id = id.to_string();

    let (access_token_opt, account_id, refresh_token, is_relay) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => return err_resp(format!("锁获取失败: {}", e)),
        };
        match store.accounts.get(&id) {
            Some(a) => (
                AccountStore::extract_access_token(&a.auth_json),
                AccountStore::extract_account_id(&a.auth_json),
                a.refresh_token
                    .clone()
                    .or_else(|| AccountStore::extract_refresh_token(&a.auth_json)),
                a.is_relay(),
            ),
            None => {
                return json_resp(StatusCode::NOT_FOUND, json!({"error": "account not found"}));
            }
        }
    };

    // Relay 账号：Server 不查 OpenAI usage（会失败 + 误标 token_invalid）
    // Relay 余额由 client 自己 refresh_relay_usage，Server 这里直接返回 ok。
    if is_relay {
        return json_resp(
            StatusCode::OK,
            json!({"ok": true, "skipped": "relay account; client-side refresh_relay_usage"}),
        );
    }

    let access_token = match access_token_opt {
        Some(t) => t,
        None => {
            let Some(rt) = refresh_token.clone() else {
                return json_resp(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "TOKEN_INVALID:无 access_token 且无 refresh_token"}),
                );
            };
            match crate::oauth::refresh_access_token_locked(&id, &rt).await {
                Ok(tok) => {
                    let mutated = if let Ok(mut s) = state.store.lock() {
                        if let Some(acc) = s.accounts.get_mut(&id) {
                            AccountStore::apply_refreshed_tokens(
                                acc,
                                tok.access_token.clone(),
                                tok.refresh_token.clone(),
                                tok.id_token,
                                tok.expires_in,
                            );
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if mutated {
                        schedule_save(state.store.clone());
                    }
                    tok.access_token
                }
                Err(e) => {
                    return json_resp(
                        StatusCode::BAD_REQUEST,
                        json!({"error": format!("TOKEN_INVALID:刷新 token 失败: {}", e)}),
                    );
                }
            }
        }
    };

    match crate::usage::UsageFetcher::fetch_usage_direct(
        access_token,
        account_id,
        refresh_token,
        true,
    )
    .await
    {
        Ok((usage, _)) => {
            let mutated = if let Ok(mut s) = state.store.lock() {
                if let Some(acc) = s.accounts.get_mut(&id) {
                    acc.cached_quota = Some(crate::account::CachedQuota {
                        five_hour_left: usage.five_hour_left as f64,
                        five_hour_reset: usage.five_hour_reset.clone(),
                        five_hour_reset_at: usage.five_hour_reset_at,
                        five_hour_label: usage.five_hour_label.clone(),
                        weekly_left: usage.weekly_left as f64,
                        weekly_reset: usage.weekly_reset.clone(),
                        weekly_reset_at: usage.weekly_reset_at,
                        weekly_label: usage.weekly_label.clone(),
                        plan_type: usage.plan_type.clone(),
                        is_valid_for_cli: usage.is_valid_for_cli,
                        updated_at: chrono::Utc::now(),
                    });
                    acc.is_banned = false;
                    acc.is_token_invalid = false;
                    acc.is_logged_out = false;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if mutated {
                schedule_save(state.store.clone());
            }
            let _ = state.app_handle.emit("accounts-updated", ());
            json_resp(StatusCode::OK, json!({"ok": true, "usage": usage}))
        }
        Err(e) => {
            let mut mutated = false;
            if e.contains("ACCOUNT_BANNED") {
                if let Ok(mut s) = state.store.lock() {
                    if let Some(a) = s.accounts.get_mut(&id) {
                        a.is_banned = true;
                        mutated = true;
                    }
                }
            } else if e.contains("TOKEN_INVALID") {
                if let Ok(mut s) = state.store.lock() {
                    if let Some(a) = s.accounts.get_mut(&id) {
                        a.is_token_invalid = true;
                        mutated = true;
                    }
                }
            } else if e.contains("ACCOUNT_LOGGED_OUT") {
                if let Ok(mut s) = state.store.lock() {
                    if let Some(a) = s.accounts.get_mut(&id) {
                        a.is_logged_out = true;
                        mutated = true;
                    }
                }
            }
            if mutated {
                schedule_save(state.store.clone());
            }
            json_resp(StatusCode::BAD_REQUEST, json!({"error": e}))
        }
    }
}

fn json_resp(status: StatusCode, value: Value) -> Response<ResponseBody> {
    match serde_json::to_vec(&value) {
        Ok(v) => resp_with_body(status, v),
        Err(_) => resp_with_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            b"{\"error\":\"json encode failed\"}".to_vec(),
        ),
    }
}

fn err_resp(msg: String) -> Response<ResponseBody> {
    json_resp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": msg}))
}

fn handle_get_current(state: &ApiState) -> Response<ResponseBody> {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => return err_resp(format!("锁获取失败: {}", e)),
    };
    let current = store.current.clone();
    let (name, quota) = match current.as_ref().and_then(|id| store.accounts.get(id)) {
        Some(a) => (Some(a.name.clone()), a.cached_quota.clone()),
        None => (None, None),
    };
    json_resp(
        StatusCode::OK,
        json!({
            "current": current,
            "name": name,
            "cached_quota": quota,
        }),
    )
}

#[derive(Deserialize)]
struct SwitchPayload {
    from: Option<String>,
    #[serde(default)]
    reason: String,
}

async fn handle_switch(state: &ApiState, req: Request<Incoming>) -> Response<ResponseBody> {
    let body = match req.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return err_resp(format!("读取 body 失败: {}", e)),
    };
    let payload: SwitchPayload = if body.is_empty() {
        SwitchPayload {
            from: None,
            reason: String::new(),
        }
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return json_resp(
                    StatusCode::BAD_REQUEST,
                    json!({"error": format!("JSON 解析失败: {}", e)}),
                );
            }
        }
    };

    let mut store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => return err_resp(format!("锁获取失败: {}", e)),
    };

    let current_now = store.current.clone();
    // CAS：调用方声明的 from 跟 Server 当前不一致 → 说明已经被别人切过了
    if let Some(ref from) = payload.from {
        if current_now.as_deref() != Some(from.as_str()) {
            return json_resp(
                StatusCode::OK,
                json!({
                    "switched": false,
                    "stale": true,
                    "current": current_now,
                    "reason": "already_switched",
                }),
            );
        }
    }

    // 把旧 current 的 5h 标记为耗尽（如果原因是 429/preemptive）
    let reason_lower = payload.reason.to_lowercase();
    let should_mark = reason_lower.contains("429")
        || reason_lower.contains("http")
        || reason_lower.contains("preemptive")
        || reason_lower.contains("quota");
    if should_mark {
        if let Some(ref id) = current_now {
            if let Some(acc) = store.accounts.get_mut(id) {
                if let Some(ref mut q) = acc.cached_quota {
                    q.five_hour_left = 0.0;
                }
            }
        }
    }

    // 选下一个账号
    let candidates = crate::score_candidate_accounts(&store);
    let next_id = candidates.into_iter().find_map(|(id, _, _)| {
        if current_now.as_deref() == Some(id.as_str()) {
            None
        } else {
            Some(id)
        }
    });

    let Some(new_id) = next_id else {
        // 算最早 reset
        let now = chrono::Utc::now().timestamp();
        let mut earliest: Option<i64> = None;
        for a in store.accounts.values() {
            if let Some(q) = &a.cached_quota {
                for r in [q.five_hour_reset_at, q.weekly_reset_at]
                    .into_iter()
                    .flatten()
                {
                    if now < r {
                        earliest = Some(earliest.map_or(r, |e: i64| e.min(r)));
                    }
                }
            }
        }
        return json_resp(
            StatusCode::OK,
            json!({
                "switched": false,
                "exhausted": true,
                "current": current_now,
                "earliest_reset_at": earliest,
            }),
        );
    };

    let from_name = current_now
        .as_ref()
        .and_then(|id| store.accounts.get(id))
        .map(|a| a.name.clone());

    // Server 侧按本机 switch_mode + proxy_enabled 决定热/冷。粗估：proxy_enabled 即视为 running。
    // proxy 在跑时 hot 够用，因为 Server 这台机器上的 codex / Codex App 也走 proxy
    // (OPENAI_BASE_URL = localhost:proxy_port)，永远拿到 store.current 的 token。
    let hot = crate::account::should_hot_switch(&store.settings, store.settings.proxy_enabled);
    if let Err(e) = store.switch_to(&new_id, hot) {
        return err_resp(format!("switch_to 失败: {}", e));
    }
    if let Err(e) = store.save() {
        return err_resp(format!("保存失败: {}", e));
    }
    let to_name = store
        .accounts
        .get(&new_id)
        .map(|a| a.name.clone())
        .unwrap_or_default();

    // 记录 switch_log
    use tauri::Manager;
    let app = state.app_handle.clone();
    if let Some(logger) = app.try_state::<std::sync::Arc<crate::switch_log::SwitchLogger>>() {
        logger.inner().log_switch(
            from_name,
            to_name.clone(),
            crate::switch_log::SwitchReason::Http429,
            None,
            None,
        );
    }
    let _ = app.emit("proxy-account-switched", &to_name);
    let _ = app.emit("accounts-updated", ());

    json_resp(
        StatusCode::OK,
        json!({
            "switched": true,
            "current": new_id,
            "name": to_name,
        }),
    )
}

#[derive(Deserialize)]
struct SoloHeartbeatPayload {
    #[serde(default)]
    ttl_secs: Option<i64>,
}

async fn handle_solo_heartbeat(req: Request<Incoming>) -> Response<ResponseBody> {
    let body = match req.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return err_resp(format!("读取 body 失败: {}", e)),
    };
    let payload: SoloHeartbeatPayload = if body.is_empty() {
        SoloHeartbeatPayload { ttl_secs: None }
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => SoloHeartbeatPayload { ttl_secs: None },
        }
    };
    let ttl = payload
        .ttl_secs
        .unwrap_or(crate::account::SOLO_HEARTBEAT_TTL_SECS)
        .clamp(30, 3600);
    let until = chrono::Utc::now().timestamp() + ttl;
    active_solo_until().store(until, Ordering::Relaxed);
    json_resp(StatusCode::OK, json!({"ok": true, "active_until": until}))
}

#[derive(Deserialize)]
struct SoloCurrentPayload {
    current: String,
    /// 客户端是否要求 Server 也写 disk auth.json（client 模式 = true，
    /// solo 模式 = false 仅归档 current 指针）。老客户端不传时默认 false 保留原行为。
    #[serde(default)]
    apply_to_disk: bool,
}

async fn handle_solo_current(
    state: &Arc<ApiState>,
    req: Request<Incoming>,
) -> Response<ResponseBody> {
    let body = match req.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return err_resp(format!("读取 body 失败: {}", e)),
    };
    let payload: SoloCurrentPayload = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("JSON 解析失败: {}", e)}),
            );
        }
    };
    let new_id = payload.current;
    let apply_to_disk = payload.apply_to_disk;
    let (from_name, to_name) = {
        let mut store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => return err_resp(format!("锁获取失败: {}", e)),
        };
        if !store.accounts.contains_key(&new_id) {
            return json_resp(StatusCode::NOT_FOUND, json!({"error": "account not found"}));
        }
        let from = store
            .current
            .as_ref()
            .and_then(|id| store.accounts.get(id))
            .map(|a| a.name.clone());
        // 两种语义：
        // - apply_to_disk=true（client 模式）：调 switch_to 写 disk，让 Server 这台机器
        //   的 codex 也用同一个号工作（双端对齐）
        // - apply_to_disk=false（solo 模式）：仅更新 current 指针归档，不写 disk，
        //   尊重 Server 那边的独立工作状态
        if apply_to_disk {
            if let Err(e) = store.switch_to(&new_id, false) {
                return err_resp(format!("switch_to 失败: {}", e));
            }
        } else {
            store.current = Some(new_id.clone());
        }
        if let Err(e) = store.save() {
            return err_resp(format!("保存失败: {}", e));
        }
        let to = store
            .accounts
            .get(&new_id)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        (from, to)
    };
    use tauri::Manager;
    let app = state.app_handle.clone();
    if let Some(logger) = app.try_state::<std::sync::Arc<crate::switch_log::SwitchLogger>>() {
        logger.inner().log_switch(
            from_name,
            to_name.clone(),
            crate::switch_log::SwitchReason::Manual,
            None,
            None,
        );
    }
    let _ = app.emit("proxy-account-switched", &to_name);
    let _ = app.emit("accounts-updated", ());
    crate::tray::update_tray_menu(&app);
    json_resp(StatusCode::OK, json!({"ok": true, "current": new_id}))
}

fn handle_list_skills() -> Response<ResponseBody> {
    let names = crate::skills::list_local_skill_dirs();
    json_resp(StatusCode::OK, json!({ "skills": names }))
}

async fn handle_upload_skill(req: Request<Incoming>, query: &str) -> Response<ResponseBody> {
    let name = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("name="))
        .map(|v| percent_decode(v))
        .unwrap_or_default();
    if name.is_empty() {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({"error": "缺少 name 查询参数"}),
        );
    }
    let body = match req.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return err_resp(format!("读取 body 失败: {}", e)),
    };
    match crate::skills::extract_skill_zip(&name, &body) {
        Ok(_) => json_resp(
            StatusCode::OK,
            json!({"ok": true, "name": name, "bytes": body.len()}),
        ),
        Err(e) => json_resp(StatusCode::BAD_REQUEST, json!({"error": e})),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn resp_with_body(status: StatusCode, body: Vec<u8>) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}
