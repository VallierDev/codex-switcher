//! Codex Switcher - 本地 HTTP/WebSocket 代理服务器
//!
//! 透明代理：拦截 Codex CLI/App 请求，动态注入当前账号 Token 并转发。
//! HTTP: Header 转发逻辑与官方 responses-api-proxy 一致
//! WebSocket: 双向桥接，支持 Codex App 的 WebSocket 通信
//!
//! 功能：SSE 流式转发 | WebSocket 透传 | 429 自动切号 | 封号检测 | 评分选号

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use reqwest::Client;
use tauri::Emitter;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tungstenite::client::IntoClientRequest;

use crate::account::AccountStore;
use crate::session_affinity::SessionAffinity;
use crate::switch_log::{SwitchLogger, SwitchReason};
use crate::token_tracker::TokenTracker;

/// 待注入的切号通知消息
static PENDING_INJECT_MSG: std::sync::LazyLock<Mutex<Option<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// 全局 store 引用 —— 让 SSE 流转换器（无 ProxyState 入参）也能读 settings.
/// 由 `proxy::start` 初始化；其它入口点（test 等）允许 None。
static GLOBAL_STORE: std::sync::OnceLock<Arc<Mutex<AccountStore>>> = std::sync::OnceLock::new();

/// 读取压缩开关 + 阈值；store 未初始化时按"开 + 8KB"兜底
fn compress_settings_snapshot() -> (bool, usize) {
    if let Some(store) = GLOBAL_STORE.get() {
        if let Ok(s) = store.lock() {
            return (
                s.settings.output_compression_enabled,
                s.settings.output_compression_threshold_bytes,
            );
        }
    }
    (true, 8192)
}

/// client 模式下，per-account 的 token 短期缓存（避免每次请求都 round-trip Server）
struct RemoteTokenCacheEntry {
    token: String,
    is_chatgpt: bool,
    at: std::time::Instant,
}

const REMOTE_TOKEN_CACHE_TTL_SECS: u64 = 30;

static REMOTE_TOKEN_CACHE: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, RemoteTokenCacheEntry>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn remote_token_cache_get(id: &str) -> Option<(String, bool)> {
    let g = REMOTE_TOKEN_CACHE.lock().ok()?;
    let e = g.get(id)?;
    if e.at.elapsed() < std::time::Duration::from_secs(REMOTE_TOKEN_CACHE_TTL_SECS) {
        Some((e.token.clone(), e.is_chatgpt))
    } else {
        None
    }
}

fn remote_token_cache_put(id: &str, token: &str, is_chatgpt: bool) {
    if let Ok(mut g) = REMOTE_TOKEN_CACHE.lock() {
        g.insert(
            id.to_string(),
            RemoteTokenCacheEntry {
                token: token.to_string(),
                is_chatgpt,
                at: std::time::Instant::now(),
            },
        );
    }
}

/// 401 静默刷新的统一返回。
enum SilentRefreshOutcome {
    /// 拿到新 access_token，调用方应用它重试上游
    Refreshed(String),
    /// auth0 拒绝（RT 已轮换 / 用户登出 / 切号到别处）→ 调用方应该切号
    LoggedOut,
    /// 其他错误（网络抖动等），调用方按原路径返回 401
    OtherError(String),
    /// 当前账号根本没 refresh_token，没法刷
    NoRefreshToken,
}

/// 按 remote_mode 决定刷新路径：
/// - **client / solo**：先尝试问 Server 拿 fresh token（Server 是 RT 轮换的权威），
///   Server 不可达再降级本地 oauth refresh
/// - **off / server**：直接本地 oauth refresh
///
/// 成功后：apply 到 store + 写盘 + 写 ~/.codex/auth.json，把 RT 竞态窗口压到几毫秒。
async fn silent_refresh_current(state: &ProxyState) -> SilentRefreshOutcome {
    let (current_id, remote_mode, primary, fallback, secret) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return SilentRefreshOutcome::OtherError("store lock 失败".into()),
        };
        let Some(cid) = store.current.clone() else {
            return SilentRefreshOutcome::NoRefreshToken;
        };
        (
            cid,
            store.settings.remote_mode.clone(),
            store.settings.remote_server_url.clone(),
            store.settings.remote_server_url_fallback.clone(),
            store.settings.remote_shared_secret.clone(),
        )
    };

    // 1) 优先走 Server（client/solo 模式）
    if matches!(remote_mode.as_str(), "client" | "solo") && !secret.is_empty() {
        match crate::remote_client::resolve_base_url(&primary, &fallback).await {
            Ok(base) => {
                match crate::remote_client::fetch_token(&base, &secret, &current_id).await {
                    Ok(t) => {
                        // 把 Server 的 auth_json 应用到本机 store + 写盘 auth.json
                        let token_str = AccountStore::extract_access_token(&t.auth_json);
                        if let Ok(mut store) = state.store.lock() {
                            store.sync_account_from_auth_json(&current_id, t.auth_json.clone());
                            let _ = store.save();
                        }
                        if let Err(e) = AccountStore::write_codex_auth_extended_expiry(&t.auth_json) {
                            eprintln!("[Proxy] Server 拉到 token 后写 auth.json 失败: {}", e);
                        }
                        invalidate_remote_token_cache();
                        if let Some(tok) = token_str {
                            println!("[Proxy] 通过 Server 刷新成功（{}），重试请求", remote_mode);
                            return SilentRefreshOutcome::Refreshed(tok);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[Proxy] Server fetch_token 失败 ({})：{}，降级本地 refresh",
                            remote_mode, e
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[Proxy] Server 不可达 ({})：{}，降级本地 refresh",
                    remote_mode, e
                );
            }
        }
    }

    // 2) 本地 oauth refresh（off/server 模式 或 上面 Server 路径失败的降级）
    let rt = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(_) => return SilentRefreshOutcome::OtherError("store lock 失败".into()),
        };
        match store
            .accounts
            .get(&current_id)
            .and_then(|a| a.refresh_token.clone())
        {
            Some(rt) => rt,
            None => return SilentRefreshOutcome::NoRefreshToken,
        }
    };

    match crate::oauth::refresh_access_token(&rt).await {
        Ok(new_tokens) => {
            // apply 到 store
            let updated_auth = if let Ok(mut store) = state.store.lock() {
                if let Some(acc) = store.accounts.get_mut(&current_id) {
                    AccountStore::apply_refreshed_tokens(
                        acc,
                        new_tokens.access_token.clone(),
                        new_tokens.refresh_token.clone(),
                        new_tokens.id_token.clone(),
                        new_tokens.expires_in,
                    );
                    let auth = acc.auth_json.clone();
                    let _ = store.save();
                    Some(auth)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(auth) = updated_auth {
                if let Err(e) = AccountStore::write_codex_auth(&auth) {
                    eprintln!("[Proxy] 本地刷新后写 auth.json 失败: {}", e);
                } else {
                    println!("[Proxy] 本地 refresh 成功，已同步 auth.json");
                }
            }
            SilentRefreshOutcome::Refreshed(new_tokens.access_token)
        }
        Err(e) => {
            let lower = e.to_lowercase();
            if lower.contains("logged out")
                || lower.contains("invalid_grant")
                || lower.contains("signed in to another account")
            {
                SilentRefreshOutcome::LoggedOut
            } else {
                SilentRefreshOutcome::OtherError(e)
            }
        }
    }
}

/// 切号或账号变化时手动失效（被 perform_switch 调用）
pub fn invalidate_remote_token_cache() {
    if let Ok(mut g) = REMOTE_TOKEN_CACHE.lock() {
        g.clear();
    }
}

/// ChatGPT OAuth 登录用的上游（免费/Plus/Team 账号）
const CHATGPT_HOST: &str = "chatgpt.com";
const CHATGPT_ORIGIN: &str = "https://chatgpt.com/backend-api/codex";

/// API key 用的上游
const API_HOST: &str = "api.openai.com";
const API_ORIGIN: &str = "https://api.openai.com";
const MAX_429_RETRIES: usize = 5;

/// 统一的响应 Body 类型：支持 Full（错误/小响应）和 Stream（SSE 流式）。
/// 用 UnsyncBoxBody 而非 BoxBody —— hyper 的 service 单 task 处理一个连接，不要求 Sync；
/// reqwest 的 bytes_stream 也不保证 Sync，强求 Sync 会触发 trait bound 错误。
type ProxyBody = UnsyncBoxBody<Bytes, String>;

/// 代理运行指标（与 AppState 共享）
pub struct ProxyStats {
    pub total_requests: AtomicU64,
    pub auto_switches: AtomicU64,
}

impl Default for ProxyStats {
    fn default() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            auto_switches: AtomicU64::new(0),
        }
    }
}

/// 代理运行时共享状态
struct ProxyState {
    store: Arc<Mutex<AccountStore>>,
    client: Client,
    app_handle: tauri::AppHandle,
    switching: AtomicBool,
    stats: Arc<ProxyStats>,
    tracker: Arc<TokenTracker>,
    /// 切号时通知 WebSocket 断开
    ws_disconnect: Arc<tokio::sync::Notify>,
    switch_logger: Arc<SwitchLogger>,
    session_affinity: Arc<SessionAffinity>,
}

/// 启动代理服务器
pub fn start(
    store: Arc<Mutex<AccountStore>>,
    port: u16,
    allow_lan: bool,
    app_handle: tauri::AppHandle,
    stats: Arc<ProxyStats>,
    tracker: Arc<TokenTracker>,
    ws_disconnect: Arc<tokio::sync::Notify>,
    switch_logger: Arc<SwitchLogger>,
    session_affinity: Arc<SessionAffinity>,
) -> tauri::async_runtime::JoinHandle<()> {
    // 把 store 注册到全局，供 SSE 压缩流转换器等无 ProxyState 入参的工具函数读取。
    let _ = GLOBAL_STORE.set(store.clone());
    tauri::async_runtime::spawn(async move {
        let addr = if allow_lan {
            SocketAddr::from(([0, 0, 0, 0], port))
        } else {
            SocketAddr::from(([127, 0, 0, 1], port))
        };
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[Proxy] 绑定端口 {} 失败: {}", port, e);
                return;
            }
        };

        println!("[Proxy] 代理服务器已启动，监听 {}:{}", addr.ip(), port);

        let client = Client::builder()
            .build()
            .expect("[Proxy] 构建 reqwest Client 失败");

        let state = Arc::new(ProxyState {
            store,
            client,
            app_handle,
            switching: AtomicBool::new(false),
            stats,
            tracker,
            ws_disconnect,
            switch_logger,
            session_affinity,
        });

        loop {
            let (stream, peer_addr) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Proxy] accept 失败: {}", e);
                    continue;
                }
            };

            let state = state.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let state = state.clone();
                    handle_request(state, req)
                });

                if let Err(e) = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await
                {
                    if !e.is_incomplete_message() {
                        eprintln!("[Proxy] 连接 {} 错误: {}", peer_addr, e);
                    }
                }
            });
        }
    })
}

// ────────────────────────────────────────────────────────────────
// Token 管理
// ────────────────────────────────────────────────────────────────

/// 获取当前账号最新的 access_token + 认证模式
///
/// 默认：从本地 store + ~/.codex/auth.json 回读最新值。
/// client 模式：从 Server 拉取新鲜 token，回写本地 store；失败则回退本地。
///
/// 返回 (token, is_chatgpt_auth)
async fn get_current_token(state: &ProxyState) -> Result<(String, bool), String> {
    // 1) 从 store 取一小段快照，尽快释放锁
    let (current_id, remote_mode, primary, fallback, secret) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let id = store.current.as_ref().ok_or("没有激活的账号")?.clone();
        (
            id,
            store.settings.remote_mode.clone(),
            store.settings.remote_server_url.clone(),
            store.settings.remote_server_url_fallback.clone(),
            store.settings.remote_shared_secret.clone(),
        )
    };

    // 2) client 模式：优先命中本地短缓存；miss 时去 Server 拿新鲜 token
    if remote_mode == "client" && !secret.is_empty() {
        if let Some((tok, is_chatgpt)) = remote_token_cache_get(&current_id) {
            return Ok((tok, is_chatgpt));
        }
        match crate::remote_client::resolve_base_url(&primary, &fallback).await {
            Ok(base) => {
                match crate::remote_client::fetch_token(&base, &secret, &current_id).await {
                    Ok(t) => {
                        // 回写本地 store，方便 UI 显示一致、quota 等字段也能看到
                        if let Ok(mut store) = state.store.lock() {
                            store.sync_account_from_auth_json(&current_id, t.auth_json.clone());
                            let _ = store.save();
                        }
                        // 用 Server 的新鲜 auth_json 强制覆写本机 ~/.codex/auth.json
                        // 目的：让本机 Codex CLI 永远读到新鲜 access_token，避免它自己触发 oauth refresh
                        // 使 refresh_token 在两端分叉。
                        // 用 extended_expiry 版本：把 expires_at 顶到 +24h，codex 永远不会主动 refresh。
                        if let Err(e) = AccountStore::write_codex_auth_extended_expiry(&t.auth_json) {
                            eprintln!("[Proxy] 写 ~/.codex/auth.json 失败: {}", e);
                        }
                        if let Some(tok) = AccountStore::extract_access_token(&t.auth_json) {
                            let is_chatgpt = tok.starts_with("eyJ");
                            remote_token_cache_put(&current_id, &tok, is_chatgpt);
                            return Ok((tok, is_chatgpt));
                        }
                        eprintln!("[Proxy] Server 返回的 auth_json 里没有 access_token");
                    }
                    Err(e) => eprintln!("[Proxy] client 模式 fetch_token 失败，回退本地: {}", e),
                }
            }
            Err(e) => eprintln!("[Proxy] client 模式 Server 不可达，回退本地: {}", e),
        }
    }

    // 3) 默认路径：本地 store + ~/.codex/auth.json
    let mut store = state.store.lock().map_err(|e| e.to_string())?;
    // Relay 账号不与 disk auth.json 同步（disk 是 OAuth 身份，Relay 没 uid，
    // 每次 sync 都会被 sync_account_from_auth_json_inner 的身份校验拒绝并刷一行 log）
    let current_is_relay = store
        .accounts
        .get(&current_id)
        .map(|a| a.is_relay())
        .unwrap_or(false);
    if !current_is_relay {
        if let Ok(disk_auth) = AccountStore::read_codex_auth() {
            if store.sync_account_from_auth_json(&current_id, disk_auth) {
                let _ = store.save();
            }
        }
    }
    let account = store.accounts.get(&current_id).ok_or("当前账号不存在")?;
    let token = AccountStore::extract_access_token(&account.auth_json)
        .ok_or_else(|| "当前账号缺少 access_token".to_string())?;
    let is_chatgpt = token.starts_with("eyJ");
    Ok((token, is_chatgpt))
}

/// 优先按 session affinity 找一个健康的绑定账号；若 binding 还在并指向健康号 → 用它的 token；
/// 否则落回 current。**不修改 store.current**，纯本次请求级别的 token override。
/// 返回 (token, is_chatgpt, account_id_used) —— account_id 给 end_signal 记账用
async fn resolve_token_with_affinity(
    state: &ProxyState,
    session_key: Option<&str>,
) -> Result<(String, bool, Option<String>), String> {
    let Some(sk) = session_key else {
        let (tok, is_cgpt) = get_current_token(state).await?;
        let cur = state.store.lock().ok().and_then(|s| s.current.clone());
        return Ok((tok, is_cgpt, cur));
    };

    // 1) 先看绑定的号是否健康（不依赖 quota，因为 cached_quota 可能滞后；只看 banned/logged_out/token_invalid）
    // 此外：当 current 不是 Relay 但 binding 指向 Relay 时，按 relay_auto_switch_in 决定是否
    // 用这条 binding——默认 false 即"不切到 Relay"，避免 affinity 把订阅号会话偷偷拉回 Relay 扣余额。
    let bound_account = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let cur = store.current.clone();
        let cur_is_relay = cur
            .as_deref()
            .and_then(|id| store.accounts.get(id))
            .map(|a| a.is_relay())
            .unwrap_or(false);
        let allow_in = store.settings.relay_auto_switch_in;
        let bid = state.session_affinity.lookup(sk, |id| {
            store
                .accounts
                .get(id)
                .map(|a| !a.is_banned && !a.is_logged_out && !a.is_token_invalid)
                .unwrap_or(false)
        });
        match bid {
            Some(id) if Some(&id) != cur.as_ref() => {
                let bound_is_relay = store
                    .accounts
                    .get(&id)
                    .map(|a| a.is_relay())
                    .unwrap_or(false);
                if !allow_in && !cur_is_relay && bound_is_relay {
                    println!(
                        "[Proxy] affinity binding 指向 Relay 但 current 不是 Relay 且 relay_auto_switch_in=false，忽略 binding"
                    );
                    None
                } else {
                    Some(id)
                }
            }
            _ => None,
        }
    };

    if let Some(account_id) = bound_account {
        let token = {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            let acc = store
                .accounts
                .get(&account_id)
                .ok_or_else(|| "session 绑定账号不存在".to_string())?;
            AccountStore::extract_access_token(&acc.auth_json)
                .ok_or_else(|| "session 绑定账号缺 access_token".to_string())?
        };
        let is_chatgpt = token.starts_with("eyJ");
        println!("[Proxy] Session affinity hit: {} → {}", sk, account_id);
        return Ok((token, is_chatgpt, Some(account_id)));
    }

    let (tok, is_cgpt) = get_current_token(state).await?;
    let cur = state.store.lock().ok().and_then(|s| s.current.clone());
    Ok((tok, is_cgpt, cur))
}

