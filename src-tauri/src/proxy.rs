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
    if let Ok(disk_auth) = AccountStore::read_codex_auth() {
        if store.sync_account_from_auth_json(&current_id, disk_auth) {
            let _ = store.save();
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
    let bound_account = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let cur = store.current.clone();
        let bid = state.session_affinity.lookup(sk, |id| {
            store
                .accounts
                .get(id)
                .map(|a| !a.is_banned && !a.is_logged_out && !a.is_token_invalid)
                .unwrap_or(false)
        });
        match bid {
            Some(id) if Some(&id) != cur.as_ref() => Some(id),
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

/// 根据认证模式获取上游地址
fn get_upstream(is_chatgpt: bool, path_and_query: &str) -> (String, &'static str) {
    if is_chatgpt {
        // 客户端路径: /v1/responses (因为 OPENAI_BASE_URL 带 /v1)
        // ChatGPT 上游: /backend-api/codex/responses (不含 /v1)
        // 需要去掉 /v1 前缀
        let path = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
        let url = format!("{}{}", CHATGPT_ORIGIN, path);
        (url, CHATGPT_HOST)
    } else {
        // API key: 转发到 api.openai.com + 原始路径（保留 /v1）
        let url = format!("{}{}", API_ORIGIN, path_and_query);
        (url, API_HOST)
    }
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

    // 代理内部切号：代理肯定在跑，直接按 switch_mode 决定。cold 强制写 auth.json。
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
                    if let Err(e) =
                        crate::remote_client::push_solo_switch(&base, &secret, &nid).await
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

    // 提取 session_key：用于 affinity 路由 + cache hit 记账（client 模式 affinity 由 Server 处理）
    let session_key = crate::session_affinity::extract_session_key(&body_bytes, &req_headers);

    // ── client 模式：先转发到 Server；Server 不可达或返回 402 deactivated 时回退本地 ──
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

    // 2. 根据认证模式路由上游
    let (upstream_url, upstream_host) = get_upstream(is_chatgpt, &path_and_query);

    // 3. 透明 Header 转发（官方 responses-api-proxy 逻辑）
    let base_headers = build_upstream_headers(&req_headers, upstream_host);

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

    // 7. 429 自动切号
    if status_code == reqwest::StatusCode::TOO_MANY_REQUESTS {
        println!("[Proxy] 收到 429，标记额度耗尽并切号...");
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
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        return Ok(Response::builder()
            .status(429)
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap_or_else(|_| error_response(StatusCode::TOO_MANY_REQUESTS, "429")));
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
        match forward_and_bootstrap(state, method, upstream_url, base_headers, body, &new_token, session_key)
            .await
        {
            BootstrappedForward::Ok(resp) => {
                state.stats.auto_switches.fetch_add(1, Ordering::Relaxed);
                let _ = state
                    .app_handle
                    .emit("proxy-account-switched", &outcome.name.unwrap_or_default());
                return Some(resp);
            }
            BootstrappedForward::RateLimit => {
                println!(
                    "[Proxy] 第 {} 次 Server 切号后仍限额（status/流内），再试",
                    attempt + 1
                );
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
    for attempt in 0..MAX_429_RETRIES {
        match pick_next_account(state) {
            PickResult::Found { id, token } => {
                if let Err(e) = do_switch(state, &id, reason.clone()) {
                    eprintln!("[Proxy] 切号失败: {}", e);
                    continue;
                }

                match forward_and_bootstrap(
                    state,
                    method,
                    upstream_url,
                    base_headers,
                    body,
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
                    BootstrappedForward::Banned => {
                        println!("[Proxy] 第 {} 次切号目标号疑似封号", attempt + 1);
                        mark_current_banned(state);
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

    // 根据 token 形态路由上游
    let is_chatgpt = token.starts_with("eyJ");
    let (upstream_url, upstream_host) = get_upstream(is_chatgpt, path_and_query);
    let base_headers = build_upstream_headers(req_headers, upstream_host);

    match forward_and_bootstrap(state, method, &upstream_url, &base_headers, body, &token, session_key).await {
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
        BootstrappedForward::Banned => {
            println!("[Proxy] 本地回退账号疑似封号");
            mark_current_banned(state);
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
    /// 流前缀里检测到限额事件 → 切号重发
    RateLimitInStream,
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

/// 检测累计 buffer 是否在 response.failed / error 事件里出现限额关键词
fn sse_buf_has_rate_limit(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf).to_lowercase();
    let in_failure = s.contains("event: response.failed")
        || s.contains("event: error")
        || s.contains("\"type\":\"response.failed\"")
        || s.contains("\"type\":\"error\"");
    if !in_failure {
        return false;
    }
    RATE_LIMIT_KEYWORDS.iter().any(|kw| s.contains(kw))
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

        // 顺序很重要：先嗅 rate_limit/banned，再判内容事件。
        // 防止极端情况下错误事件和内容事件落在同一 chunk 里被误判为安全。
        if sse_buf_has_rate_limit(&buf) {
            return SseBootstrap::RateLimitInStream;
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
    /// status 429 或流内 rate_limit 事件 → 调用方应该再切号重试
    RateLimit,
    /// 流内封号事件 → 调用方应该标记封号并切号重试
    Banned,
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
                if sse_buf_has_rate_limit(&buf) {
                    return SseBootstrap::RateLimitInStream;
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
        let resp = forward_with_token(state, method, upstream_url, base_headers, body, &new_token).await.ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        return Some(resp.bytes_stream().boxed());
    }

    // 本地模式
    let pick = pick_next_account(state);
    let PickResult::Found { id, token } = pick else { return None; };
    do_switch(state, &id, reason).ok()?;
    let resp = forward_with_token(state, method, upstream_url, base_headers, body, &token).await.ok()?;
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
    let heartbeat = std::time::Duration::from_millis(1500);
    let mut current_upstream = initial_upstream;
    let mut attempts: usize = 0;

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
                // forward rest（client 那头本来就在听 SSE 流；上游静默时继续发 keep-alive）
                loop {
                    match tokio::time::timeout(heartbeat, rest.next()).await {
                        Ok(Some(item)) => {
                            if tx.send(item).await.is_err() { return; }
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

    // 把 prefix 当作流的第一个 chunk 先吐出去（空 prefix 时跳过）
    let prefix_stream: ByteStream = if prefix.is_empty() {
        futures_util::stream::empty().boxed()
    } else {
        futures_util::stream::once(async move { Ok(prefix) }).boxed()
    };
    // 给 rest 套一层 SSE keep-alive：上游每 1.5s 没新 chunk 就自动塞一行 SSE 注释
    // (": keep-alive\n\n")，client 解析器忽略，但 TCP 连接和 codex 那头的读循环都不会超时
    let rest_with_heartbeat = wrap_with_sse_heartbeat(
        rest,
        std::time::Duration::from_millis(1500),
    );
    let raw_stream = prefix_stream.chain(rest_with_heartbeat);

    let stream = raw_stream.map(move |result| match result {
        Ok(bytes) => {
            if let Ok(mut buf) = buf_clone.lock() {
                buf.extend_from_slice(&bytes);
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
    let (http_url, _upstream_host) = get_upstream(is_chatgpt, &path);

    // http(s):// → ws(s)://
    let ws_url = http_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);

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

            if !is_auth_err {
                eprintln!("[Proxy] WebSocket 上游连接失败: {}", e);
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("WebSocket 上游连接失败: {}", e),
                ));
            }

            println!("[Proxy] WebSocket 认证失败 ({}), 尝试切号重连...", e);
            mark_current_quota_depleted(&state);

            // 切号重连，最多试 3 个号
            let mut retry_conn = None;
            for _attempt in 0..3 {
                if let PickResult::Found { id, token: new_tok } = pick_next_account(&state) {
                    if do_switch(&state, &id, SwitchReason::WebSocketPrecheck).is_err() {
                        continue;
                    }
                    let new_chatgpt = new_tok.starts_with("eyJ");
                    let (new_url, _) = get_upstream(new_chatgpt, &path);
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
                            println!("[Proxy] WebSocket 切号重连成功");
                            retry_conn = Some(c);
                            break;
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

/// 检测 WebSocket 消息是否为限额错误
/// 只匹配 response.failed 类型的错误消息，避免误判正常消息中的 rate_limit 字段
/// 限额关键词（快速文本匹配用）
const RATE_LIMIT_KEYWORDS: &[&str] = &[
    "rate_limit",
    "rate limit",
    "usage_limit",
    "usage limit",
    "too many requests",
    "insufficient_quota",
    "billing_hard_limit",
    "tokens per min",
    "requests per min",
];

fn detect_ws_rate_limit(msg: &tungstenite::Message) -> bool {
    if let tungstenite::Message::Text(ref text) = msg {
        let lower = text.to_lowercase();

        // 快速文本匹配
        let matched = RATE_LIMIT_KEYWORDS.iter().any(|kw| lower.contains(kw));
        if !matched {
            return false;
        }

        println!("[Proxy] WS 消息包含限额关键词");

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
        }

        // JSON 解析失败或没有 error 字段，但文本明确包含限额消息
        if lower.contains("hit your usage limit")
            || lower.contains("rate limit reached")
            || lower.contains("too many requests")
        {
            println!("[Proxy] WS 限额: 文本兜底匹配");
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
                    // 检测限额错误：**不要把错误消息转发给 client**（之前的 bug），
                    // 直接丢弃 + 切号 + 关 WS，让 Codex App 看到干净的连接断开然后重连，
                    // 而不是看到"Upgrade to Plus"那种刺眼的错误提示。
                    if detect_ws_rate_limit(&msg) {
                        println!(
                            "[Proxy] WebSocket 检测到限额，静默切号 + 关 WS（不透回 codex）..."
                        );
                        mark_current_quota_depleted(&state_clone);
                        if let PickResult::Found { id, .. } = pick_next_account(&state_clone) {
                            let _ = do_switch(&state_clone, &id, SwitchReason::WebSocketRateLimit);
                        }
                        // 主动关 client 侧 WS，让 Codex App 知道这个连接结束了 → 自动重连
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