/// 根据账号类型选定上游 URL 和 Host header。
///
/// - `is_chatgpt=true` → ChatGPT 订阅那条路径（去掉 `/v1` 前缀）
/// - `is_chatgpt=false` + `relay_base_url=Some(b)` → 中转站 b + 原始 path
/// - `is_chatgpt=false` + `relay_base_url=None` → 官方 api.openai.com + 原始 path
fn get_upstream(
    is_chatgpt: bool,
    relay_base_url: Option<&str>,
    path_and_query: &str,
) -> (String, String) {
    if is_chatgpt {
        // 客户端路径: /v1/responses (因为 OPENAI_BASE_URL 带 /v1)
        // ChatGPT 上游: /backend-api/codex/responses (不含 /v1)
        let path = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
        let url = format!("{}{}", CHATGPT_ORIGIN, path);
        (url, CHATGPT_HOST.to_string())
    } else if let Some(base) = relay_base_url {
        // 中转站: 用账号上的 base_url + 原始 path（保留 /v1 前缀）
        let base = base.trim_end_matches('/');
        let host = url::Url::parse(base)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_else(|| API_HOST.to_string());
        (format!("{}{}", base, path_and_query), host)
    } else {
        // 官方 API key: 转发到 api.openai.com + 原始路径（保留 /v1）
        let url = format!("{}{}", API_ORIGIN, path_and_query);
        (url, API_HOST.to_string())
    }
}

/// 从 store 取指定账号的 relay_base_url（仅 Relay 类型；其它返回 None）。
fn account_relay_base_url(state: &ProxyState, account_id: &str) -> Option<String> {
    let store = state.store.lock().ok()?;
    let acc = store.accounts.get(account_id)?;
    if acc.is_relay() {
        acc.relay_base_url.clone()
    } else {
        None
    }
}

/// 中转站请求路由信息：当 store.current 是 Relay 时返回 Some。
#[derive(Debug, Clone)]
struct RelayRoute {
    #[allow(dead_code)]
    account_id: String,
    model_map: Option<std::collections::HashMap<String, String>>,
    model_fallback: Option<String>,
}

/// 取 store.current 的 Relay 路由信息（仅 Relay 类型；其它 None）。
fn current_relay_route(state: &ProxyState) -> Option<RelayRoute> {
    let store = state.store.lock().ok()?;
    let id = store.current.clone()?;
    let acc = store.accounts.get(&id)?;
    if !acc.is_relay() {
        return None;
    }
    Some(RelayRoute {
        account_id: id,
        model_map: acc.relay_model_map.clone(),
        model_fallback: acc.relay_model_fallback.clone(),
    })
}

/// 若 body 是 JSON 且含 `model` 字段，按 `map` / `fallback` 重写。
///
/// 优先级：map 命中 > fallback；都不命中或值跟原值相等 → 原样返回。
fn rewrite_model_in_body(
    body: &Bytes,
    map: Option<&std::collections::HashMap<String, String>>,
    fallback: Option<&str>,
) -> Bytes {
    if map.map_or(true, |m| m.is_empty()) && fallback.is_none() {
        return body.clone();
    }
    let mut json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(), // 非 JSON 直接透传（流式上传等）
    };
    let original = match json.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return body.clone(),
    };
    let target = map
        .and_then(|m| m.get(&original).cloned())
        .or_else(|| fallback.map(String::from));
    if let Some(t) = target {
        if t != original {
            println!("[Proxy] Relay model rewrite: {} → {}", original, t);
            json["model"] = serde_json::Value::String(t);
            return Bytes::from(serde_json::to_vec(&json).unwrap_or_else(|_| body.to_vec()));
        }
    }
    body.clone()
}

// ────────────────────────────────────────────────────────────────
// 选号算法（复用 lib.rs 共享评分）
// ────────────────────────────────────────────────────────────────

enum PickResult {
    Found { id: String, token: String },
    Exhausted { earliest_reset: Option<i64> },
}

fn pick_next_account(state: &ProxyState) -> PickResult {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(_) => {
            return PickResult::Exhausted {
                earliest_reset: None,
            }
        }
    };

    let candidates = crate::score_candidate_accounts(&store);

    if candidates.is_empty() {
        let now = Utc::now().timestamp();
        let mut earliest: Option<i64> = None;
        for account in store.accounts.values() {
            if let Some(q) = &account.cached_quota {
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
        return PickResult::Exhausted {
            earliest_reset: earliest,
        };
    }

    let (id, _, _) = &candidates[0];
    if let Some(account) = store.accounts.get(id) {
        if let Some(token) = AccountStore::extract_access_token(&account.auth_json) {
            return PickResult::Found {
                id: id.clone(),
                token,
            };
        }
    }

    PickResult::Exhausted {
        earliest_reset: None,
    }
}

// ────────────────────────────────────────────────────────────────
// 预防性切号 / 封号检测 / 切号执行
// ────────────────────────────────────────────────────────────────

fn should_preemptive_switch(state: &ProxyState) -> bool {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let (t5h, tw, fg) = (
        store.settings.proxy_threshold_5h as f64,
        store.settings.proxy_threshold_weekly as f64,
        store.settings.proxy_free_guard as f64,
    );

    if t5h == 0.0 && tw == 0.0 && fg == 0.0 {
        return false;
    }

    let current_id = match &store.current {
        Some(id) => id,
        None => return false,
    };

    let account = match store.accounts.get(current_id) {
        Some(a) => a,
        None => return false,
    };

    // current 是 Relay 时，按设置决定是否允许预切走（401/429/quota 触发）
    if account.is_relay() && !store.settings.relay_auto_switch_out {
        return false;
    }

    if account.is_banned || account.is_token_invalid || account.is_logged_out {
        println!("[Proxy] 发现当前账号被封禁/失效/登出，触发预防性切号");
        return true;
    }

    let quota = match account.cached_quota.as_ref() {
        Some(q) => q,
        None => return false,
    };

    let plan = quota.plan_type.to_lowercase();
    let is_free = plan == "free" || plan == "unknown";

    if is_free && fg > 0.0 && quota.five_hour_left < fg {
        println!(
            "[Proxy] Free 保护线触发: {:.0}% < {:.0}%",
            quota.five_hour_left, fg
        );
        return true;
    }
    if t5h > 0.0 && quota.five_hour_left < t5h {
        println!(
            "[Proxy] 5h 阈值触发: {:.0}% < {:.0}%",
            quota.five_hour_left, t5h
        );
        return true;
    }
    if tw > 0.0 && quota.weekly_left < tw {
        println!("[Proxy] 周阈值触发: {:.0}% < {:.0}%", quota.weekly_left, tw);
        return true;
    }
    false
}

fn mark_current_banned(state: &ProxyState) {
    if let Ok(mut store) = state.store.lock() {
        if let Some(current_id) = store.current.clone() {
            // 顺便作废 session affinity 里指向该号的所有 binding
            state.session_affinity.invalidate_account(&current_id);
            if let Some(account) = store.accounts.get_mut(&current_id) {
                account.is_banned = true;
                let name = account.name.clone();
                let _ = store.save();
                println!("[Proxy] 账号 {} 已标记为封号", name);
                let _ = state.app_handle.emit("proxy-account-banned", &name);
                // macOS 系统通知（可配置）
                if store.settings.notify_on_switch {
                    let notify_name = name.clone();
                    std::thread::spawn(move || {
                        let _ = std::process::Command::new("osascript")
                            .arg("-e")
                            .arg(format!(
                                "display notification \"{}\" with title \"Codex Switcher\" subtitle \"检测到封号\"",
                                notify_name
                            ))
                            .output();
                    });
                }
            }
        }
    }
}

/// 429 后标记当前账号的 5h 额度为耗尽
/// 标记指定账号的 5h 额度为耗尽
fn mark_account_quota_depleted(state: &ProxyState, account_id: &str) {
    state.session_affinity.invalidate_account(account_id);
    if let Ok(mut store) = state.store.lock() {
        if let Some(account) = store.accounts.get_mut(account_id) {
            if let Some(ref mut q) = account.cached_quota {
                q.five_hour_left = 0.0;
            }
            let _ = store.save();
        }
    }
}

fn mark_current_quota_depleted(state: &ProxyState) {
    if let Ok(mut store) = state.store.lock() {
        if let Some(current_id) = store.current.clone() {
            state.session_affinity.invalidate_account(&current_id);
            if let Some(account) = store.accounts.get_mut(&current_id) {
                if let Some(ref mut q) = account.cached_quota {
                    q.five_hour_left = 0.0;
                }
                let _ = store.save();
            }
        }
    }
}

fn do_switch(state: &ProxyState, new_id: &str, reason: SwitchReason) -> Result<(), String> {
    let mut store = state.store.lock().map_err(|e| e.to_string())?;

    // 记录切号前的账号信息
    let from_name = store
        .current
        .as_ref()
        .and_then(|id| store.accounts.get(id))
        .map(|a| a.name.clone());
    let from_quota = store
        .current
        .as_ref()
        .and_then(|id| store.accounts.get(id))
        .and_then(|a| a.cached_quota.as_ref())
        .map(|q| q.five_hour_left);

    // 代理内部切号：按 switch_mode 决定 hot/cold。proxy 在跑时 hot 完全够 ——
    // 因为 codex（CLI / App 内置二进制）走 OPENAI_BASE_URL=proxy，每次请求 proxy
    // 注入 store.current 的 token，codex 永远拿到 200，不触发 UnauthorizedRecovery。
    // disk auth.json 跟 store 不一致只是"UI 显眼"，不影响 codex 实际工作。
    let hot = crate::account::should_hot_switch(&store.settings, true);
    store.switch_to(new_id, hot)?;
    store.save()?;
    // 切号后远端 token 缓存作废，下一次请求重新拉
    invalidate_remote_token_cache();

    let to_name = store
        .accounts
        .get(new_id)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let to_quota = store
        .accounts
        .get(new_id)
        .and_then(|a| a.cached_quota.as_ref())
        .map(|q| q.five_hour_left);

    println!("[Proxy] 自动切号 → {} ({})", to_name, reason);

    // 记录切号日志
    state.switch_logger.log_switch(
        from_name.clone(),
        to_name.clone(),
        reason,
        from_quota,
        to_quota,
    );

    state.stats.auto_switches.fetch_add(1, Ordering::Relaxed);
    state.ws_disconnect.notify_waiters();
    let _ = state.app_handle.emit("proxy-account-switched", &to_name);
    let _ = state.app_handle.emit("accounts-updated", ());

    // 读取通知设置
    let notify_enabled = store.settings.notify_on_switch;
    let inject_enabled = store.settings.inject_switch_message;
    // solo 模式下把 current 同步给 Server（非阻塞）
    let solo_push = if store.settings.remote_mode == "solo"
        && !store.settings.remote_shared_secret.is_empty()
    {
        Some((
            store.settings.remote_server_url.clone(),
            store.settings.remote_server_url_fallback.clone(),
            store.settings.remote_shared_secret.clone(),
            new_id.to_string(),
        ))
    } else {
        None
    };

    drop(store); // 释放锁

    if let Some((primary, fallback, secret, nid)) = solo_push {
        tauri::async_runtime::spawn(async move {
            match crate::remote_client::resolve_base_url(&primary, &fallback).await {
                Ok(base) => {
                    // solo 模式：apply_to_disk=false，Server 仅归档 current 不写盘
                    if let Err(e) = crate::remote_client::push_solo_switch(
                        &base, &secret, &nid, false,
                    )
                    .await
                    {
                        eprintln!("[Solo] 自动切号后 push Server 失败: {}", e);
                    }
                }
                Err(e) => eprintln!("[Solo] Server 不可达，自动切号未同步: {}", e),
            }
        });
    }

    // macOS 系统通知（可配置）
    if notify_enabled {
        let from = from_name.unwrap_or_else(|| "无".to_string());
        let notify_msg = format!("{} → {}", from, to_name);
        std::thread::spawn(move || {
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(format!(
                    "display notification \"{}\" with title \"Codex Switcher\" subtitle \"自动切号\"",
                    notify_msg
                ))
                .output();
        });
    }

    // 注入 WebSocket 消息标记（可配置，实验性）
    if inject_enabled {
        PENDING_INJECT_MSG.lock().ok().map(|mut msg| {
            *msg = Some(format!("⚡ [Codex Switcher] 已切换到 {}", to_name));
        });
    }

    Ok(())
}

// ────────────────────────────────────────────────────────────────
// 核心请求处理
// ────────────────────────────────────────────────────────────────

async fn handle_request(
    state: Arc<ProxyState>,
    req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    state.stats.total_requests.fetch_add(1, Ordering::Relaxed);

    // ── 健康检查 ──
    if req.method() == Method::GET && req.uri().path() == "/health" {
        let total = state.stats.total_requests.load(Ordering::Relaxed);
        let switches = state.stats.auto_switches.load(Ordering::Relaxed);
        let body = serde_json::json!({
            "status": "ok",
            "total_requests": total,
            "auto_switches": switches,
        });
        return Ok(Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(full_body(Bytes::from(body.to_string())))
            .unwrap());
    }

    // ── WebSocket 升级检测 ──
    if is_websocket_upgrade(&req) {
        println!("[Proxy] WebSocket upgrade 请求: {}", req.uri());
        return handle_websocket(state, req).await;
    }

    // ── 读取请求元数据 + body（client/server 分支共用）──
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let req_headers = req.headers().clone();
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            eprintln!("[Proxy] 读取请求体失败: {}", e);
            return Ok(error_response(StatusCode::BAD_REQUEST, "读取请求体失败"));
        }
    };

    // ── Relay 路由前置处理 ──
    // 1) 当 current 是 Relay 时，重写 body 里的 `model` 字段（codex 端发的 gpt-* → glm-*）
    // 2) Relay 不能走 client→Server 转发：Server 不知道这账号，会一路 fall through 浪费时间
    let relay_route = current_relay_route(&state);
    let body_bytes = if let Some(ref r) = relay_route {
        rewrite_model_in_body(&body_bytes, r.model_map.as_ref(), r.model_fallback.as_deref())
    } else {
        body_bytes
    };

    // 提取 session_key：用于 affinity 路由 + cache hit 记账（client 模式 affinity 由 Server 处理）
    let session_key = crate::session_affinity::extract_session_key(&body_bytes, &req_headers);

    // ── client 模式：先转发到 Server；Server 不可达或返回 402 deactivated 时回退本地 ──
    // Relay 账号也走这条 —— Server 端的 codex-switcher 知道 Relay schema，
    // 收到请求后会按自己 store 里的 Relay base_url 转发到 unity2/glm 上游。
    let remote_mode = state
        .store
        .lock()
        .map(|s| s.settings.remote_mode.clone())
        .unwrap_or_default();
    if remote_mode == "client" {
        match forward_to_server_parts(&state, &method, &path_and_query, &req_headers, &body_bytes)
            .await
        {
            Ok(mini_resp) => {
                let status = mini_resp.status();
                if status == reqwest::StatusCode::PAYMENT_REQUIRED {
                    let resp_bytes = mini_resp.bytes().await.unwrap_or_default();
                    let lower = String::from_utf8_lossy(&resp_bytes).to_lowercase();
                    let is_deactivated = lower.contains("deactivated")
                        || lower.contains("account_deactivated")
                        || lower.contains("deactivated_workspace");
                    if is_deactivated {
                        println!("[Proxy] Server 返回 402 deactivated，尝试本地账号回退");
                        if let Some(resp) = try_local_fallback(
                            &state,
                            &method,
                            &path_and_query,
                            &req_headers,
                            &body_bytes,
                            session_key.as_deref(),
                        )
                        .await
                        {
                            return Ok(resp);
                        }
                    }
                    return Ok(Response::builder()
                        .status(402)
                        .header("content-type", "application/json")
                        .body(full_body(resp_bytes))
                        .unwrap_or_else(|_| {
                            error_response(StatusCode::PAYMENT_REQUIRED, "402")
                        }));
                }
                // client 模式：affinity 由 Mini 那侧处理，本机不记录
                return Ok(build_stream_response(mini_resp, None, None));
            }
            Err(e) => {
                eprintln!(
                    "[Proxy] Server 转发失败（{}），fall through 到本地完整 401/429 处理路径",
                    e
                );
                // 不再 early-return：让执行流走到下面非 client 模式的完整逻辑去
                // （含 silent_refresh + try_switch_and_retry + SSE bootstrap）
                // 这样即使 Server 不可达，本机用 store.current 的 token 直连上游被 401 时，
                // 也会自动 refresh / 切号，而不是把 401 透回给 codex。
            }
        }
    }

    // 1. 获取 token —— 优先按 session affinity 选号，其次落回 current
    let (token, is_chatgpt, used_account_id) =
        match resolve_token_with_affinity(&state, session_key.as_deref()).await {
            Ok(t) => t,
            Err(e) => return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &e)),
        };
    let session_affinity_ctx = match (session_key.clone(), used_account_id.clone()) {
        (Some(sk), Some(aid)) => Some(AffinityCtx {
            affinity: state.session_affinity.clone(),
            session_key: sk,
            account_id: aid,
        }),
        _ => None,
    };

    // 2. 根据认证模式路由上游（Relay 类型从 used_account_id 查 base_url）
    let relay_base_url = used_account_id
        .as_deref()
        .and_then(|id| account_relay_base_url(&state, id));
    let (upstream_url, upstream_host) =
        get_upstream(is_chatgpt, relay_base_url.as_deref(), &path_and_query);

    // 3. 透明 Header 转发（官方 responses-api-proxy 逻辑）
    let base_headers = build_upstream_headers(&req_headers, &upstream_host);

    // 5. 首次转发
    let upstream_resp = match forward_with_token(
        &state,
        &method,
        &upstream_url,
        &base_headers,
        &body_bytes,
        &token,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("上游连接失败: {}", e),
            ))
        }
    };

    let status_code = upstream_resp.status();

    // 6. 封号检测（401/402/403）
    //    - 401/403 可能是 token 过期、登出、或封号；body 里有 deactivated 关键词视为封号
    //    - 402 Payment Required（deactivated_workspace）始终视为封号
    if status_code == reqwest::StatusCode::UNAUTHORIZED
        || status_code == reqwest::StatusCode::FORBIDDEN
        || status_code == reqwest::StatusCode::PAYMENT_REQUIRED
    {
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        let body_lower = String::from_utf8_lossy(&resp_bytes).to_lowercase();
        let body_hits_banned = body_lower.contains("deactivated")
            || body_lower.contains("banned")
            || body_lower.contains("suspended")
            || body_lower.contains("account_deactivated")
            || body_lower.contains("deactivated_workspace");
        let is_402 = status_code == reqwest::StatusCode::PAYMENT_REQUIRED;
        let banned = body_hits_banned || is_402;

        if banned {
            println!(
                "[Proxy] 封号检测触发（status={}），标记并切号...",
                status_code
            );
            mark_current_banned(&state);

            if let Some(resp) = try_switch_and_retry(
                &state,
                &method,
                &upstream_url,
                &base_headers,
                &body_bytes,
                session_key.as_deref(),
                SwitchReason::BannedDetected,
            )
            .await
            {
                return Ok(resp);
            }
        } else {
            // 401 且未封号，可能是正常过期或被登出。
            // 按设计：client / solo 模式下 Server 是 RT 轮换的唯一权威，本机不独自 refresh
            // —— 优先问 Server 拿 fresh token，避免和 Server 撞轮换；Server 不可达再降级本地。
            // off / server 模式下本地直接 refresh。
            println!("[Proxy] 拦截到 401，尝试刷新 Token...");

            let outcome = silent_refresh_current(&state).await;
            match outcome {
                SilentRefreshOutcome::Refreshed(new_token) => {
                    if let Ok(retry_resp) = forward_with_token(
                        &state,
                        &method,
                        &upstream_url,
                        &base_headers,
                        &body_bytes,
                        &new_token,
                    )
                    .await
                    {
                        return Ok(build_stream_response(
                            retry_resp,
                            Some(state.tracker.clone()),
                            session_affinity_ctx.clone(),
                        ));
                    }
                }
                SilentRefreshOutcome::LoggedOut => {
                    println!("[Proxy] 刷新失败：账号已登出/RT 被轮换，标记 + 切号");
                    if let Ok(mut store) = state.store.lock() {
                        if let Some(current_id) = store.current.clone() {
                            if let Some(acc) = store.accounts.get_mut(&current_id) {
                                acc.is_logged_out = true;
                                let _ = store.save();
                            }
                        }
                    }
                    if let Some(resp) = try_switch_and_retry(
                        &state,
                        &method,
                        &upstream_url,
                        &base_headers,
                        &body_bytes,
                        session_key.as_deref(),
                        SwitchReason::Http429,
                    )
                    .await
                    {
                        return Ok(resp);
                    }
                }
                SilentRefreshOutcome::OtherError(e) => {
                    println!("[Proxy] 刷新失败 (其他原因): {}", e);
                }
                SilentRefreshOutcome::NoRefreshToken => {
                    println!("[Proxy] 当前账号缺 refresh_token，无法刷新");
                }
            }
        }

        return Ok(Response::builder()
            .status(status_code.as_u16())
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "响应构建失败")));
    }

    // 7. HTTP 429：按 body 内容分流
    //   - body 含 server_is_overloaded / slow_down → **全局过载**，同号 backoff retry
    //     （切号撞同样错，烧账号没用）
    //   - body 含 usage_limit_reached / insufficient_quota / usage_not_included
    //     → per-account 配额耗尽，切号
    //   - 其他 429（无明确 code） → 默认切号
    if status_code == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        let body_lower = String::from_utf8_lossy(&resp_bytes).to_lowercase();
        let is_capacity = body_lower.contains("server_is_overloaded")
            || body_lower.contains("slow_down")
            || matches_global_capacity(&body_lower);

        if is_capacity {
            println!("[Proxy] HTTP 429 + body 含 capacity 关键词，同号 backoff retry...");
            // 同号 backoff retry 3 次（复用与 step 7.5 同样的逻辑思路）
            for attempt in 1..=3u64 {
                let backoff = std::time::Duration::from_secs(2 * attempt);
                tokio::time::sleep(backoff).await;
                let (retry_token, _) = match resolve_token_with_affinity(
                    &state,
                    session_key.as_deref(),
                )
                .await
                {
                    Ok((t, c, _)) => (t, c),
                    Err(_) => continue,
                };
                if let Ok(retry_resp) = forward_with_token(
                    &state,
                    &method,
                    &upstream_url,
                    &base_headers,
                    &body_bytes,
                    &retry_token,
                )
                .await
                {
                    let s = retry_resp.status();
                    if s == reqwest::StatusCode::OK {
                        println!("[Proxy] 429 capacity 同号 retry 第 {} 次成功", attempt);
                        if is_sse_response(&retry_resp) {
                            let h = retry_resp.headers().clone();
                            let stream = retry_resp.bytes_stream().boxed();
                            let resp = build_streaming_response_with_bootstrap(
                                state.clone(),
                                s,
                                h,
                                stream,
                                method.clone(),
                                upstream_url.clone(),
                                base_headers.clone(),
                                body_bytes.clone(),
                                session_affinity_ctx.clone(),
                            );
                            return Ok(resp);
                        }
                        return Ok(build_stream_response(
                            retry_resp,
                            Some(state.tracker.clone()),
                            session_affinity_ctx.clone(),
                        ));
                    }
                    if s == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || (s.as_u16() >= 500 && s.as_u16() < 600)
                    {
                        continue;
                    }
                    return Ok(build_stream_response(
                        retry_resp,
                        Some(state.tracker.clone()),
                        session_affinity_ctx.clone(),
                    ));
                }
            }
            println!("[Proxy] 429 capacity 同号 retry 三次都失败，降级走切号兜底");
            // 撑不住 → 切号兜底
        }

        // 走切号路径（per-account 限额 OR capacity 同号 retry 失败兜底）
        if !is_capacity {
            println!("[Proxy] HTTP 429 (per-account 限额)，标记额度耗尽并切号...");
        }
        mark_current_quota_depleted(&state);
        if let Some(resp) = dispatch_quota_switch_retry(
            &state,
            &method,
            &upstream_url,
            &base_headers,
            &body_bytes,
            session_key.as_deref(),
            SwitchReason::Http429,
        )
        .await
        {
            return Ok(resp);
        }
        // 切号失败/账号耗尽 → 缓冲原始 429 返回
        return Ok(Response::builder()
            .status(429)
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap_or_else(|_| error_response(StatusCode::TOO_MANY_REQUESTS, "429")));
    }

    // 7.5 上游容量满 → 同号 backoff retry，不切号
    // 典型场景：OpenAI 模型池过载，返回 5xx + body "Selected model is at capacity..."
    // 或 HTTP 200 + JSON body 含 server_overloaded / at capacity（codex App 内部 API
    // 调用偶尔会用这种）。两种都不是单号问题，切号也撞同样错；同号等几秒重试就能继续。
    let is_json_response = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_lowercase().contains("json"))
        .unwrap_or(false);
    let should_check_capacity_body = (status_code.as_u16() >= 500 && status_code.as_u16() < 600)
        || (status_code == reqwest::StatusCode::OK && is_json_response);
    if should_check_capacity_body {
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        let body_lower = String::from_utf8_lossy(&resp_bytes).to_lowercase();
        let is_capacity = matches_global_capacity(&body_lower)
            || status_code == reqwest::StatusCode::SERVICE_UNAVAILABLE
            || body_lower.contains("server_is_overloaded")
            || body_lower.contains("slow_down");
        if is_capacity {
            println!("[Proxy] 上游 {} + 容量满，同号 backoff retry...", status_code);
            for attempt in 1..=3u64 {
                let backoff = std::time::Duration::from_secs(2 * attempt);
                tokio::time::sleep(backoff).await;
                let (retry_token, _) = match resolve_token_with_affinity(&state, session_key.as_deref()).await {
                    Ok((t, c, _)) => (t, c),
                    Err(_) => continue,
                };
                if let Ok(retry_resp) = forward_with_token(
                    &state,
                    &method,
                    &upstream_url,
                    &base_headers,
                    &body_bytes,
                    &retry_token,
                )
                .await
                {
                    let s = retry_resp.status();
                    if s == reqwest::StatusCode::OK {
                        println!("[Proxy] 容量满同号 retry 第 {} 次成功", attempt);
                        // 200 + SSE 走 bootstrap，否则透传
                        if is_sse_response(&retry_resp) {
                            let h = retry_resp.headers().clone();
                            let stream = retry_resp.bytes_stream().boxed();
                            let resp = build_streaming_response_with_bootstrap(
                                state.clone(),
                                s,
                                h,
                                stream,
                                method.clone(),
                                upstream_url.clone(),
                                base_headers.clone(),
                                body_bytes.clone(),
                                session_affinity_ctx.clone(),
                            );
                            return Ok(resp);
                        }
                        return Ok(build_stream_response(
                            retry_resp,
                            Some(state.tracker.clone()),
                            session_affinity_ctx.clone(),
                        ));
                    }
                    if s.as_u16() >= 500 && s.as_u16() < 600 {
                        // 还是 5xx，下一轮 backoff
                        continue;
                    }
                    // 其他 status：当作正常响应退出 retry 透传
                    return Ok(build_stream_response(
                        retry_resp,
                        Some(state.tracker.clone()),
                        session_affinity_ctx.clone(),
                    ));
                }
            }
            println!("[Proxy] 容量满同号 retry 三次都失败，降级走切号兜底");
            // 撑不住 → 当 quota 路径处理（切号），最后兜底也失败再返 502
            if let Some(resp) = dispatch_quota_switch_retry(
                &state,
                &method,
                &upstream_url,
                &base_headers,
                &body_bytes,
                session_key.as_deref(),
                SwitchReason::Http429,
            )
            .await
            {
                return Ok(resp);
            }
            // 全部失败 → 把缓冲下来的原始 5xx body 转一个 generic 502，**不带原文**
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                "上游容量满且重试无果",
            ));
        }
        // 不是容量满的 5xx → 缓冲下来的 body 透传给 codex
        return Ok(Response::builder()
            .status(status_code.as_u16())
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "5xx 透传")));
    }

    // 7.6 其他 4xx（400/404/422 等）：上游通常返 {"detail":"Bad Request"} 一类的 FastAPI
    // 错误体。proxy 不切号、不重试，但把 path + body 前 1KB 写 log，方便排错。
    if status_code.is_client_error()
        && status_code != reqwest::StatusCode::UNAUTHORIZED
        && status_code != reqwest::StatusCode::FORBIDDEN
        && status_code != reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        let preview: String = String::from_utf8_lossy(&resp_bytes).chars().take(1024).collect();
        println!(
            "[Proxy] 上游 {} {} body: {}",
            status_code.as_u16(),
            upstream_url,
            preview
        );
        return Ok(Response::builder()
            .status(status_code.as_u16())
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "4xx 透传失败")));
    }

    // 8. 成功响应（200 + SSE）→ 立刻返回 Response，body 流内部跑 bootstrap+心跳+切号
    if status_code == reqwest::StatusCode::OK && is_sse_response(&upstream_resp) {
        let resp_status = upstream_resp.status();
        let resp_headers = upstream_resp.headers().clone();
        let raw_stream = upstream_resp.bytes_stream().boxed();

        let resp = build_streaming_response_with_bootstrap(
            state.clone(),
            resp_status,
            resp_headers,
            raw_stream,
            method.clone(),
            upstream_url.clone(),
            base_headers.clone(),
            body_bytes.clone(),
            session_affinity_ctx.clone(),
        );
        // 后台检查预防性切号（保持原行为）
        let state_clone = state.clone();
        tokio::spawn(async move {
            if should_preemptive_switch(&state_clone) {
                if state_clone
                    .switching
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    if let PickResult::Found { id, .. } = pick_next_account(&state_clone) {
                        let _ = do_switch(&state_clone, &id, SwitchReason::QuotaThreshold);
                    }
                    state_clone.switching.store(false, Ordering::SeqCst);
                }
            }
        });
        return Ok(resp);
    }

    // 9. 非 SSE / 其它 status → 旧的透传路径
    let resp = build_stream_response(upstream_resp, Some(state.tracker.clone()), session_affinity_ctx);

    // 后台检查预防性切号
    let state_clone = state.clone();
    tokio::spawn(async move {
        if should_preemptive_switch(&state_clone) {
            if state_clone
                .switching
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                if let PickResult::Found { id, .. } = pick_next_account(&state_clone) {
                    let _ = do_switch(&state_clone, &id, SwitchReason::QuotaThreshold);
                }
                state_clone.switching.store(false, Ordering::SeqCst);
            }
        }
    });

    Ok(resp)
}

/// client 模式：让 Server 仲裁切号，然后用新 token 重试
async fn try_remote_switch_and_retry(
    state: &ProxyState,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
    session_key: Option<&str>,
    reason_label: &str,
) -> Option<Response<ProxyBody>> {
    let (current_id, primary, fallback, secret) = {
        let store = state.store.lock().ok()?;
        (
            store.current.clone(),
            store.settings.remote_server_url.clone(),
            store.settings.remote_server_url_fallback.clone(),
            store.settings.remote_shared_secret.clone(),
        )
    };
    if secret.is_empty() {
        eprintln!("[Proxy] client 模式但未配置 remote_shared_secret");
        return None;
    }
    let base = match crate::remote_client::resolve_base_url(&primary, &fallback).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[Proxy] 解析 Server 地址失败: {}", e);
            return None;
        }
    };

    for attempt in 0..MAX_429_RETRIES {
        let outcome = match crate::remote_client::request_switch(
            &base,
            &secret,
            current_id.as_deref(),
            reason_label,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[Proxy] 向 Server 请求切号失败: {}", e);
                return None;
            }
        };
        if outcome.exhausted {
            eprintln!("[Proxy] Server 告知无可用账号，停止重试");
            return None;
        }
        let Some(new_current) = outcome.current.clone() else {
            return None;
        };
        // 把 Server 的 current 同步到本机（拉 token + 写 auth.json）
        if let Err(e) = adopt_remote_current(state, &base, &secret, &new_current).await {
            eprintln!("[Proxy] 采纳 Server current 失败: {}", e);
            return None;
        }
        invalidate_remote_token_cache();
        let (new_token, _) = match get_current_token(state).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[Proxy] 获取新 token 失败: {}", e);
                return None;
            }
        };
        // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
        let body_for_new = rewrite_prompt_cache_key(body, &new_current);
        let headers_no_ts = headers_without_turn_state(base_headers);
        match forward_and_bootstrap(state, method, upstream_url, &headers_no_ts, &body_for_new, &new_token, session_key)
            .await
        {
            BootstrappedForward::Ok(resp) => {
                state.stats.auto_switches.fetch_add(1, Ordering::Relaxed);
                let _ = state
                    .app_handle
                    .emit("proxy-account-switched", &outcome.name.unwrap_or_default());
                return Some(resp);
            }
            BootstrappedForward::RateLimit | BootstrappedForward::Capacity => {
                println!(
                    "[Proxy] 第 {} 次 Server 切号后仍限额/容量满，再试",
                    attempt + 1
                );
                continue;
            }
            BootstrappedForward::Unauthorized => {
                // Server 给的 token 失效（罕见，Server 应该已经 refresh 过了）。继续 retry，
                // 下一轮 request_switch 会让 Server 选别的号
                println!("[Proxy] 第 {} 次 Server 切号 token 失效，再试", attempt + 1);
                continue;
            }
            BootstrappedForward::Banned => {
                println!(
                    "[Proxy] 第 {} 次 Server 切号目标号疑似封号，再试",
                    attempt + 1
                );
                // Server 那侧的封号判定由 Server 自己处理；本机继续向 Server 询问
                continue;
            }
            BootstrappedForward::Failed(e) => {
                eprintln!("[Proxy] 重试请求失败: {}", e);
                return None;
            }
        }
    }
    None
}

/// 把 Server 的 current 采纳到本机 store：拉 token、写 auth.json、更新 store.current
async fn adopt_remote_current(
    state: &ProxyState,
    base: &str,
    secret: &str,
    new_id: &str,
) -> Result<(), String> {
    let t = crate::remote_client::fetch_token(base, secret, new_id).await?;
    // 先检查账号是否存在（短作用域 lock）
    let had_account = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store.accounts.contains_key(new_id)
    };
    if !had_account {
        // Server 上有但本机没有 → 拉整个账号列表同步
        if let Ok(list) = crate::remote_client::list_accounts(base, secret).await {
            if let Ok(mut s) = state.store.lock() {
                for a in list {
                    s.accounts.insert(a.id.clone(), a);
                }
                let _ = s.save();
            }
        }
    }
    // 写入新 token + 更新 current（短作用域 lock，无 await）
    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        store.sync_account_from_auth_json(new_id, t.auth_json.clone());
        if let Some(acc) = store.accounts.get(new_id) {
            let auth = acc.auth_json.clone();
            // adopt_remote_current 是 client 模式的换号路径，扩展 expires_at 防 codex 自刷
            crate::account::AccountStore::write_codex_auth_extended_expiry(&auth)
                .map_err(|e| format!("写 auth.json 失败: {}", e))?;
        }
        store.current = Some(new_id.to_string());
        let _ = store.save();
    }
    state.ws_disconnect.notify_waiters();
    let _ = state.app_handle.emit("accounts-updated", ());
    Ok(())
}

/// 切号并重试（最多 MAX_429_RETRIES 次）
async fn try_switch_and_retry(
    state: &ProxyState,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
    session_key: Option<&str>,
    reason: SwitchReason,
) -> Option<Response<ProxyBody>> {
    // current 是 Relay 时按设置决定 401/429 是否自动切走
    {
        let store = state.store.lock().ok();
        let cur_is_relay = store
            .as_ref()
            .and_then(|s| s.current.clone())
            .and_then(|id| store.as_ref().unwrap().accounts.get(&id).map(|a| a.is_relay()))
            .unwrap_or(false);
        let allow_out = store
            .map(|s| s.settings.relay_auto_switch_out)
            .unwrap_or(true);
        if cur_is_relay && !allow_out {
            println!("[Proxy] current 是 Relay 且 relay_auto_switch_out=false，不自动切（{:?}）", reason);
            return None;
        }
    }
    for attempt in 0..MAX_429_RETRIES {
        match pick_next_account(state) {
            PickResult::Found { id, token } => {
                if let Err(e) = do_switch(state, &id, reason.clone()) {
                    eprintln!("[Proxy] 切号失败: {}", e);
                    continue;
                }
                // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
                let body_for_new = rewrite_prompt_cache_key(body, &id);
                let headers_no_ts = headers_without_turn_state(base_headers);
                match forward_and_bootstrap(
                    state,
                    method,
                    upstream_url,
                    &headers_no_ts,
                    &body_for_new,
                    &token,
                    session_key,
                )
                .await
                {
                    BootstrappedForward::Ok(resp) => {
                        println!("[Proxy] 第 {} 次切号重试成功", attempt + 1);
                        return Some(resp);
                    }
                    BootstrappedForward::RateLimit => {
                        println!("[Proxy] 第 {} 次切号后仍限额（status/流内）", attempt + 1);
                        mark_current_quota_depleted(state);
                        continue;
                    }
                    BootstrappedForward::Capacity => {
                        println!("[Proxy] 第 {} 次切号目标号也容量满（全局过载），再试下一个", attempt + 1);
                        // 容量满不是该号的问题，不要 mark_quota_depleted；直接换下一个看运气
                        continue;
                    }
                    BootstrappedForward::Banned => {
                        println!("[Proxy] 第 {} 次切号目标号疑似封号", attempt + 1);
                        mark_current_banned(state);
                        continue;
                    }
                    BootstrappedForward::Unauthorized => {
                        // 切到的新号 token 也失效了（refresh_token 已轮换或被吊销），
                        // 标记 token_invalid 让 pick_next 跳过，继续找下一个
                        println!("[Proxy] 第 {} 次切号目标号 token 失效", attempt + 1);
                        if let Ok(mut store) = state.store.lock() {
                            if let Some(acc) = store.accounts.get_mut(&id) {
                                acc.is_token_invalid = true;
                                let _ = store.save();
                            }
                        }
                        continue;
                    }
                    BootstrappedForward::Failed(e) => {
                        eprintln!("[Proxy] 切号后转发失败: {}", e);
                        continue;
                    }
                }
            }
            PickResult::Exhausted { earliest_reset } => {
                let msg = if let Some(ts) = earliest_reset {
                    let dt = chrono::DateTime::from_timestamp(ts, 0)
                        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                        .unwrap_or_else(|| "未知".to_string());
                    format!("所有账号额度已耗尽，最早恢复：{}", dt)
                } else {
                    "所有账号额度已耗尽".to_string()
                };
                eprintln!("[Proxy] {}", msg);
                let _ = state.app_handle.emit("proxy-all-exhausted", &msg);
                return None;
            }
        }
    }
    None
}

// ────────────────────────────────────────────────────────────────
// HTTP 转发与响应构建
// ────────────────────────────────────────────────────────────────

/// 把 Server 的 remote-api URL（端口通常是 18081）换成 proxy URL（默认 18080）
fn derive_server_proxy_url(api_url: &str, proxy_port: u16) -> Option<String> {
    let u = reqwest::Url::parse(api_url.trim()).ok()?;
    let host = u.host_str()?;
    Some(format!("{}://{}:{}", u.scheme(), host, proxy_port))
}

/// 切号到新账号时调用：复制 headers + 剥掉 `x-codex-turn-state`。
/// 这个 header 是 OpenAI 服务端给的 sticky-routing token，绑定到颁发它的账号/分片。
/// 老账号的 turn-state 在新账号上无效甚至有害（可能 401 或路由错误）。
/// codex-rs 自己的 client.rs:360-370 注释也明说"must not send between different turns"。
/// 切号 = 跨 turn，必须剥。
fn headers_without_turn_state(
    base: &reqwest::header::HeaderMap,
) -> reqwest::header::HeaderMap {
    let mut h = base.clone();
    h.remove("x-codex-turn-state");
    h
}

/// 构造上游请求的透明转发 header（剔除 host/authorization/connection，注入上游 Host）
fn build_upstream_headers(
    req_headers: &hyper::HeaderMap,
    upstream_host: &str,
) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    for (name, value) in req_headers {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "authorization" || lower == "host" || lower == "connection" {
            continue;
        }
        if let Ok(rn) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(rv) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
                h.append(rn, rv);
            }
        }
    }
    if let Ok(host_val) = reqwest::header::HeaderValue::from_str(upstream_host) {
        h.insert(reqwest::header::HOST, host_val);
    }
    h
}

/// client 模式专用：把已解析好的请求部件原样透传到 Server 的 proxy 端口。
/// 返回原始 reqwest::Response，由调用方决定是流式透传还是（针对 402 等）缓冲后重试。
async fn forward_to_server_parts(
    state: &ProxyState,
    method: &hyper::Method,
    path_and_query: &str,
    req_headers: &hyper::HeaderMap,
    body: &Bytes,
) -> Result<reqwest::Response, String> {
    let (primary, fallback, proxy_port) = {
        let s = state.store.lock().map_err(|e| e.to_string())?;
        (
            s.settings.remote_server_url.clone(),
            s.settings.remote_server_url_fallback.clone(),
            s.settings.proxy_port,
        )
    };

    let api_base = crate::remote_client::resolve_base_url(&primary, &fallback)
        .await
        .map_err(|e| format!("Server 不可达: {}", e))?;
    let proxy_base = derive_server_proxy_url(&api_base, proxy_port)
        .ok_or_else(|| format!("无法从 {} 构造 Server proxy URL", api_base))?;
    let upstream_url = format!("{}{}", proxy_base, path_and_query);
    println!("[Proxy] → server forward: {} {}", method, upstream_url);

    let mut fwd_headers = reqwest::header::HeaderMap::new();
    for (name, value) in req_headers {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "host" || lower == "authorization" || lower == "connection" {
            continue;
        }
        if let Ok(rn) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(rv) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
                fwd_headers.append(rn, rv);
            }
        }
    }

    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    state
        .client
        .request(reqwest_method, &upstream_url)
        .headers(fwd_headers)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| format!("转发到 Server 失败: {}", e))
}

/// client 模式回退路径：当 Server 不可达或 Server 告知无可用账号时，尝试使用本机账号直连上游。
/// - 挑一个未封号且有额度的本地账号
/// - 本地 store.current 切到该账号（标记切号来源 RemoteFallback）
/// - 用其 token 直接打 OpenAI 上游
/// 若本机无可用账号则返回 None
async fn try_local_fallback(
    state: &ProxyState,
    method: &hyper::Method,
    path_and_query: &str,
    req_headers: &hyper::HeaderMap,
    body: &Bytes,
    session_key: Option<&str>,
) -> Option<Response<ProxyBody>> {
    let PickResult::Found { id, token } = pick_next_account(state) else {
        eprintln!("[Proxy] 本地回退失败：无可用账号");
        return None;
    };

    // 把本机 current 切到回退账号（也顺便写 auth.json 以便 UI/其它进程感知）
    if let Err(e) = do_switch(state, &id, SwitchReason::RemoteFallback) {
        eprintln!("[Proxy] 本地回退切号失败: {}", e);
        return None;
    }

    // 根据 token 形态路由上游（Relay 类型 sk- 不以 eyJ 开头，is_chatgpt 自然 false）
    let is_chatgpt = token.starts_with("eyJ");
    let relay_base_url = account_relay_base_url(state, &id);
    let (upstream_url, upstream_host) =
        get_upstream(is_chatgpt, relay_base_url.as_deref(), path_and_query);
    let base_headers = build_upstream_headers(req_headers, &upstream_host);
    // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
    let body_for_new = rewrite_prompt_cache_key(body, &id);
    let headers_no_ts = headers_without_turn_state(&base_headers);
    match forward_and_bootstrap(state, method, &upstream_url, &headers_no_ts, &body_for_new, &token, session_key).await {
        BootstrappedForward::Ok(resp) => {
            println!("[Proxy] 本地回退转发成功");
            Some(resp)
        }
        BootstrappedForward::RateLimit => {
            // 回退账号也限额：交回 None 让上层报错（None 时 Server 路径会返回 Server 的 402/原错误）
            println!("[Proxy] 本地回退账号也已限额");
            mark_current_quota_depleted(state);
            None
        }
        BootstrappedForward::Capacity => {
            println!("[Proxy] 本地回退碰到全局容量满（不是单号问题）");
            None
        }
        BootstrappedForward::Banned => {
            println!("[Proxy] 本地回退账号疑似封号");
            mark_current_banned(state);
            None
        }
        BootstrappedForward::Unauthorized => {
            println!("[Proxy] 本地回退账号 token 失效");
            None
        }
        BootstrappedForward::Failed(e) => {
            eprintln!("[Proxy] 本地回退转发失败: {}", e);
            None
        }
    }
}

async fn forward_with_token(
    state: &ProxyState,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
    token: &str,
) -> Result<reqwest::Response, String> {
    let mut headers = base_headers.clone();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token)) {
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }

    println!("[Proxy] → upstream: {} {}", method, upstream_url);

    state
        .client
        .request(
            reqwest::Method::from_bytes(method.as_str().as_bytes())
                .unwrap_or(reqwest::Method::POST),
            upstream_url,
        )
        .headers(headers)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| format!("转发请求失败: {}", e))
}

// ────────────────────────────────────────────────────────────────
// SSE bootstrap：在把字节下发给 codex 前嗅探流前缀，确认不是限额/封号错误。
// 模式来自 CLIProxyAPI（conductor.go::readStreamBootstrap），针对 Codex 收紧到
// "看到首个真正的内容事件（output_text.delta / output_item.added / ...）"
// 才算 commit。期间出现 response.failed + rate_limit/usage_limit 关键词 →
// 触发切号重发，codex 那头一字节都没收到，无损。
// ────────────────────────────────────────────────────────────────

/// SSE bootstrap 默认上限（settings 没配时的兜底）
const DEFAULT_BOOTSTRAP_BYTE_CAP: usize = 32 * 1024;
const DEFAULT_BOOTSTRAP_TIME_CAP_MS: u64 = 8000;

/// 从 store 读 bootstrap 上限；没读到（锁失败）就用默认值
fn read_bootstrap_caps(state: &ProxyState) -> (usize, u64) {
    state
        .store
        .lock()
        .map(|s| {
            let b = if s.settings.proxy_bootstrap_byte_cap > 0 {
                s.settings.proxy_bootstrap_byte_cap
            } else {
                DEFAULT_BOOTSTRAP_BYTE_CAP
            };
            let t = if s.settings.proxy_bootstrap_time_cap_ms > 0 {
                s.settings.proxy_bootstrap_time_cap_ms
            } else {
                DEFAULT_BOOTSTRAP_TIME_CAP_MS
            };
            (b, t)
        })
        .unwrap_or((DEFAULT_BOOTSTRAP_BYTE_CAP, DEFAULT_BOOTSTRAP_TIME_CAP_MS))
}

type ByteStream =
    futures_util::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>;

enum SseBootstrap {
    /// 安全可下发：已看到内容事件，或缓冲达到上限/超时，让流继续走
    Ready { prefix: Bytes, rest: ByteStream },
    /// 流前缀里检测到 per-account 限额事件 → 切号重发
    RateLimitInStream,
    /// 流前缀里检测到 global 容量满事件 → 同号 backoff retry，不切号
    CapacityInStream,
    /// 流前缀里检测到封号事件 → 切号重发
    BannedInStream,
}

fn is_sse_response(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_lowercase().contains("event-stream"))
        .unwrap_or(false)
}

/// 检测前缀里有没有错误事件 + 是 per-account 限额 / global 容量满 / 都不是
enum SseErrorClass {
    None,
    PerAccountLimit,
    GlobalCapacity,
}

fn classify_sse_error(buf: &[u8]) -> SseErrorClass {
    let s = String::from_utf8_lossy(buf).to_lowercase();
    let in_failure = s.contains("event: response.failed")
        || s.contains("event: error")
        || s.contains("\"type\":\"response.failed\"")
        || s.contains("\"type\":\"error\"");
    if !in_failure {
        return SseErrorClass::None;
    }
    // 注意先判 per-account（更具体），再判 global capacity（更通用）。
    // 若同时命中两类（极端拼接），按 per-account 处理（保守）。
    if PER_ACCOUNT_LIMIT_KEYWORDS.iter().any(|kw| s.contains(kw)) {
        return SseErrorClass::PerAccountLimit;
    }
    if GLOBAL_CAPACITY_KEYWORDS.iter().any(|kw| s.contains(kw)) {
        return SseErrorClass::GlobalCapacity;
    }
    SseErrorClass::None
}

/// 兼容老调用：任何 limit/capacity 信号都返回 true
fn sse_buf_has_rate_limit(buf: &[u8]) -> bool {
    !matches!(classify_sse_error(buf), SseErrorClass::None)
}

fn sse_buf_has_banned(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf).to_lowercase();
    let in_failure = s.contains("event: response.failed")
        || s.contains("event: error")
        || s.contains("\"type\":\"response.failed\"")
        || s.contains("\"type\":\"error\"");
    if !in_failure {
        return false;
    }
    BANNED_KEYWORDS.iter().any(|kw| s.contains(kw))
}

/// 是否看到了首个"真内容"事件 —— 看到这个就 commit，让流走出去。
/// Codex Responses API 的内容事件名（不含 response.created / response.in_progress）。
fn sse_buf_has_content_event(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    s.contains("event: response.output_text.delta")
        || s.contains("event: response.output_item.added")
        || s.contains("event: response.content_part.added")
        || s.contains("event: response.reasoning_summary_text.delta")
        || s.contains("event: response.reasoning_text.delta")
        || s.contains("event: response.completed")
        || s.contains("\"type\":\"response.output_text.delta\"")
        || s.contains("\"type\":\"response.output_item.added\"")
        || s.contains("\"type\":\"response.content_part.added\"")
        || s.contains("\"type\":\"response.completed\"")
}

async fn read_sse_bootstrap(
    mut stream: ByteStream,
    byte_cap: usize,
    time_cap_ms: u64,
) -> SseBootstrap {
    let mut buf = Vec::<u8>::new();
    let started = std::time::Instant::now();
    let time_cap = std::time::Duration::from_millis(time_cap_ms);

    loop {
        if buf.len() >= byte_cap {
            return SseBootstrap::Ready {
                prefix: Bytes::from(buf),
                rest: stream,
            };
        }
        let elapsed = started.elapsed();
        if elapsed >= time_cap {
            return SseBootstrap::Ready {
                prefix: Bytes::from(buf),
                rest: stream,
            };
        }
        let next = match tokio::time::timeout(time_cap - elapsed, stream.next()).await {
            Ok(item) => item,
            Err(_) => {
                return SseBootstrap::Ready {
                    prefix: Bytes::from(buf),
                    rest: stream,
                };
            }
        };
        let chunk = match next {
            Some(Ok(c)) => c,
            // 上游错误：把已缓冲部分原样下发，让 build_stream_response_from_parts
            // 正常完成（错误会在尾部触发 stream 结束）。
            Some(Err(_)) => {
                return SseBootstrap::Ready {
                    prefix: Bytes::from(buf),
                    rest: stream,
                };
            }
            None => {
                // 流自然结束 + 没有内容事件 + 没有错误事件 → 当作空响应透传
                return SseBootstrap::Ready {
                    prefix: Bytes::from(buf),
                    rest: futures_util::stream::empty().boxed(),
                };
            }
        };
        buf.extend_from_slice(&chunk);

        // 顺序很重要：先嗅错误，再判内容事件。错误又分两类：
        //   - PerAccountLimit → 切号
        //   - GlobalCapacity  → 同号 retry
        match classify_sse_error(&buf) {
            SseErrorClass::PerAccountLimit => {
                return SseBootstrap::RateLimitInStream;
            }
            SseErrorClass::GlobalCapacity => {
                return SseBootstrap::CapacityInStream;
            }
            SseErrorClass::None => {}
        }
        if sse_buf_has_banned(&buf) {
            return SseBootstrap::BannedInStream;
        }
        if sse_buf_has_content_event(&buf) {
            return SseBootstrap::Ready {
                prefix: Bytes::from(buf),
                rest: stream,
            };
        }
    }
}

/// 转发并嗅探：上游一发回 response 立刻判定 status，再决定要不要做 bootstrap。
/// 主要给 try_switch_and_retry / try_remote_switch_and_retry / try_local_fallback /
/// 主链路成功路径复用，避免重复代码。
enum BootstrappedForward {
    /// 安全可下发的 Response（status 任意，对 200+SSE 已做过 bootstrap）
    Ok(Response<ProxyBody>),
    /// status 429 或流内 per-account 限额事件 → 调用方应该再切号重试
    RateLimit,
    /// 流内全局容量满事件 → 调用方应该 backoff retry **同号**（不切）
    Capacity,
    /// 流内封号事件 → 调用方应该标记封号并切号重试
    Banned,
    /// 401/403 token 失效 → 调用方应该 silent_refresh + 重试，不能透回 codex
    /// （否则触发 codex UnauthorizedRecovery，撞 account_id mismatch 永久失败）
    Unauthorized,
    /// 上游连接错误
    Failed(String),
}

async fn forward_and_bootstrap(
    state: &ProxyState,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
    token: &str,
    session_key: Option<&str>,
) -> BootstrappedForward {
    let resp = match forward_with_token(state, method, upstream_url, base_headers, body, token).await {
        Ok(r) => r,
        Err(e) => return BootstrappedForward::Failed(e),
    };
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return BootstrappedForward::RateLimit;
    }
    // 401/403 不能透回 codex：触发它的 UnauthorizedRecovery 会撞 account_id mismatch 永久失败
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return BootstrappedForward::Unauthorized;
    }
    let aff = make_affinity_ctx(state, session_key);
    // 非 200 或非 SSE：保留旧行为，直接透传给客户端
    if status != reqwest::StatusCode::OK || !is_sse_response(&resp) {
        return BootstrappedForward::Ok(build_stream_response(resp, Some(state.tracker.clone()), aff));
    }
    let headers = resp.headers().clone();
    let stream = resp.bytes_stream().boxed();
    let (byte_cap, time_cap_ms) = read_bootstrap_caps(state);
    match read_sse_bootstrap(stream, byte_cap, time_cap_ms).await {
        SseBootstrap::Ready { prefix, rest } => BootstrappedForward::Ok(
            build_stream_response_from_parts(status, headers, prefix, rest, Some(state.tracker.clone()), aff),
        ),
        SseBootstrap::RateLimitInStream => BootstrappedForward::RateLimit,
        SseBootstrap::CapacityInStream => BootstrappedForward::Capacity,
        SseBootstrap::BannedInStream => BootstrappedForward::Banned,
    }
}

/// 把 client / local 模式下"切号 + 重发"的分支统一起来。
/// 给 status 429 路径和流内限额路径共用。
async fn dispatch_quota_switch_retry(
    state: &Arc<ProxyState>,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
    session_key: Option<&str>,
    reason: SwitchReason,
) -> Option<Response<ProxyBody>> {
    let remote_mode = state
        .store
        .lock()
        .map(|s| s.settings.remote_mode.clone())
        .unwrap_or_default();

    let remote_label = match &reason {
        SwitchReason::Http429 => "http_429",
        SwitchReason::InStreamRateLimit => "in_stream_rate_limit",
        SwitchReason::InStreamBanned => "in_stream_banned",
        _ => "http_429",
    };

    if remote_mode == "client" {
        if state
            .switching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let result =
                try_remote_switch_and_retry(state, method, upstream_url, base_headers, body, session_key, remote_label).await;
            state.switching.store(false, Ordering::SeqCst);
            return result;
        }
        // 别人正在切号 → 短等后用最新 current 直接重发
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok((new_token, _)) = get_current_token(state).await {
            if let BootstrappedForward::Ok(resp) =
                forward_and_bootstrap(state, method, upstream_url, base_headers, body, &new_token, session_key)
                    .await
            {
                return Some(resp);
            }
        }
        return None;
    }

    // 本地模式
    if state
        .switching
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let result = try_switch_and_retry(state, method, upstream_url, base_headers, body, session_key, reason).await;
        state.switching.store(false, Ordering::SeqCst);
        return result;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if let Ok((new_token, _)) = get_current_token(state).await {
        if let BootstrappedForward::Ok(resp) =
            forward_and_bootstrap(state, method, upstream_url, base_headers, body, &new_token, session_key).await
        {
            return Some(resp);
        }
    }
    None
}

/// 用于把"该请求归属于哪个 session、用了哪个号"信息传到响应解析末尾，
/// 让 end_signal 在看到 cached_tokens>0 时把 binding 记进 SessionAffinity。
#[derive(Clone)]
struct AffinityCtx {
    affinity: Arc<SessionAffinity>,
    session_key: String,
    account_id: String,
}

/// 切号到新账号时调用：把 body 里的 `prompt_cache_key` 后缀拼上当前 account_id。
/// codex CLI/App 默认用 `conversation_id` 当 prompt_cache_key（codex-rs/core/src/client.rs:699）
/// —— 不区分账号。账号 A 写过 cache 后，切到 B 拿同样 key 命中的 cache 在 OpenAI
/// user-shard 里根本不存在，造成 cache miss。
///
/// 拼上 `::<account_id>` 后缀让 cache 按账号天然隔离：
///   - 同号请求保持原 key（cache 持续命中）
///   - 切到新号自动用新 key（OpenAI 那边 cold cache 一轮，下一轮起 warm）
///
/// 注意：只在切号 retry 路径调用。第一次发请求时不动，让 codex 自己原始的
/// conversation_id 走（codex 重启会重新生成 conversation_id，自动带换号语义）。
fn rewrite_prompt_cache_key(body: &Bytes, account_id: &str) -> Bytes {
    if account_id.is_empty() {
        return body.clone();
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.clone();
    };
    let new_key = match obj.get("prompt_cache_key").and_then(|x| x.as_str()) {
        Some(orig) if !orig.is_empty() => format!("{}::{}", orig, account_id),
        _ => format!("acc::{}", account_id),
    };
    obj.insert(
        "prompt_cache_key".to_string(),
        serde_json::Value::String(new_key),
    );
    match serde_json::to_vec(&v) {
        Ok(s) => Bytes::from(s),
        Err(_) => body.clone(),
    }
}

fn make_affinity_ctx(state: &ProxyState, session_key: Option<&str>) -> Option<AffinityCtx> {
    let sk = session_key?;
    let aid = state.store.lock().ok().and_then(|s| s.current.clone())?;
    Some(AffinityCtx {
        affinity: state.session_affinity.clone(),
        session_key: sk.to_string(),
        account_id: aid,
    })
}

/// 给 ByteStream 套一层 SSE keep-alive：每 `interval` 没新 chunk 就 yield 一个 SSE 注释
/// (": keep-alive\n\n")。这是 SSE 协议层的 no-op，浏览器/codex 都会忽略，但能让 TCP 写
/// 操作有动作，避免 client 那头超时关连接。
fn wrap_with_sse_heartbeat(
    inner: ByteStream,
    interval: std::time::Duration,
) -> ByteStream {
    futures_util::stream::unfold(inner, move |mut s| async move {
        match tokio::time::timeout(interval, s.next()).await {
            Ok(Some(item)) => Some((item, s)),
            Ok(None) => None,
            Err(_) => Some((
                Ok(Bytes::from_static(b": keep-alive\n\n")),
                s,
            )),
        }
    })
    .boxed()
}

// ────────────────────────────────────────────────────────────────
// Bootstrap-aware streaming response：返回 Response 后在 body stream 内部
// 跑 bootstrap 嗅探 + 失败重试，期间向 client 发 SSE keep-alive 心跳。
// 这样 bootstrap 时间上限可以放宽到 30s+，不怕 client 那头超时。
// ────────────────────────────────────────────────────────────────

/// 类似 read_sse_bootstrap，但在等 chunk 期间通过 `tx` 向 client 发心跳。
async fn bootstrap_with_heartbeats(
    mut stream: ByteStream,
    byte_cap: usize,
    time_cap_ms: u64,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, reqwest::Error>>,
    heartbeat_interval: std::time::Duration,
) -> SseBootstrap {
    let mut buf = Vec::<u8>::new();
    let started = std::time::Instant::now();
    let time_cap = std::time::Duration::from_millis(time_cap_ms);
    let mut last_heartbeat = std::time::Instant::now();

    loop {
        if buf.len() >= byte_cap {
            return SseBootstrap::Ready { prefix: Bytes::from(buf), rest: stream };
        }
        let elapsed = started.elapsed();
        if elapsed >= time_cap {
            return SseBootstrap::Ready { prefix: Bytes::from(buf), rest: stream };
        }
        // 取 (剩余时间预算, 距下次心跳的时间) 的较小值
        let until_time_cap = time_cap - elapsed;
        let since_hb = last_heartbeat.elapsed();
        let until_hb = if since_hb >= heartbeat_interval {
            std::time::Duration::ZERO
        } else {
            heartbeat_interval - since_hb
        };
        let wait = until_time_cap.min(until_hb);

        let next = tokio::time::timeout(wait, stream.next()).await;
        match next {
            Ok(Some(Ok(chunk))) => {
                buf.extend_from_slice(&chunk);
                match classify_sse_error(&buf) {
                    SseErrorClass::PerAccountLimit => {
                        return SseBootstrap::RateLimitInStream;
                    }
                    SseErrorClass::GlobalCapacity => {
                        return SseBootstrap::CapacityInStream;
                    }
                    SseErrorClass::None => {}
                }
                if sse_buf_has_banned(&buf) {
                    return SseBootstrap::BannedInStream;
                }
                if sse_buf_has_content_event(&buf) {
                    return SseBootstrap::Ready { prefix: Bytes::from(buf), rest: stream };
                }
            }
            Ok(Some(Err(_))) => {
                return SseBootstrap::Ready { prefix: Bytes::from(buf), rest: stream };
            }
            Ok(None) => {
                return SseBootstrap::Ready {
                    prefix: Bytes::from(buf),
                    rest: futures_util::stream::empty().boxed(),
                };
            }
            Err(_) => {
                // 等待超时 → 看看是不是该发心跳
                if last_heartbeat.elapsed() >= heartbeat_interval {
                    if tx.send(Ok(Bytes::from_static(b": keep-alive\n\n"))).await.is_err() {
                        // client 已断
                        return SseBootstrap::Ready { prefix: Bytes::from(buf), rest: stream };
                    }
                    last_heartbeat = std::time::Instant::now();
                }
            }
        }
    }
}

/// 给 body 流任务用：换号 + forward → 拿到下一个 upstream 的 raw bytes_stream。
/// 不做 bootstrap，调用方继续在新流上跑 bootstrap_with_heartbeats。
/// 用 store.current 的最新 token 在**同账号**上重发一次请求，拿新的 upstream stream。
/// 给"上游全局容量满 → 同号 backoff retry"用。不切号、不修改 store.current。
async fn acquire_same_account_upstream(
    state: &Arc<ProxyState>,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
) -> Option<ByteStream> {
    let (token, _) = get_current_token(state).await.ok()?;
    let resp = forward_with_token(state, method, upstream_url, base_headers, body, &token)
        .await
        .ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    Some(resp.bytes_stream().boxed())
}

async fn acquire_replacement_upstream(
    state: &Arc<ProxyState>,
    method: &hyper::Method,
    upstream_url: &str,
    base_headers: &reqwest::header::HeaderMap,
    body: &Bytes,
    reason: SwitchReason,
) -> Option<ByteStream> {
    let remote_mode = state
        .store
        .lock()
        .map(|s| s.settings.remote_mode.clone())
        .unwrap_or_default();

    if remote_mode == "client" {
        let (current_id, primary, fallback, secret) = {
            let s = state.store.lock().ok()?;
            (
                s.current.clone(),
                s.settings.remote_server_url.clone(),
                s.settings.remote_server_url_fallback.clone(),
                s.settings.remote_shared_secret.clone(),
            )
        };
        if secret.is_empty() {
            return None;
        }
        let base = crate::remote_client::resolve_base_url(&primary, &fallback).await.ok()?;
        let label = match &reason {
            SwitchReason::InStreamRateLimit => "in_stream_rate_limit",
            SwitchReason::InStreamBanned => "in_stream_banned",
            _ => "http_429",
        };
        let outcome = crate::remote_client::request_switch(&base, &secret, current_id.as_deref(), label).await.ok()?;
        if outcome.exhausted { return None; }
        let new_current = outcome.current?;
        adopt_remote_current(state, &base, &secret, &new_current).await.ok()?;
        invalidate_remote_token_cache();
        let (new_token, _) = get_current_token(state).await.ok()?;
        // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
        let body_for_new = rewrite_prompt_cache_key(body, &new_current);
        let headers_no_ts = headers_without_turn_state(base_headers);
        let resp = forward_with_token(state, method, upstream_url, &headers_no_ts, &body_for_new, &new_token).await.ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        return Some(resp.bytes_stream().boxed());
    }

    // 本地模式
    let pick = pick_next_account(state);
    let PickResult::Found { id, token } = pick else { return None; };
    do_switch(state, &id, reason).ok()?;
    // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
    let body_for_new = rewrite_prompt_cache_key(body, &id);
    let headers_no_ts = headers_without_turn_state(base_headers);
    let resp = forward_with_token(state, method, upstream_url, &headers_no_ts, &body_for_new, &token).await.ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    Some(resp.bytes_stream().boxed())
}

/// body stream 任务：在 channel 上跑 bootstrap → forward 全过程。
/// 期间发心跳；嗅到 RateLimit/Banned 就静默切号继续。
async fn bootstrap_loop_task(
    state: Arc<ProxyState>,
    initial_upstream: ByteStream,
    method: hyper::Method,
    upstream_url: String,
    base_headers: reqwest::header::HeaderMap,
    body: Bytes,
    affinity_ctx: Option<AffinityCtx>,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, reqwest::Error>>,
) {
    let (byte_cap, time_cap_ms) = read_bootstrap_caps(&state);
    // SSE keep-alive 节奏 30s。codex 自己 idle_timeout 5 分钟（codex-rs/codex-api/src/
    // sse/responses.rs:372 + model-provider-info DEFAULT_STREAM_IDLE_TIMEOUT_MS=300_000），
    // 30s 心跳 = 5x 安全边界，比之前 1.5s 省 95% 心跳开销。
    let heartbeat = std::time::Duration::from_secs(30);
    let mut current_upstream = initial_upstream;
    let mut attempts: usize = 0;
    let mut capacity_attempts: usize = 0;
    const MAX_CAPACITY_RETRIES: usize = 3;

    loop {
        let outcome = bootstrap_with_heartbeats(
            current_upstream,
            byte_cap,
            time_cap_ms,
            &tx,
            heartbeat,
        )
        .await;

        match outcome {
            SseBootstrap::Ready { prefix, mut rest } => {
                if !prefix.is_empty() {
                    if tx.send(Ok(prefix)).await.is_err() { return; }
                }
                // forward rest：client 那头本来就在听 SSE 流；上游静默时继续发 keep-alive。
                // 关键：bootstrap 只能拦 turn 起点错误。turn 跑到中段（已 commit 内容）才出
                // capacity/限额错误时，bootstrap 窗口早就过了。这里**对每个 chunk 再嗅探一次**：
                // 检到 mid-stream 错误立即**截断流不转发**给 codex（不让"Selected model is at
                // capacity"原文到达 codex），codex 看到流意外断开会自动重试这个 turn
                // (codex-rs 自己的 DEFAULT_STREAM_MAX_RETRIES=5)。比让原文显示给用户好。
                let mut tail_buf: Vec<u8> = Vec::new();
                loop {
                    match tokio::time::timeout(heartbeat, rest.next()).await {
                        Ok(Some(Ok(chunk))) => {
                            // 累计最近 2KB 用于错误事件检测（够覆盖一个完整 SSE 事件）
                            tail_buf.extend_from_slice(&chunk);
                            if tail_buf.len() > 4096 {
                                let drop_n = tail_buf.len() - 2048;
                                tail_buf.drain(..drop_n);
                            }
                            match classify_sse_error(&tail_buf) {
                                SseErrorClass::PerAccountLimit => {
                                    println!("[Proxy] mid-stream 检测到 per-account 限额，截断流不转发原文，codex 会自动 retry");
                                    mark_current_quota_depleted(&state);
                                    return; // tx drop → client 看到 stream 意外结束 → codex retry
                                }
                                SseErrorClass::GlobalCapacity => {
                                    println!("[Proxy] mid-stream 检测到 global 容量满，截断流不转发原文，codex 会自动 retry");
                                    return;
                                }
                                SseErrorClass::None => {}
                            }
                            if tx.send(Ok(chunk)).await.is_err() { return; }
                        }
                        Ok(Some(Err(e))) => {
                            // 上游 chunk 错误，原样透给 client（让 codex 自己处理传输错误）
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                        Ok(None) => return,
                        Err(_) => {
                            if tx.send(Ok(Bytes::from_static(b": keep-alive\n\n"))).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            SseBootstrap::RateLimitInStream => {
                println!("[Proxy] SSE 流前缀检测到限额事件（response.failed），无损切号重发");
                mark_current_quota_depleted(&state);
                attempts += 1;
                if attempts >= MAX_429_RETRIES {
                    let _ = tx.send(Ok(Bytes::from_static(
                        b"event: response.failed\ndata: {\"error\":{\"message\":\"all accounts exhausted\",\"type\":\"usage_limit_reached\"}}\n\n",
                    ))).await;
                    return;
                }
                let new_up = match acquire_replacement_upstream(
                    &state, &method, &upstream_url, &base_headers, &body, SwitchReason::InStreamRateLimit,
                ).await {
                    Some(s) => s,
                    None => {
                        let _ = tx.send(Ok(Bytes::from_static(
                            b"event: response.failed\ndata: {\"error\":{\"message\":\"switch failed\"}}\n\n",
                        ))).await;
                        return;
                    }
                };
                // 切号了 → 重新构建 affinity_ctx 用新的 current；旧的 affinity_ctx 引用的还是失败号
                let _ = &affinity_ctx; // 占位以避免 unused 警告
                current_upstream = new_up;
                continue;
            }
            SseBootstrap::CapacityInStream => {
                // 上游全局过载，不是单号问题。先同号 backoff retry，撑不住才换号兜底。
                capacity_attempts += 1;
                if capacity_attempts <= MAX_CAPACITY_RETRIES {
                    let backoff = std::time::Duration::from_secs(2 * capacity_attempts as u64);
                    println!(
                        "[Proxy] 上游容量满（全局过载），{}s 后同号 retry ({}/{})",
                        backoff.as_secs(),
                        capacity_attempts,
                        MAX_CAPACITY_RETRIES
                    );
                    tokio::time::sleep(backoff).await;
                    // 用 store.current 的最新 token 在同账号上发新请求
                    let new_up = match acquire_same_account_upstream(
                        &state, &method, &upstream_url, &base_headers, &body,
                    )
                    .await
                    {
                        Some(s) => s,
                        None => {
                            let _ = tx.send(Ok(Bytes::from_static(
                                b"event: response.failed\ndata: {\"error\":{\"message\":\"upstream capacity retry failed\"}}\n\n",
                            ))).await;
                            return;
                        }
                    };
                    current_upstream = new_up;
                    continue;
                }
                // 同号 retry 达到上限 → 兜底走切号
                println!(
                    "[Proxy] 容量满 retry 用尽 ({} 次)，降级走切号兜底",
                    MAX_CAPACITY_RETRIES
                );
                attempts += 1;
                if attempts >= MAX_429_RETRIES {
                    let _ = tx.send(Ok(Bytes::from_static(
                        b"event: response.failed\ndata: {\"error\":{\"message\":\"upstream at capacity, all retries exhausted\"}}\n\n",
                    ))).await;
                    return;
                }
                let new_up = match acquire_replacement_upstream(
                    &state, &method, &upstream_url, &base_headers, &body, SwitchReason::Http429,
                ).await {
                    Some(s) => s,
                    None => {
                        let _ = tx.send(Ok(Bytes::from_static(
                            b"event: response.failed\ndata: {\"error\":{\"message\":\"switch failed after capacity\"}}\n\n",
                        ))).await;
                        return;
                    }
                };
                capacity_attempts = 0; // 切号后 capacity 计数重置
                current_upstream = new_up;
                continue;
            }
            SseBootstrap::BannedInStream => {
                println!("[Proxy] SSE 流前缀检测到封号事件，标记并无损切号重发");
                mark_current_banned(&state);
                attempts += 1;
                if attempts >= MAX_429_RETRIES {
                    let _ = tx.send(Ok(Bytes::from_static(
                        b"event: response.failed\ndata: {\"error\":{\"message\":\"all accounts banned\"}}\n\n",
                    ))).await;
                    return;
                }
                let new_up = match acquire_replacement_upstream(
                    &state, &method, &upstream_url, &base_headers, &body, SwitchReason::InStreamBanned,
                ).await {
                    Some(s) => s,
                    None => {
                        let _ = tx.send(Ok(Bytes::from_static(
                            b"event: response.failed\ndata: {\"error\":{\"message\":\"switch failed\"}}\n\n",
                        ))).await;
                        return;
                    }
                };
                current_upstream = new_up;
                continue;
            }
        }
    }
}

/// 立刻返回 Response（status 200 + 上游 SSE headers），body 是后台 bootstrap+forward 任务驱动的 channel 流。
fn build_streaming_response_with_bootstrap(
    state: Arc<ProxyState>,
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    initial_upstream: ByteStream,
    method: hyper::Method,
    upstream_url: String,
    base_headers: reqwest::header::HeaderMap,
    body_bytes: Bytes,
    affinity_ctx: Option<AffinityCtx>,
) -> Response<ProxyBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, reqwest::Error>>(8);

    tokio::spawn(bootstrap_loop_task(
        state.clone(),
        initial_upstream,
        method,
        upstream_url,
        base_headers,
        body_bytes,
        affinity_ctx.clone(),
        tx,
    ));

    let body_stream: ByteStream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed();

    // 复用 build_stream_response_from_parts —— prefix 留空（已经从 task 经 tx 进了 rx），
    // 同时也能拿到 usage 提取 + affinity 记账的 end_signal。
    build_stream_response_from_parts(
        status,
        headers,
        Bytes::new(),
        body_stream,
        Some(state.tracker.clone()),
        affinity_ctx,
    )
}

/// SSE 流式响应构建：复制 header + 流式传输 body + 后台提取 usage
fn build_stream_response(
    upstream_resp: reqwest::Response,
    tracker: Option<Arc<TokenTracker>>,
    affinity_ctx: Option<AffinityCtx>,
) -> Response<ProxyBody> {
    let status = upstream_resp.status();
    let headers = upstream_resp.headers().clone();
    let stream = upstream_resp.bytes_stream().boxed();
    build_stream_response_from_parts(status, headers, Bytes::new(), stream, tracker, affinity_ctx)
}

// ────────────────────────────────────────────────────────────────
// SSE 审查事件嗅探（采样阶段）
// 目标：上游 SSE 里出现 OpenAI 内容审查信号（cybersecurity flag /
// content_policy_violation / refusal / content_filter）时，把整段
// SSE 体落盘到独立样本文件，便于后续写精准过滤。每条流第一次命中
// 即 dump，避免巨流多次写盘；采样阶段不修改流本身。
// ────────────────────────────────────────────────────────────────

const MODERATION_SNIFF_KEYWORDS: &[&str] = &[
    "flagged for possible cybersecurity",
    "chatgpt.com/cyber",
    "trusted access for cyber",
    "content_policy_violation",
    "content_filter",
    "\"refusal\"",
    "response.refusal",
    "incomplete_details",
];

fn moderation_sniff_hit(window_lower: &str) -> Option<&'static str> {
    MODERATION_SNIFF_KEYWORDS
        .iter()
        .copied()
        .find(|kw| window_lower.contains(kw))
}

fn dump_moderation_sample(buf: Arc<Mutex<Vec<u8>>>, hit_kw: &'static str) {
    tokio::spawn(async move {
        let snapshot = match buf.lock() {
            Ok(b) => b.clone(),
            Err(_) => return,
        };
        let Some(home) = dirs::home_dir() else { return };
        let dir = home.join(".codex-switcher").join("moderation-samples");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("[Proxy] moderation 样本目录创建失败: {}", e);
            return;
        }
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
        let path = dir.join(format!("sample-{}.log", ts));
        let header = format!(
            "# moderation sniff hit: {}\n# captured_at: {}\n# bytes: {}\n# ───────────────────────────────────────────\n",
            hit_kw,
            chrono::Utc::now().to_rfc3339(),
            snapshot.len()
        );
        let mut payload = Vec::with_capacity(header.len() + snapshot.len());
        payload.extend_from_slice(header.as_bytes());
        payload.extend_from_slice(&snapshot);
        if let Err(e) = std::fs::write(&path, &payload) {
            eprintln!("[Proxy] moderation 样本写盘失败: {}", e);
            return;
        }
        println!(
            "[Proxy] 命中审查关键词「{}」→ 样本已落盘: {}",
            hit_kw,
            path.display()
        );
    });
}

/// 同上，但允许传入已经缓冲的 prefix（bootstrap 阶段读到的字节），prefix 先发再接 rest。
fn build_stream_response_from_parts(
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    prefix: Bytes,
    rest: ByteStream,
    tracker: Option<Arc<TokenTracker>>,
    affinity_ctx: Option<AffinityCtx>,
) -> Response<ProxyBody> {
    let mut builder = Response::builder().status(status.as_u16());

    for (name, value) in &headers {
        if matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "trailer" | "upgrade"
        ) {
            continue;
        }
        if let Ok(hn) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(hv) = HeaderValue::from_bytes(value.as_bytes()) {
                builder = builder.header(hn, hv);
            }
        }
    }

    let usage_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    if !prefix.is_empty() {
        if let Ok(mut b) = usage_buf.lock() {
            b.extend_from_slice(&prefix);
        }
    }
    let buf_clone = usage_buf.clone();
    let tracker_clone = tracker.clone();
    // 仅 200 + SSE 才嗅探审查事件，其他状态码（4xx/5xx 错误体）跳过。
    let moderation_sniff_enabled = status == reqwest::StatusCode::OK
        && headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_lowercase().contains("event-stream"))
            .unwrap_or(false);
    let moderation_dumped = Arc::new(AtomicBool::new(false));
    let moderation_dumped_clone = moderation_dumped.clone();
    let moderation_buf_clone = usage_buf.clone();
    // 命中后如果 prefix 里就有关键词，开流瞬间就 dump 一次（不等首个 chunk）
    if moderation_sniff_enabled && !prefix.is_empty() {
        let prefix_lower = String::from_utf8_lossy(&prefix).to_lowercase();
        if let Some(kw) = moderation_sniff_hit(&prefix_lower) {
            if !moderation_dumped.swap(true, Ordering::Relaxed) {
                dump_moderation_sample(usage_buf.clone(), kw);
            }
        }
    }

    // 把 prefix 当作流的第一个 chunk 先吐出去（空 prefix 时跳过）
    let prefix_stream: ByteStream = if prefix.is_empty() {
        futures_util::stream::empty().boxed()
    } else {
        futures_util::stream::once(async move { Ok(prefix) }).boxed()
    };
    // 给 rest 套一层 SSE keep-alive：上游 30s 没新 chunk 就自动塞一行 SSE 注释
    // (": keep-alive\n\n")，client 解析器忽略，但 TCP 连接和 codex 那头的读循环都不会超时。
    // codex 自己的 idle_timeout 是 5 分钟（DEFAULT_STREAM_IDLE_TIMEOUT_MS=300_000），
    // 30s 心跳给 5x 安全边界够。
    let rest_with_heartbeat = wrap_with_sse_heartbeat(
        rest,
        std::time::Duration::from_secs(30),
    );
    let raw_stream = prefix_stream.chain(rest_with_heartbeat);

    // ── shell tool 输出压缩（SSE 路径）──
    // 若 settings.output_compression_enabled=true：对 SSE event 边界做切分，
    // 在每个 `data: ...\n\n` 单元内尝试解析 + 重写 tool_result / function_call_output 的 output。
    // SSE 流最终保留事件结构，下游解析照常工作。
    // 默认就开（settings 默认 true）；false 时直接走 raw_stream 不再绕一道。
    let (compress_enabled, compress_threshold) = {
        // 同 read_compress_settings 但这里没有 ProxyState，直接读 store 的全局——
        // 不过 store 不在 affinity_ctx 里。简单起见：用 OnceLock fallback (默认开 + 8KB)。
        // build_stream_response_from_parts 是个工具函数；调用方已通过 ProxyState 持有 store；
        // 我们这里不增加额外参数，转而通过 lib::compress_stats 同源的全局 settings 做轻量缓存。
        compress_settings_snapshot()
    };
    let raw_stream: ByteStream = if compress_enabled {
        sse_compress_stream(raw_stream.boxed(), compress_threshold).boxed()
    } else {
        raw_stream.boxed()
    };

    let stream = raw_stream.map(move |result| match result {
        Ok(bytes) => {
            if let Ok(mut buf) = buf_clone.lock() {
                buf.extend_from_slice(&bytes);
                // 审查嗅探：每条 SSE 流第一次命中即异步 dump 整段 buf。
                // 扫描窗口 = 新 chunk + 16KB 尾巴（防关键词跨 chunk 切断），
                // 避免对整段缓冲做 O(n²) 重扫。
                if moderation_sniff_enabled
                    && !moderation_dumped_clone.load(Ordering::Relaxed)
                {
                    let tail_overlap = 16 * 1024;
                    let scan_start = buf.len().saturating_sub(bytes.len() + tail_overlap);
                    let window_lower =
                        String::from_utf8_lossy(&buf[scan_start..]).to_lowercase();
                    if let Some(kw) = moderation_sniff_hit(&window_lower) {
                        if !moderation_dumped_clone.swap(true, Ordering::Relaxed) {
                            dump_moderation_sample(moderation_buf_clone.clone(), kw);
                        }
                    }
                }
            }
            Ok(Frame::data(bytes))
        }
        Err(e) => Err(e.to_string()),
    });

    let buf_for_end = usage_buf;
    let affinity_clone = affinity_ctx.clone();
    let end_signal = futures_util::stream::once(async move {
        if let Ok(buf) = buf_for_end.lock() {
            if !buf.is_empty() {
                if let Some(mut usage) = crate::token_tracker::extract_usage_from_sse(&buf, "") {
                    let cache_pct = if usage.input_tokens > 0 {
                        (usage.cached_input_tokens as f64 / usage.input_tokens as f64) * 100.0
                    } else {
                        0.0
                    };
                    let account_id_for_record = affinity_clone
                        .as_ref()
                        .map(|c| c.account_id.clone())
                        .unwrap_or_default();
                    println!(
                        "[Proxy] Token: input={} cached={} ({:.0}%) output={} total={} model={} account={}",
                        usage.input_tokens,
                        usage.cached_input_tokens,
                        cache_pct,
                        usage.output_tokens,
                        usage.total_tokens,
                        usage.model,
                        if account_id_for_record.is_empty() { "?" } else { &account_id_for_record }
                    );
                    // Evidence-based affinity：只在 cached_tokens > 0 时把 session 黏到当前号
                    if let Some(ctx) = &affinity_clone {
                        if usage.cached_input_tokens > 0 {
                            ctx.affinity.record_cache_hit(
                                &ctx.session_key,
                                &ctx.account_id,
                                usage.cached_input_tokens,
                            );
                        }
                    }
                    usage.account_id = account_id_for_record;
                    if let Some(tracker) = tracker_clone {
                        tracker.record(usage);
                    }
                }
            }
        }
        Err("".to_string())
    })
    .filter(|_| futures_util::future::ready(false));

    let combined = stream.chain(end_signal);

    builder
        .body(BodyExt::boxed_unsync(StreamBody::new(combined)))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "流构建失败"))
}

/// Full body 包装（用于错误响应等小数据）
fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes).map_err(|_| String::new()).boxed_unsync()
}

// ────────────────────────────────────────────────────────────────
// WebSocket 代理
// ────────────────────────────────────────────────────────────────

/// 检测是否为 WebSocket 升级请求
fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_lowercase().contains("websocket"))
        .unwrap_or(false)
}

/// 处理 WebSocket 代理：连接上游 + 双向桥接
async fn handle_websocket(
    state: Arc<ProxyState>,
    mut req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    // 1. 获取 token 和上游地址
    let (mut token, mut is_chatgpt) = match get_current_token(&state).await {
        Ok(t) => t,
        Err(e) => return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, &e)),
    };

    // 预检：如果当前账号没额度，先切号再连接
    {
        let should_switch = {
            let store = match state.store.lock() {
                Ok(s) => s,
                Err(_) => return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, "锁失败")),
            };
            if let Some(current_id) = &store.current {
                store
                    .accounts
                    .get(current_id)
                    .and_then(|a| {
                        // current 是 Relay 时按设置决定是否预检切走
                        if a.is_relay() && !store.settings.relay_auto_switch_out {
                            return Some(false);
                        }
                        if a.is_banned || a.is_token_invalid || a.is_logged_out {
                            return Some(true);
                        }
                        a.cached_quota.as_ref().map(|q| {
                            let is_free = q.plan_type.to_lowercase() == "free";
                            if is_free {
                                q.five_hour_left <= 0.0
                            } else {
                                q.five_hour_left <= 0.0 || q.weekly_left <= 0.0
                            }
                        })
                    })
                    .unwrap_or(false)
            } else {
                false
            }
        };

        if should_switch {
            println!("[Proxy] WebSocket 预检：当前账号无额度，尝试切号...");
            // 最多尝试 3 个候选号，查 API 确认有额度才切
            for _attempt in 0..3 {
                if let PickResult::Found {
                    id,
                    token: new_token,
                } = pick_next_account(&state)
                {
                    // 查 API 确认候选号是否真的有额度
                    let has_quota = {
                        let (at, aid, rt) = {
                            let store = state.store.lock().map_err(|e| e.to_string()).ok();
                            if let Some(s) = store {
                                let acc = s.accounts.get(&id);
                                acc.map(|a| {
                                    (
                                        AccountStore::extract_access_token(&a.auth_json),
                                        AccountStore::extract_account_id(&a.auth_json),
                                        a.refresh_token.clone(),
                                    )
                                })
                                .unwrap_or((None, None, None))
                            } else {
                                (None, None, None)
                            }
                        };
                        if let Some(access_token) = at {
                            match crate::usage::UsageFetcher::fetch_usage_direct(
                                access_token,
                                aid,
                                rt,
                                false,
                            )
                            .await
                            {
                                Ok((usage, _)) => {
                                    // 更新缓存
                                    if let Ok(mut store) = state.store.lock() {
                                        if let Some(acc) = store.accounts.get_mut(&id) {
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
                                            let _ = store.save();
                                        }
                                    }
                                    usage.five_hour_left > 0 && usage.weekly_left > 0
                                }
                                Err(e) => {
                                    println!("[Proxy] 预检查询候选号额度失败: {}", e);
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    };

                    if has_quota {
                        if do_switch(&state, &id, SwitchReason::WebSocketPrecheck).is_ok() {
                            is_chatgpt = new_token.starts_with("eyJ");
                            token = new_token;
                            println!("[Proxy] WebSocket 预检切号成功（已确认有额度）");
                        }
                        break;
                    } else {
                        println!("[Proxy] 候选号无额度，跳过继续找...");
                        // 标记为耗尽，下次不再选
                        mark_account_quota_depleted(&state, &id);
                    }
                } else {
                    println!("[Proxy] 无可用候选号");
                    break;
                }
            }
        }
    }

    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let relay_base_url = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.current.clone())
        .and_then(|id| account_relay_base_url(&state, &id));
    let (http_url, _upstream_host) =
        get_upstream(is_chatgpt, relay_base_url.as_deref(), &path);

    // http(s):// → ws(s)://
    let ws_url = http_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    println!("[Proxy] → upstream WS: {}", ws_url);

    // 2. 构建上游 WebSocket 请求（透明 header 转发 + token 注入）
    let mut upstream_req: tungstenite::http::Request<()> =
        match ws_url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("WebSocket 请求构建失败: {}", e),
                ))
            }
        };

    // 转发客户端 header（排除 WebSocket 握手专用 header，由 into_client_request 生成）
    for (name, value) in req.headers() {
        let lower = name.as_str().to_lowercase();
        if matches!(
            lower.as_str(),
            "authorization"
                | "host"
                | "upgrade"
                | "connection"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-extensions"
        ) {
            continue;
        }
        upstream_req
            .headers_mut()
            .insert(name.clone(), value.clone());
    }

    // 注入 token
    if let Ok(auth_val) = HeaderValue::from_str(&format!("Bearer {}", token)) {
        upstream_req
            .headers_mut()
            .insert(hyper::header::AUTHORIZATION, auth_val);
    }

    // 3. 连接上游 WebSocket（认证失败时自动切号重连）
    let connect_result = tokio_tungstenite::connect_async(upstream_req).await;

    let (upstream_ws, upstream_handshake_resp) = match connect_result {
        Ok(conn) => conn,
        Err(e) => {
            let err_lower = e.to_string().to_lowercase();
            let is_auth_err = err_lower.contains("401")
                || err_lower.contains("403")
                || err_lower.contains("unauthorized")
                || err_lower.contains("forbidden");
            // 区分两类拒绝：
            //   - per-account 限额（429 / rate_limit 关键词 / 401/403）→ 切号
            //   - global 容量满（503 / "at capacity" / "model overloaded"）→ **同号 backoff retry**
            //     不是单号问题，切号也是撞同样错；同号等几秒再试通常能继续
            // 真正网络层错误（DNS / TLS / connect refused 等）才报给 client。
            let is_global_capacity = err_lower.contains("503")
                || matches_global_capacity(&err_lower);
            let is_per_account_limit = err_lower.contains("429")
                || PER_ACCOUNT_LIMIT_KEYWORDS.iter().any(|kw| err_lower.contains(kw));

            if !is_auth_err && !is_global_capacity && !is_per_account_limit
                && !err_lower.contains("502") && !err_lower.contains("504")
            {
                eprintln!("[Proxy] WebSocket 上游连接失败（网络层）: {}", e);
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "WebSocket 上游连接失败",
                ));
            }

            // global 容量满：先尝试同号 backoff retry，3 次失败再降级走切号
            if is_global_capacity && !is_auth_err && !is_per_account_limit {
                println!(
                    "[Proxy] WebSocket 握手被上游拒绝（容量满），同号 backoff retry..."
                );
                let mut same_account_retry = None;
                for attempt in 1..=3u64 {
                    let backoff = std::time::Duration::from_secs(2 * attempt);
                    tokio::time::sleep(backoff).await;
                    let cur_token = match get_current_token(&state).await {
                        Ok((t, _)) => t,
                        Err(_) => continue,
                    };
                    let cur_chatgpt = cur_token.starts_with("eyJ");
                    let cur_relay = state
                        .store
                        .lock()
                        .ok()
                        .and_then(|s| s.current.clone())
                        .and_then(|id| account_relay_base_url(&state, &id));
                    let (cur_url, _) =
                        get_upstream(cur_chatgpt, cur_relay.as_deref(), &path);
                    let ws = cur_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
                    if let Ok(mut r) = ws.as_str().into_client_request() {
                        for (n, v) in req.headers() {
                            let l = n.as_str().to_lowercase();
                            if matches!(l.as_str(), "authorization"|"host"|"upgrade"|"connection"|"sec-websocket-key"|"sec-websocket-version"|"sec-websocket-extensions") { continue; }
                            r.headers_mut().insert(n.clone(), v.clone());
                        }
                        if let Ok(av) = HeaderValue::from_str(&format!("Bearer {}", cur_token)) {
                            r.headers_mut().insert(hyper::header::AUTHORIZATION, av);
                        }
                        if let Ok(c) = tokio_tungstenite::connect_async(r).await {
                            println!("[Proxy] 容量满同号 retry 第 {} 次成功", attempt);
                            same_account_retry = Some(c);
                            break;
                        }
                    }
                }
                if let Some(conn) = same_account_retry {
                    conn
                } else {
                    println!("[Proxy] 容量满同号 retry 三次都失败，降级走切号兜底");
                    // 落到切号逻辑（fall through 不行，得显式调）
                    let mut retry_conn = None;
                    for _attempt in 0..3 {
                        if let PickResult::Found { id, token: new_tok } = pick_next_account(&state) {
                            if do_switch(&state, &id, SwitchReason::WebSocketPrecheck).is_err() { continue; }
                            let new_chatgpt = new_tok.starts_with("eyJ");
                            let new_relay = account_relay_base_url(&state, &id);
                            let (new_url, _) =
                                get_upstream(new_chatgpt, new_relay.as_deref(), &path);
                            let ws = new_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
                            if let Ok(mut r) = ws.as_str().into_client_request() {
                                for (n, v) in req.headers() {
                                    let l = n.as_str().to_lowercase();
                                    if matches!(l.as_str(), "authorization"|"host"|"upgrade"|"connection"|"sec-websocket-key"|"sec-websocket-version"|"sec-websocket-extensions") { continue; }
                                    r.headers_mut().insert(n.clone(), v.clone());
                                }
                                if let Ok(av) = HeaderValue::from_str(&format!("Bearer {}", new_tok)) {
                                    r.headers_mut().insert(hyper::header::AUTHORIZATION, av);
                                }
                                if let Ok(c) = tokio_tungstenite::connect_async(r).await {
                                    retry_conn = Some(c);
                                    break;
                                }
                            }
                        } else { break; }
                    }
                    match retry_conn {
                        Some(conn) => conn,
                        None => {
                            return Ok(error_response(
                                StatusCode::BAD_GATEWAY,
                                "上游容量满且重试无果",
                            ));
                        }
                    }
                }
            } else {
                // per-account 限额 / auth：直接走切号
                println!(
                    "[Proxy] WebSocket 握手被上游拒绝 ({})，切号重连...",
                    if is_auth_err { "auth" } else { "per-account 限额" }
                );
                mark_current_quota_depleted(&state);

                // 切号重连，最多试 3 个号；连不上也算这个号当前不可用，标 depleted 避免重复挑
                let mut retry_conn = None;
            for _attempt in 0..3 {
                if let PickResult::Found { id, token: new_tok } = pick_next_account(&state) {
                    if do_switch(&state, &id, SwitchReason::WebSocketPrecheck).is_err() {
                        continue;
                    }
                    let new_chatgpt = new_tok.starts_with("eyJ");
                    let new_relay = account_relay_base_url(&state, &id);
                    let (new_url, _) =
                        get_upstream(new_chatgpt, new_relay.as_deref(), &path);
                    let ws = new_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);

                    if let Ok(mut r) = ws.as_str().into_client_request() {
                        for (n, v) in req.headers() {
                            let l = n.as_str().to_lowercase();
                            if matches!(l.as_str(), "authorization"|"host"|"upgrade"|"connection"|"sec-websocket-key"|"sec-websocket-version"|"sec-websocket-extensions") { continue; }
                            r.headers_mut().insert(n.clone(), v.clone());
                        }
                        if let Ok(av) = HeaderValue::from_str(&format!("Bearer {}", new_tok)) {
                            r.headers_mut().insert(hyper::header::AUTHORIZATION, av);
                        }
                        match tokio_tungstenite::connect_async(r).await {
                            Ok(c) => {
                                println!("[Proxy] WebSocket 切号重连成功（{}）", id);
                                retry_conn = Some(c);
                                break;
                            }
                            Err(e2) => {
                                println!(
                                    "[Proxy] 切号到 {} 后仍连不上（{}），继续试下一个",
                                    id, e2
                                );
                                // 这个号也用不了 → 标耗尽，下一轮 pick 不会再选
                                mark_current_quota_depleted(&state);
                            }
                        }
                    }
                } else {
                    break;
                }
            }

            match retry_conn {
                Some(conn) => conn,
                None => {
                    return Ok(error_response(
                        StatusCode::BAD_GATEWAY,
                        "所有账号 WebSocket 连接均失败",
                    ));
                }
            }
            }  // 关闭 is_global_capacity 的 else 分支
        }
    };

    println!("[Proxy] WebSocket 上游已连接");

    // 4. 计算 Sec-WebSocket-Accept 回复客户端
    let ws_key = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let accept_key = tungstenite::handshake::derive_accept_key(ws_key.as_bytes());

    // 5. 提取 hyper upgrade handle（必须在返回 101 之前）
    let on_upgrade = hyper::upgrade::on(&mut req);

    // 6. 构建 101 响应，转发上游的响应 header（x-codex-turn-state 等）
    let mut response_builder = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", &accept_key);

    // 转发上游响应 header（排除 WebSocket 握手 header）
    for (name, value) in upstream_handshake_resp.headers() {
        let lower = name.as_str().to_lowercase();
        if matches!(
            lower.as_str(),
            "upgrade"
                | "connection"
                | "sec-websocket-accept"
                | "sec-websocket-extensions"
                | "content-length"
                | "transfer-encoding"
        ) {
            continue;
        }
        if let Ok(hn) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            response_builder = response_builder.header(hn, value.clone());
        }
    }

    let response = response_builder
        .body(full_body(Bytes::new()))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "101 构建失败"));

    // 7. 后台任务：upgrade 完成后双向桥接
    let disconnect = state.ws_disconnect.clone();
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let io = TokioIo::new(upgraded);
                let mut client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    io,
                    tungstenite::protocol::Role::Server,
                    None,
                )
                .await;

                println!("[Proxy] WebSocket 客户端已升级，开始桥接");

                // 检查是否有待注入的切号通知消息
                let inject_text = PENDING_INJECT_MSG.lock().ok().and_then(|mut m| m.take());
                if let Some(msg_text) = inject_text {
                    let inject_json = serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": format!("\n{}\n", msg_text)
                    });
                    let _ = futures_util::SinkExt::send(
                        &mut client_ws,
                        tungstenite::Message::Text(inject_json.to_string().into()),
                    )
                    .await;
                    println!("[Proxy] 已注入切号通知到 WebSocket");
                }

                bridge_websockets(client_ws, upstream_ws, disconnect, state).await;
                println!("[Proxy] WebSocket 连接已关闭");
            }
            Err(e) => eprintln!("[Proxy] WebSocket upgrade 失败: {}", e),
        }
    });

    Ok(response)
}

// ────────────────────────────────────────────────────────────────
// 错误关键词分类
// ────────────────────────────────────────────────────────────────
//
// 把信号分成两类，因为它们应该走不同的恢复策略：
//
//   PER-ACCOUNT 限额：当前号已经把自己 5h / weekly / TPM / RPM 消耗完了。
//   同号再 retry 还是同样错误。**必须切号**才能继续。
//
//   GLOBAL 容量满：OpenAI 那头模型池子过载（gpt-5-codex 一波拥堵之类）。
//   跟账号无关，所有号都会撞同一个错。同号短暂等几秒再试通常就能继续。
//   **不应该切号**，浪费账号还触发不必要的额度耗尽标记。

/// per-account 限额信号 —— 命中即切号
///
/// wire 上真实的错误码（codex-rs/codex-api/src/sse/responses.rs:543-557 +
/// api_bridge.rs:80-100）：
/// - `usage_limit_reached` — 该号 plan 配额耗尽 → CodexErr::UsageLimitReached（fatal）
/// - `insufficient_quota` — 同上 → CodexErr::QuotaExceeded（fatal）
/// - `usage_not_included` — 该号订阅不含 codex → CodexErr::UsageNotIncluded（fatal）
/// codex 都不会自己 retry，proxy 必须切号。
const PER_ACCOUNT_LIMIT_KEYWORDS: &[&str] = &[
    "usage_limit_reached", // ★ HTTP 429 / SSE response.failed 真实 code
    "usage_not_included",  // ★ 该号订阅不含 codex
    "insufficient_quota",  // ★ SSE response.failed 真实 code
    "rate_limit",
    "rate limit",
    "usage_limit",
    "usage limit",
    "too many requests",
    "billing_hard_limit",
    "tokens per min",
    "requests per min",
    "hit your usage limit",
    "upgrade to plus", // free 号耗尽时的劝升级提示
];

/// global 容量满信号 —— 命中应该同号 backoff retry，不切号
///
/// 关键：codex 二进制识别的真正错误码是 `server_is_overloaded`（注意中间的 `_is_`）和
/// `slow_down`，证据：codex-rs/codex-api/src/sse/responses.rs:566-570 +
/// api_bridge.rs:45-56。`Selected model is at capacity. Please try a different model.`
/// 是 codex 内部硬编码字符串（protocol/src/error.rs:111），不会出现在 wire data 里。
/// 所以匹配 wire 数据必须包含 server_is_overloaded / slow_down。
const GLOBAL_CAPACITY_KEYWORDS: &[&str] = &[
    "server_is_overloaded",  // ★ codex 真正认的错误码
    "slow_down",             // ★ 同上
    "at capacity",
    "selected model is at capacity",
    "try a different model",
    "model_overloaded",
    "model overloaded",
    "service unavailable",
];

/// 任意限额/容量信号（任一类都算"上游不让发"）
fn matches_any_limit_signal(lower: &str) -> bool {
    PER_ACCOUNT_LIMIT_KEYWORDS.iter().any(|kw| lower.contains(kw))
        || GLOBAL_CAPACITY_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

fn matches_global_capacity(lower: &str) -> bool {
    GLOBAL_CAPACITY_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// 老 const，保留是为了避免改太多调用点；语义保持"任何限额/容量信号"。
const RATE_LIMIT_KEYWORDS: &[&str] = &[
    // codex 二进制实际识别的错误码（codex-rs/codex-api/src/sse/responses.rs:543-570）
    "server_is_overloaded",  // ★ 全局 capacity（中间有 _is_）
    "slow_down",             // ★ 全局 capacity
    "usage_limit_reached",   // ★ per-account 配额耗尽
    "usage_not_included",    // ★ 该号订阅不含 codex
    "insufficient_quota",    // ★ per-account 配额
    "rate_limit",
    "rate limit",
    "usage_limit",
    "usage limit",
    "too many requests",
    "billing_hard_limit",
    "tokens per min",
    "requests per min",
    "at capacity",
    "selected model is at capacity",
    "try a different model",
    "model_overloaded",
    "model overloaded",
    "service unavailable",
    "hit your usage limit",
    "upgrade to plus",
];

fn detect_ws_rate_limit(msg: &tungstenite::Message) -> bool {
    if let tungstenite::Message::Text(ref text) = msg {
        let lower = text.to_lowercase();

        // 快速文本匹配
        let matched = RATE_LIMIT_KEYWORDS.iter().any(|kw| lower.contains(kw));
        if !matched {
            return false;
        }

        // 打印命中关键词的消息前 400 字节用于排错
        let preview: String = text.chars().take(400).collect();
        println!("[Proxy] WS 消息包含限额关键词，msg 预览: {}", preview);

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // response.failed / error 类型直接判定
            if msg_type == "response.failed" || msg_type == "error" {
                println!("[Proxy] WS 限额: type={}", msg_type);
                return true;
            }

            // 有 error 字段也判定
            if val.get("response").and_then(|r| r.get("error")).is_some()
                || val.get("error").is_some()
            {
                println!("[Proxy] WS 限额: 有 error 字段");
                return true;
            }
            // 还有一种格式：response.failed 在 outer，error 在 inner（OpenAI 实际格式）
            // {"type":"response.failed","sequence_number":N,"response":{"id":..,"status":"failed","error":{"code":"server_overloaded","message":"..."}}}
            // 已经被前两个 case 覆盖，但加个明确的日志方便排错
            println!(
                "[Proxy] WS 限额关键词命中但 JSON 字段不匹配（type={}）",
                msg_type
            );
        }

        // JSON 解析失败或没有 error 字段，但文本明确包含限额/容量满消息
        // 注意：server_is_overloaded 才是 codex 真正认的（有 _is_），不是 server_overloaded
        if lower.contains("server_is_overloaded")
            || lower.contains("slow_down")
            || lower.contains("usage_limit_reached")
            || lower.contains("usage_not_included")
            || lower.contains("insufficient_quota")
            || lower.contains("hit your usage limit")
            || lower.contains("rate limit reached")
            || lower.contains("too many requests")
            || lower.contains("at capacity")
            || lower.contains("try a different model")
            || lower.contains("model overloaded")
        {
            println!("[Proxy] WS 限额/容量满: 文本兜底匹配");
            return true;
        }
    }
    false
}

/// 封号关键词
const BANNED_KEYWORDS: &[&str] = &[
    "deactivated",
    "banned",
    "suspended",
    "account_deactivated",
    "deactivated_workspace",
];

fn detect_ws_banned(msg: &tungstenite::Message) -> bool {
    if let tungstenite::Message::Text(ref text) = msg {
        let lower = text.to_lowercase();

        // 快速文本匹配
        if !BANNED_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            return false;
        }

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if msg_type == "response.failed" || msg_type == "error" {
                return true;
            }

            // error 字段里包含封号关键词
            if val.get("response").and_then(|r| r.get("error")).is_some()
                || val.get("error").is_some()
            {
                return true;
            }
        }
    }
    false
}

/// 双向桥接两个 WebSocket 连接
/// - 切号信号 → 断开连接
/// - 检测到限额/封号消息 → 断开连接（代理会在下次连接时预检切号）
async fn bridge_websockets<S1, S2>(
    client: S1,
    upstream: S2,
    disconnect: Arc<tokio::sync::Notify>,
    state: Arc<ProxyState>,
) where
    S1: futures_util::Stream<Item = Result<tungstenite::Message, tungstenite::Error>>
        + futures_util::Sink<tungstenite::Message, Error = tungstenite::Error>
        + Unpin,
    S2: futures_util::Stream<Item = Result<tungstenite::Message, tungstenite::Error>>
        + futures_util::Sink<tungstenite::Message, Error = tungstenite::Error>
        + Unpin,
{
    let (mut client_write, mut client_read) = client.split();
    let (mut upstream_write, mut upstream_read) = upstream.split();

    // 在桥接期间记录该 WS 会话用的 session_key（从 client→upstream 的请求消息里提取）
    // 用于在 response.completed 时记 affinity binding。
    let ws_session_key: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let ws_session_key_w = ws_session_key.clone();

    let client_to_upstream = async {
        while let Some(msg) = client_read.next().await {
            match msg {
                Ok(msg) => {
                    if msg.is_close() {
                        let _ = upstream_write.send(msg).await;
                        break;
                    }
                    // 嗅探 client→upstream 的 JSON 文本，提取 session_key（首次命中即固定）
                    if let tungstenite::Message::Text(ref t) = msg {
                        if ws_session_key_w.lock().map(|g| g.is_none()).unwrap_or(false) {
                            let bytes = t.as_bytes();
                            if let Some(sk) = crate::session_affinity::extract_session_key(
                                bytes,
                                &reqwest::header::HeaderMap::new(),
                            ) {
                                if let Ok(mut g) = ws_session_key_w.lock() {
                                    *g = Some(sk);
                                }
                            }
                        }
                    }
                    if upstream_write.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    let state_clone = state.clone();
    let ws_session_key_r = ws_session_key.clone();
    let upstream_to_client = async {
        while let Some(msg) = upstream_read.next().await {
            match msg {
                Ok(msg) => {
                    // 检测错误：**不要把错误消息转发给 client**（之前的 bug），
                    // 区分两种 case：
                    //   - per-account 限额：本号已耗尽 → 切号 + 关 WS
                    //   - global 容量满：上游全局过载 → 不切号，只关 WS
                    //     codex App 会自动重连，下一次握手如果还容量满就在握手层
                    //     进 backoff retry（同号），别浪费账号
                    if detect_ws_rate_limit(&msg) {
                        let is_capacity_only = if let tungstenite::Message::Text(ref t) = msg {
                            let lower = t.to_lowercase();
                            matches_global_capacity(&lower)
                                && !PER_ACCOUNT_LIMIT_KEYWORDS.iter().any(|kw| lower.contains(kw))
                        } else {
                            false
                        };
                        if is_capacity_only {
                            println!(
                                "[Proxy] WebSocket 上游容量满（全局过载），关 WS 不切号 → 等 codex App 重连"
                            );
                        } else {
                            println!(
                                "[Proxy] WebSocket 单号限额，静默切号 + 关 WS"
                            );
                            mark_current_quota_depleted(&state_clone);
                            if let PickResult::Found { id, .. } = pick_next_account(&state_clone) {
                                let _ = do_switch(&state_clone, &id, SwitchReason::WebSocketRateLimit);
                            }
                        }
                        // 关 client 侧 WS，让 Codex App 干净断开 + 自动重连
                        let _ = client_write.send(tungstenite::Message::Close(None)).await;
                        break;
                    }
                    // 检测封号
                    if detect_ws_banned(&msg) {
                        println!("[Proxy] WebSocket 检测到封号，静默切号 + 关 WS（不透回 codex）...");
                        mark_current_banned(&state_clone);
                        if let PickResult::Found { id, .. } = pick_next_account(&state_clone) {
                            let _ = do_switch(&state_clone, &id, SwitchReason::BannedDetected);
                        }
                        let _ = client_write.send(tungstenite::Message::Close(None)).await;
                        break;
                    }

                    // 在 response.completed 文本里抽 usage：记 token_tracker + (可选) 记 affinity
                    if let tungstenite::Message::Text(ref t) = msg {
                        if t.contains("response.completed") {
                            // WS 消息是裸 JSON，不是 "data: ..." SSE 行；预处理一下让
                            // extract_usage_from_sse 能复用
                            let wrapped = format!("data: {}\n\n", t);
                            if let Some(mut usage) = crate::token_tracker::extract_usage_from_sse(wrapped.as_bytes(), "") {
                                let cur_id = state_clone
                                    .store
                                    .lock()
                                    .ok()
                                    .and_then(|s| s.current.clone())
                                    .unwrap_or_default();
                                let cache_pct = if usage.input_tokens > 0 {
                                    (usage.cached_input_tokens as f64 / usage.input_tokens as f64) * 100.0
                                } else {
                                    0.0
                                };
                                println!(
                                    "[Proxy] WS Token: input={} cached={} ({:.0}%) output={} model={} account={}",
                                    usage.input_tokens,
                                    usage.cached_input_tokens,
                                    cache_pct,
                                    usage.output_tokens,
                                    usage.model,
                                    if cur_id.is_empty() { "?" } else { &cur_id }
                                );
                                if usage.cached_input_tokens > 0 {
                                    if let Ok(g) = ws_session_key_r.lock() {
                                        if let Some(sk) = g.as_ref() {
                                            if !cur_id.is_empty() {
                                                state_clone.session_affinity.record_cache_hit(
                                                    sk,
                                                    &cur_id,
                                                    usage.cached_input_tokens,
                                                );
                                            }
                                        }
                                    }
                                }
                                usage.account_id = cur_id;
                                state_clone.tracker.record(usage);
                            }
                        }
                    }

                    if msg.is_close() {
                        let _ = client_write.send(msg).await;
                        break;
                    }

                    // ── shell tool 输出压缩（codex WS 协议）──
                    // 仅对 Text 帧、且 settings.output_compression_enabled=true 时尝试。
                    // 不能压缩的帧（非 JSON / 非 function_call_output / 已小于阈值）原样下发。
                    let msg = maybe_compress_ws_text(&state_clone, msg);

                    if client_write.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
        _ = disconnect.notified() => {
            println!("[Proxy] 账号已切换，断开 WebSocket 连接（Codex App 将自动重连）");
        },
    }
}

// ────────────────────────────────────────────────────────────────
// Shell tool 输出压缩（Phase 1）
// ────────────────────────────────────────────────────────────────
// 命中策略：按 JSON 形状（type=function_call_output / tool_result + output|content
// 是 string 字段）判定，不靠白名单 tool name；这样新加 shell 类工具不需要改代码。

/// 取当前 settings 的压缩开关 + 阈值
fn read_compress_settings(state: &ProxyState) -> (bool, usize) {
    state
        .store
        .lock()
        .ok()
        .map(|s| {
            (
                s.settings.output_compression_enabled,
                s.settings.output_compression_threshold_bytes,
            )
        })
        .unwrap_or((true, 8192))
}

/// 把 codex 的 WS Text 帧里 `function_call_output` 的 `output` 字段做截断。
/// 不影响其它类型的帧；解析失败 / 非 JSON / 字段不存在 → 原样返回。
fn maybe_compress_ws_text(
    state: &ProxyState,
    msg: tungstenite::Message,
) -> tungstenite::Message {
    let (enabled, threshold) = read_compress_settings(state);
    if !enabled {
        return msg;
    }
    let text = match &msg {
        tungstenite::Message::Text(t) => t.as_str(),
        _ => return msg, // Binary / Ping / Pong / Close 透传
    };
    // 廉价快路径：连关键词都没有 → 不解析
    if !text.contains("function_call_output") && !text.contains("\"output\"") {
        return msg;
    }
    let mut val: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return msg,
    };
    let mut hit_tool: Option<String> = None;
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut any_compressed = false;
    rewrite_function_call_output(&mut val, threshold, &mut hit_tool, &mut total_in, &mut total_out, &mut any_compressed);
    if total_in == 0 {
        return msg; // 没找到 output 字段
    }
    crate::record_compress_frame(any_compressed, total_in, total_out);
    if any_compressed {
        let savings = if total_in == 0 { 0.0 } else {
            (1.0 - (total_out as f64 / total_in as f64)) * 100.0
        };
        println!(
            "[Compress] {}→{} bytes (-{:.0}%) tool={}",
            total_in,
            total_out,
            savings,
            hit_tool.as_deref().unwrap_or("?")
        );
    }
    if !any_compressed {
        return msg; // 没真的截断 → 复用原 Message，省一次 reserialize
    }
    match serde_json::to_string(&val) {
        Ok(s) => tungstenite::Message::Text(s.into()),
        Err(_) => msg,
    }
}

/// 递归扫 JSON：发现形如 `{"type":"function_call_output", "output": "..."}` 或
/// `{"type":"function_call_output", "content":"..."}` 的对象，对 output/content 做压缩。
/// 也覆盖 codex 5.5 的 `response.output_item.added` 嵌套形态。
fn rewrite_function_call_output(
    val: &mut serde_json::Value,
    threshold: usize,
    hit_tool: &mut Option<String>,
    total_in: &mut u64,
    total_out: &mut u64,
    any_compressed: &mut bool,
) {
    match val {
        serde_json::Value::Object(map) => {
            let is_fco = map
                .get("type")
                .and_then(|v| v.as_str())
                .map(|t| t == "function_call_output" || t == "tool_result")
                .unwrap_or(false);
            if is_fco {
                // 先把 tool 名快照出来（避开同时 borrow_mut + borrow 同一个 map）
                let tool_name_snapshot = map
                    .get("name")
                    .or_else(|| map.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "shell".to_string());
                // 优先 output，其次 content
                for key in ["output", "content"] {
                    if let Some(field) = map.get_mut(key) {
                        if let Some(s) = field.as_str() {
                            let r = crate::output_compress::compress(s, threshold);
                            *total_in += r.original_bytes as u64;
                            *total_out += r.output_bytes as u64;
                            if r.was_compressed {
                                *any_compressed = true;
                                if hit_tool.is_none() {
                                    *hit_tool = Some(tool_name_snapshot.clone());
                                }
                                let new_text = r.compressed.into_owned();
                                *field = serde_json::Value::String(new_text);
                            }
                        }
                    }
                }
            }
            // 继续递归（function_call_output 可能嵌套在 response.output_item.added 等结构里）
            for (_, v) in map.iter_mut() {
                rewrite_function_call_output(v, threshold, hit_tool, total_in, total_out, any_compressed);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_function_call_output(v, threshold, hit_tool, total_in, total_out, any_compressed);
            }
        }
        _ => {}
    }
}

/// 把 SSE byte stream 切到事件边界（`\n\n`），逐事件尝试压缩 tool 输出。
/// 不命中的事件原样回吐；命中且真截断的事件用新 bytes 替换。
/// flush 时把残留 buffer 一并送出（兼容上游不带尾部 `\n\n` 的实现）。
fn sse_compress_stream(
    inner: ByteStream,
    threshold: usize,
) -> impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send {
    use futures_util::stream;
    enum State {
        Active {
            inner: ByteStream,
            buf: Vec<u8>,
            pending: std::collections::VecDeque<Bytes>,
        },
        Flushing {
            tail: Option<Bytes>,
        },
        Done,
    }

    let init = State::Active {
        inner,
        buf: Vec::with_capacity(16 * 1024),
        pending: std::collections::VecDeque::new(),
    };

    stream::unfold(init, move |state| async move {
        match state {
            State::Active { mut inner, mut buf, mut pending } => {
                // 已有缓存的待发事件先吐
                if let Some(b) = pending.pop_front() {
                    return Some((Ok(b), State::Active { inner, buf, pending }));
                }
                loop {
                    let next = inner.next().await;
                    match next {
                        None => {
                            // 上游 EOF：把残留 buffer 作为最后一块发出
                            let tail = if buf.is_empty() {
                                None
                            } else {
                                Some(Bytes::from(std::mem::take(&mut buf)))
                            };
                            return Some((
                                Ok(Bytes::new()),
                                State::Flushing { tail },
                            ));
                        }
                        Some(Err(e)) => {
                            // 错误时不再尝试压缩；把错误透传，结束
                            return Some((Err(e), State::Done));
                        }
                        Some(Ok(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            // 切到 \n\n 边界：找最后一个 \n\n，前面的全部按事件处理
                            let mut emit = Vec::<u8>::with_capacity(chunk.len());
                            let mut search_from = 0usize;
                            while let Some(rel) = find_double_newline(&buf[search_from..]) {
                                let end = search_from + rel + 2; // 含 \n\n
                                let event_bytes = &buf[search_from..end];
                                let rewritten = maybe_rewrite_sse_event(event_bytes, threshold);
                                emit.extend_from_slice(&rewritten);
                                search_from = end;
                            }
                            // 把已处理掉的部分从 buf 头部清除
                            if search_from > 0 {
                                buf.drain(..search_from);
                            }
                            if !emit.is_empty() {
                                let head = Bytes::from(emit);
                                return Some((Ok(head), State::Active { inner, buf, pending }));
                            }
                            // 此 chunk 还没出现完整事件 → 继续 await 下一块
                            // （不空转：上面 inner.next().await 是真正的 await 点）
                        }
                    }
                }
            }
            State::Flushing { tail } => {
                if let Some(b) = tail {
                    // 残留：尽力按"一个不完整事件"去压缩
                    let rewritten = maybe_rewrite_sse_event(&b, threshold);
                    Some((Ok(Bytes::from(rewritten)), State::Done))
                } else {
                    None
                }
            }
            State::Done => None,
        }
    })
    // 第一个元素是占位 Bytes::new()（EOF 触发的伪 yield），过滤掉
    .filter(|res| {
        let keep = match res {
            Ok(b) => !b.is_empty(),
            Err(_) => true,
        };
        futures_util::future::ready(keep)
    })
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    for i in 0..buf.len() - 1 {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    None
}

/// 给一个完整 SSE 事件（含 `data:` 行 + 终结的 `\n\n`），尝试压缩里面的 tool 输出。
/// 不是 JSON / 不是 tool_result / 字段不命中 → 返回原 bytes（拷贝；调用方已 `&[u8]` 视图）。
fn maybe_rewrite_sse_event(event: &[u8], threshold: usize) -> Vec<u8> {
    // 协议：data:<json>\n\n  或  event:xxx\ndata:<json>\n\n
    // 我们只动 data: 行的 JSON。
    let text = match std::str::from_utf8(event) {
        Ok(s) => s,
        Err(_) => return event.to_vec(),
    };
    // 只对包含相关关键词的事件做 JSON 解析（廉价 prefilter）
    if !(text.contains("function_call_output")
        || text.contains("tool_result")
        || text.contains("\"output\""))
    {
        return event.to_vec();
    }

    let mut out = Vec::with_capacity(event.len());
    let mut any_changed = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some(payload) = trimmed.strip_prefix("data:") {
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            // 空 data 或 [DONE] 透传
            if payload.is_empty() || payload == "[DONE]" {
                out.extend_from_slice(line.as_bytes());
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(payload) {
                Ok(mut val) => {
                    let mut hit_tool: Option<String> = None;
                    let mut total_in: u64 = 0;
                    let mut total_out: u64 = 0;
                    let mut any_compressed = false;
                    rewrite_function_call_output(
                        &mut val,
                        threshold,
                        &mut hit_tool,
                        &mut total_in,
                        &mut total_out,
                        &mut any_compressed,
                    );
                    // 也覆盖 claude code SSE 的 content_block_delta.delta 形态
                    rewrite_claude_tool_result(
                        &mut val,
                        threshold,
                        &mut hit_tool,
                        &mut total_in,
                        &mut total_out,
                        &mut any_compressed,
                    );
                    if total_in > 0 {
                        crate::record_compress_frame(any_compressed, total_in, total_out);
                        if any_compressed {
                            let savings = if total_in == 0 { 0.0 } else {
                                (1.0 - (total_out as f64 / total_in as f64)) * 100.0
                            };
                            println!(
                                "[Compress] {}→{} bytes (-{:.0}%) tool={}",
                                total_in,
                                total_out,
                                savings,
                                hit_tool.as_deref().unwrap_or("?")
                            );
                        }
                    }
                    if any_compressed {
                        any_changed = true;
                        // 重新序列化为单行 data
                        let serialized = serde_json::to_string(&val).unwrap_or_else(|_| payload.to_string());
                        out.extend_from_slice(b"data: ");
                        out.extend_from_slice(serialized.as_bytes());
                        out.push(b'\n');
                    } else {
                        out.extend_from_slice(line.as_bytes());
                    }
                }
                Err(_) => out.extend_from_slice(line.as_bytes()),
            }
        } else {
            out.extend_from_slice(line.as_bytes());
        }
    }

    if any_changed {
        out
    } else {
        event.to_vec()
    }
}

/// 处理 claude code 的 SSE 形态：
/// - 完整 tool_result block：`{"type":"tool_result","content": "..." | [{"type":"text","text":"..."}]}`
/// - content_block_delta：`{"type":"content_block_delta", "delta":{"type":"tool_result_delta", "text":"..."}}`
fn rewrite_claude_tool_result(
    val: &mut serde_json::Value,
    threshold: usize,
    hit_tool: &mut Option<String>,
    total_in: &mut u64,
    total_out: &mut u64,
    any_compressed: &mut bool,
) {
    match val {
        serde_json::Value::Object(map) => {
            let block_type = map.get("type").and_then(|v| v.as_str()).map(String::from);
            if matches!(block_type.as_deref(), Some("tool_result")) {
                // content 可能是 string 或 array of blocks
                if let Some(field) = map.get_mut("content") {
                    match field {
                        serde_json::Value::String(s) => {
                            let r = crate::output_compress::compress(s, threshold);
                            *total_in += r.original_bytes as u64;
                            *total_out += r.output_bytes as u64;
                            if r.was_compressed {
                                *any_compressed = true;
                                if hit_tool.is_none() {
                                    *hit_tool = Some("Bash".to_string());
                                }
                                *field = serde_json::Value::String(r.compressed.into_owned());
                            }
                        }
                        serde_json::Value::Array(arr) => {
                            for blk in arr.iter_mut() {
                                if let Some(blk_obj) = blk.as_object_mut() {
                                    if blk_obj.get("type").and_then(|v| v.as_str()) == Some("text") {
                                        if let Some(text_field) = blk_obj.get_mut("text") {
                                            if let Some(s) = text_field.as_str() {
                                                let r = crate::output_compress::compress(s, threshold);
                                                *total_in += r.original_bytes as u64;
                                                *total_out += r.output_bytes as u64;
                                                if r.was_compressed {
                                                    *any_compressed = true;
                                                    if hit_tool.is_none() {
                                                        *hit_tool = Some("Bash".to_string());
                                                    }
                                                    *text_field = serde_json::Value::String(r.compressed.into_owned());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            // 递归找嵌套的 tool_result
            for (_, v) in map.iter_mut() {
                rewrite_claude_tool_result(v, threshold, hit_tool, total_in, total_out, any_compressed);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_claude_tool_result(v, threshold, hit_tool, total_in, total_out, any_compressed);
            }
        }
        _ => {}
    }
}

fn error_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "proxy_error",
            "code": status.as_u16(),
        }
    });

    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(full_body(Bytes::from("internal error")))
                .unwrap()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_chatgpt_strips_v1_prefix() {
        let (url, host) = get_upstream(true, None, "/v1/responses");
        assert_eq!(url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(host, "chatgpt.com");
    }

    #[test]
    fn upstream_openai_keeps_v1_path() {
        let (url, host) = get_upstream(false, None, "/v1/chat/completions");
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(host, "api.openai.com");
    }

    #[test]
    fn upstream_relay_uses_base_url_with_full_path() {
        let (url, host) =
            get_upstream(false, Some("https://unity2.ai"), "/v1/chat/completions");
        assert_eq!(url, "https://unity2.ai/v1/chat/completions");
        assert_eq!(host, "unity2.ai");
    }

    #[test]
    fn upstream_relay_strips_trailing_slash_on_base_url() {
        let (url, host) =
            get_upstream(false, Some("https://unity2.ai/"), "/v1/responses");
        assert_eq!(url, "https://unity2.ai/v1/responses");
        assert_eq!(host, "unity2.ai");
    }

    #[test]
    fn upstream_relay_with_port_extracts_host_only() {
        let (url, host) = get_upstream(
            false,
            Some("http://127.0.0.1:9080"),
            "/v1/chat/completions",
        );
        assert_eq!(url, "http://127.0.0.1:9080/v1/chat/completions");
        assert_eq!(host, "127.0.0.1");
    }

    #[test]
    fn upstream_chatgpt_takes_precedence_over_relay_url() {
        // 防呆：is_chatgpt=true 时 relay_base_url 应该被忽略（理论上不会同时设置，但要稳）
        let (url, host) =
            get_upstream(true, Some("https://unity2.ai"), "/v1/responses");
        assert_eq!(url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(host, "chatgpt.com");
    }
}
