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
use crate::session_routes::SessionRoutesStore;
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
                        // 走 anchor-guarded 路径，anchor 设置时跳过非 anchor 账号的写盘
                        write_codex_auth_respecting_anchor(state, &current_id, &t.auth_json);
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

    // 2) 本地 oauth refresh（**仅** off/server 模式；client/solo 走不到这里）
    //
    // 协议：client/solo 把 rt 权威完全交给 Server。哪怕 Server 抖了一下也不能本机偷偷
    // rotate —— 一旦 rotate，Server 那边的 rt 会立刻被 OpenAI 标 reused 而死号。
    // 让 codex 收个 401 走自己的重试链路，比赌"这一次本机刷成功就好"安全得多。
    if matches!(remote_mode.as_str(), "client" | "solo") {
        return SilentRefreshOutcome::OtherError(
            "client/solo 模式禁止本机 rt 刷新；Server 不可达，跳过本机降级".into(),
        );
    }

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

    match crate::oauth::refresh_access_token_locked(&current_id, &rt).await {
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
                // anchor 设置时跳过非 anchor 账号的写盘
                write_codex_auth_respecting_anchor(state, &current_id, &auth);
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
    /// 用户级硬路由：session_id → account 强绑定
    session_routes: Arc<Mutex<SessionRoutesStore>>,
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
    session_routes: Arc<Mutex<SessionRoutesStore>>,
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
            session_routes,
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
                        // anchor 设置时跳过非 anchor 账号的写盘（保护手机 bridge 的 disk 镜像）
                        write_codex_auth_respecting_anchor(state, &current_id, &t.auth_json);
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
///
/// 返回 (token, is_chatgpt, account_id_used, hard_routed)
/// - account_id 给 end_signal 记账用
/// - hard_routed=true 表示命中用户主动定义的"硬路由"（session_routes），
///   上层在 401/429 时应跳过 try_switch_and_retry / silent_refresh_current，
///   把上游错误原样透回（严格模式）。
async fn resolve_token_with_affinity(
    state: &ProxyState,
    session_key: Option<&str>,
) -> Result<(String, bool, Option<String>, bool), String> {
    let Some(sk) = session_key else {
        let (tok, is_cgpt) = get_current_token(state).await?;
        let cur = state.store.lock().ok().and_then(|s| s.current.clone());
        return Ok((tok, is_cgpt, cur, false));
    };

    // 0) 用户级硬路由优先：session_id 命中 enabled route → 强制用绑定账号 token，
    //    即使账号被标 banned/logged_out 也照打（让上游回 401/403），不触发自动切号。
    let hard_route_hit: Option<(String, String, String)> = {
        let mut routes = state
            .session_routes
            .lock()
            .map_err(|e| format!("session_routes lock: {}", e))?;
        if let Some(route) = routes.find_enabled_by_session(sk) {
            let route_id = route.id.clone();
            let account_id = route.account_id.clone();
            // 取出 token + account name（用同一把 store lock，避免半路被切号）
            let pair = {
                let store = state.store.lock().map_err(|e| e.to_string())?;
                match store.accounts.get(&account_id) {
                    Some(acc) => match AccountStore::extract_access_token(&acc.auth_json) {
                        Some(tok) => Some((tok, acc.name.clone())),
                        None => {
                            eprintln!(
                                "[Proxy] Hard route 命中 {} → {}，但账号缺 access_token；按策略仍透传请求",
                                sk, account_id
                            );
                            None
                        }
                    },
                    None => {
                        eprintln!(
                            "[Proxy] Hard route 命中 {} → {}，但账号不存在；落回普通选号",
                            sk, account_id
                        );
                        None
                    }
                }
            };
            if let Some((token, name)) = pair {
                // 即使路由的账号已被标 banned/logged_out 也继续——严格模式由用户负责
                routes.record_hit(&route_id);
                let _ = routes.save();
                drop(routes);
                let is_chatgpt = token.starts_with("eyJ");
                println!("[Proxy] Hard route hit: {} → {}", sk, name);
                Some((token, account_id, name))
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some((token, account_id, _name)) = hard_route_hit {
        let is_chatgpt = token.starts_with("eyJ");
        return Ok((token, is_chatgpt, Some(account_id), true));
    }

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
                .map(|a| {
                    // 基础健康：没被打三个 flag
                    let basic_ok = !a.is_banned && !a.is_logged_out && !a.is_token_invalid;
                    if !basic_ok {
                        return false;
                    }
                    // Relay 没有 5h/周配额概念，跳过额度检查
                    if a.is_relay() {
                        return true;
                    }
                    // 5h / 周配额耗尽时也认为不健康 —— 否则 ChatGPT 上游会做
                    // 静默 mid-stream RST（不是干净 429），codex 看到 transport error
                    // 反复重连 5/5 仍失败。把这种"软失效"也从 affinity 候选剔除。
                    match a.cached_quota.as_ref() {
                        Some(q) => q.five_hour_left > 0.0 && q.weekly_left > 0.0,
                        None => true, // 没缓存就给个 benefit of doubt
                    }
                })
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
        return Ok((token, is_chatgpt, Some(account_id), false));
    }

    let (tok, is_cgpt) = get_current_token(state).await?;
    let cur = state.store.lock().ok().and_then(|s| s.current.clone());
    Ok((tok, is_cgpt, cur, false))
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
    /// `"responses"`（默认 / 上游原生）或 `"chat_completions"`（GLM 走翻译）
    protocol: String,
    /// Relay 配的 API key（chat_completions 模式下用作上游 Authorization）
    api_key: Option<String>,
    /// Relay base_url，便于在 chat_completions 模式下重写为 `<base>/chat/completions`
    base_url: Option<String>,
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
        protocol: acc.relay_protocol_or_default().to_string(),
        api_key: AccountStore::extract_access_token(&acc.auth_json),
        base_url: acc.relay_base_url.clone(),
    })
}

/// 按 account_id 直接构造 RelayRoute（hard route 用，跳过 store.current 检查）。
fn relay_route_for_account(state: &ProxyState, account_id: &str) -> Option<RelayRoute> {
    let store = state.store.lock().ok()?;
    let acc = store.accounts.get(account_id)?;
    if !acc.is_relay() {
        return None;
    }
    Some(RelayRoute {
        account_id: account_id.to_string(),
        model_map: acc.relay_model_map.clone(),
        model_fallback: acc.relay_model_fallback.clone(),
        protocol: acc.relay_protocol_or_default().to_string(),
        api_key: AccountStore::extract_access_token(&acc.auth_json),
        base_url: acc.relay_base_url.clone(),
    })
}

/// 从请求 headers + body 提取 session_key，并查 enabled hard route。
/// 命中即记录 hit 并返回 (session_key, account_id)。
///
/// hyper::HeaderMap 跟 reqwest::header::HeaderMap 都是 http::HeaderMap 的别名，
/// 同类型直接传，不需要拷贝转换（之前转换可能因 HeaderName::from_bytes 验证规则
/// 把 codex 的 `session_id`（带 underscore）丢掉，导致路由命中失败）。
fn resolve_hard_route(
    state: &ProxyState,
    body: &[u8],
    headers: &hyper::HeaderMap,
) -> Option<(String, String)> {
    // 显式 fallback：先按 codex 实际发的 session_id / thread_id header 直接抠，
    // 跟 session_affinity 模块兼容（`hdr:` 前缀）。
    let sk: Option<String> = (|| -> Option<String> {
        for name in &["session_id", "session-id", "x-session-id", "thread_id"] {
            if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
                if !v.is_empty() {
                    return Some(format!("hdr:{v}"));
                }
            }
        }
        // fallback：还看 body（JSON prompt_cache_key / previous_response_id），
        // 走 session_affinity 已有逻辑
        crate::session_affinity::extract_session_key(body, headers)
    })();
    let sk = sk?;
    // 路由表里存的是 bare session UUID（不带 hdr: / pck: 前缀），lookup 要剥掉前缀
    let raw_session = sk
        .strip_prefix("hdr:")
        .or_else(|| sk.strip_prefix("pck:"))
        .or_else(|| sk.strip_prefix("prev:"))
        .unwrap_or(&sk)
        .to_string();
    let account_id = {
        let mut routes = state.session_routes.lock().ok()?;
        match routes.find_enabled_by_session(&raw_session) {
            Some(r) => {
                let aid = r.account_id.clone();
                let rid = r.id.clone();
                routes.record_hit(&rid);
                let _ = routes.save();
                aid
            }
            None => {
                let known: Vec<String> = routes
                    .routes
                    .values()
                    .filter(|r| r.enabled)
                    .map(|r| r.session_id.clone())
                    .collect();
                println!(
                    "[Proxy] Hard route MISS: incoming session_key={:?} raw={:?} known_routes={:?}",
                    sk, raw_session, known
                );
                return None;
            }
        }
    };
    let acc_name = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.accounts.get(&account_id).map(|a| a.name.clone()))
        .unwrap_or_else(|| account_id.clone());
    println!(
        "[Proxy] Hard route hit: session={} → {} ({})",
        raw_session, account_id, acc_name
    );
    Some((sk, account_id))
}

/// 若 body 是 JSON 且含 `model` 字段，按 `map` / `fallback` 重写。
///
/// 优先级：map 命中 > fallback；都不命中或值跟原值相等 → 原样返回。
///
/// **幂等保证**：函数可能在请求生命周期里被调用多次（顶层 raw body 一次 + 解压
/// 后翻译器入口再一次）。如果当前 model 已经是 map 的某个 value（或正好等于
/// fallback），就跳过——避免出现"o1→deepseek-reasoner 之后再被 fallback 拉回
/// deepseek-chat"这种 double-rewrite 把 reasoner 模型偷偷换成 chat 的 bug。
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

    // 幂等：如果 original 已经是 map 的某个 target value，或就是 fallback，
    // 说明此前已经 rewrite 过；直接返回，不再二次替换。
    let already_target = map
        .map(|m| m.values().any(|v| v == &original))
        .unwrap_or(false)
        || fallback.map_or(false, |f| f == original);
    if already_target {
        return body.clone();
    }

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

    // 阈值命中前先看：当前号是否还有活跃 session affinity binding。
    // 有的话主动抢切会立刻让进行中的会话掉 prompt cache —— 这种"预防 429"的代价
    // 是 cache cold-start（write 1.25× + 下一轮 input 全价）。让请求自己撞 429
    // 再切才划算（撞 429 时 affinity 也会被 invalidate，行为一致），并且能榨出
    // 当前号最后那一点额度。
    let has_active_session = state.session_affinity.has_active_binding_to(current_id);

    if is_free && fg > 0.0 && quota.five_hour_left < fg {
        if has_active_session {
            return false;
        }
        println!(
            "[Proxy] Free 保护线触发: {:.0}% < {:.0}%",
            quota.five_hour_left, fg
        );
        return true;
    }
    if t5h > 0.0 && quota.five_hour_left < t5h {
        if has_active_session {
            return false;
        }
        println!(
            "[Proxy] 5h 阈值触发: {:.0}% < {:.0}%",
            quota.five_hour_left, t5h
        );
        return true;
    }
    if tw > 0.0 && quota.weekly_left < tw {
        if has_active_session {
            return false;
        }
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

/// Spark 模型 id（chat_inbound 已把所有 spark 别名归一到这个）。
const SPARK_MODEL_ID: &[u8] = b"gpt-5.3-codex-spark";

/// 请求是否针对 Spark 模型。Spark 是 **Pro 专属的独立子限额**（accounts.json
/// cached_quota.spark），跟基础 5h/周额度是两个池子；gpt-5.5 等走的是基础额度。
/// 所以 Spark 请求 429 只代表 Spark 耗尽，**绝不能据此判定整个账号没额度而切号**——
/// 否则会把同账号上正常的 gpt-5.5 会话一起踢到没额度的号上（用户报的"任务停"根因）。
/// 直接扫 body 里的精确模型 id（chat_inbound 已归一），不解析大 body。
fn request_is_spark_model(body: &[u8]) -> bool {
    body.len() >= SPARK_MODEL_ID.len()
        && body
            .windows(SPARK_MODEL_ID.len())
            .any(|w| w == SPARK_MODEL_ID)
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

/// 后台拉一次 /wham/usage 把 used_percent 写进 quota-snapshots.jsonl。
/// info 为 None（Relay / 没拿到 access_token / 等）则跳过。
fn spawn_quota_snapshot(
    info: Option<(String, String, Option<String>, Option<String>, String)>,
    trigger: &'static str,
) {
    let (store_id, access_token, chatgpt_account_id, refresh_token, email) = match info {
        Some(x) => x,
        None => return,
    };
    tauri::async_runtime::spawn(async move {
        match crate::usage::UsageFetcher::fetch_usage_direct(
            access_token,
            chatgpt_account_id,
            refresh_token,
            false,
        )
        .await
        {
            Ok((usage, _)) => {
                let snap = crate::quota_snapshot::QuotaSnapshot {
                    ts: chrono::Utc::now(),
                    account_id: store_id,
                    email,
                    plan_type: usage.plan_type,
                    five_hour_used_pct: usage.five_hour_used,
                    weekly_used_pct: usage.weekly_used,
                    five_hour_reset_at: usage.five_hour_reset_at,
                    weekly_reset_at: usage.weekly_reset_at,
                    trigger: trigger.to_string(),
                };
                crate::quota_snapshot::append(&snap);
            }
            Err(e) => {
                println!(
                    "[QuotaSnap] {} 抓取失败（trigger={}）: {}",
                    store_id, trigger, e
                );
            }
        }
    });
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

    // 抓 quota 快照需要的字段（必须在 switch_to 改 store.current 之前取，
    // 否则就拿不到 from 号了）。Relay 号跳过，因为 /wham/usage 不接受其 token。
    let from_fetch_info = store.current.as_ref().and_then(|id| {
        let acc = store.accounts.get(id)?;
        if acc.is_relay() {
            return None;
        }
        let at = crate::account::AccountStore::extract_access_token(&acc.auth_json)?;
        let aid = crate::account::AccountStore::extract_account_id(&acc.auth_json);
        let rt = acc.refresh_token.clone();
        let email = crate::account::AccountStore::extract_email(&acc.auth_json)
            .unwrap_or_else(|| acc.name.clone());
        Some((id.clone(), at, aid, rt, email))
    });
    let to_fetch_info = {
        let acc = store.accounts.get(new_id);
        match acc {
            Some(a) if !a.is_relay() => {
                let at = crate::account::AccountStore::extract_access_token(&a.auth_json);
                let aid = crate::account::AccountStore::extract_account_id(&a.auth_json);
                let rt = a.refresh_token.clone();
                let email = crate::account::AccountStore::extract_email(&a.auth_json)
                    .unwrap_or_else(|| a.name.clone());
                at.map(|at| (new_id.to_string(), at, aid, rt, email))
            }
            _ => None,
        }
    };

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
    // 注意：do_switch 这里**不再**广播 ws_disconnect。
    // 理由：notify_waiters 会把所有并发 WS bridge 一口气全炸断（不区分账号），
    // 但实际"该断的"那条 bridge 自己在 detect_ws_rate_limit/banned 里就 send 了
    // Close 帧；其他 bridge 用的是别的号、跟本次切号无关，被殃及只会让 codex
    // 看到 "websocket closed by server before response.completed" + Reconnecting。
    // 手动切号（lib.rs::switch_account）依然 notify，那是用户期望立即生效。
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

    // Quota 快照：切走前的 from + 切到后的 to，两个时间点的 used_percent 让
    // get_plan_capacity_estimates 能反推 Plan 配额（详见 quota_snapshot.rs）。
    spawn_quota_snapshot(from_fetch_info, "switch_out");
    spawn_quota_snapshot(to_fetch_info, "switch_in");

    if let Some((primary, fallback, secret, nid)) = solo_push {
        tauri::async_runtime::spawn(async move {
            match crate::remote_client::resolve_base_url(&primary, &fallback).await {
                Ok(base) => {
                    // solo 模式：apply_to_disk=false，Server 仅归档 current 不写盘
                    if let Err(e) =
                        crate::remote_client::push_solo_switch(&base, &secret, &nid, false).await
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

/// OpenAI `chat/completions` 入站处理：翻成 codex `responses`，用一个 Spark-capable(Pro)
/// 账号打 ChatGPT 上游，缓冲整条 SSE 后组装回单条 chat/completions JSON。
/// 供 glance 等 OpenAI 兼容客户端复用 ChatGPT 订阅里闲置的 Spark 额度。
async fn handle_chat_inbound(state: Arc<ProxyState>, body: &Bytes) -> Response<ProxyBody> {
    let chat: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("invalid JSON: {}", e)),
    };

    // 选供号：第一个非 relay 且 plan_type==pro 的账号（Spark 专属）。
    let (token, account_id, name) = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        let mut picked: Option<(String, Option<String>, String)> = None;
        for (_id, acc) in store.accounts.iter() {
            if acc.is_relay() {
                continue;
            }
            let plan = acc
                .cached_quota
                .as_ref()
                .map(|q| q.plan_type.to_lowercase())
                .unwrap_or_default();
            if plan == "pro" {
                if let Some(tok) = AccountStore::extract_access_token(&acc.auth_json) {
                    picked = Some((
                        tok,
                        AccountStore::extract_account_id(&acc.auth_json),
                        acc.name.clone(),
                    ));
                    break;
                }
            }
        }
        match picked {
            Some(p) => p,
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "没有可用的 Pro 账号（chat 入站/Spark 需要一个 plan=pro 的账号；先刷新一次该号配额）",
                )
            }
        }
    };

    let want_stream = chat.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let default_model = "gpt-5.3-codex-spark";
    let responses_body = crate::chat_inbound::chat_to_responses(&chat, default_model);
    let model = responses_body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(default_model)
        .to_string();
    println!("[ChatInbound] 账号={} model={}", name, model);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    // spark 偶尔"只 reasoning 不出 message"（空响应）—— 最多重试一次。
    // 每次新 session_id，避免命中相同的空结果缓存。
    let mut chat_resp = serde_json::Value::Null;
    for attempt in 0..2 {
        let sid = uuid::Uuid::new_v4().to_string();
        let mut up = client
            .post(format!("{}/responses", CHATGPT_ORIGIN))
            .header("Authorization", format!("Bearer {}", token))
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", crate::codex_ua::CODEX_ORIGINATOR)
            .header("User-Agent", crate::codex_ua::codex_user_agent())
            .header("session_id", &sid)
            .header("Accept", "text/event-stream")
            .header("Content-Type", "application/json")
            .json(&responses_body);
        if let Some(aid) = account_id.as_deref() {
            if !aid.is_empty() {
                up = up.header("ChatGPT-Account-Id", aid);
            }
        }
        let resp = match up.send().await {
            Ok(r) => r,
            Err(e) => return error_response(StatusCode::BAD_GATEWAY, &format!("上游请求失败: {}", e)),
        };
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = crate::chat_inbound::extract_upstream_error(&raw)
                .unwrap_or_else(|| format!("上游 HTTP {}", status.as_u16()));
            return error_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                &msg,
            );
        }
        let parsed = crate::chat_inbound::responses_sse_to_chat(&raw, &model);
        // 有内容或有 tool_calls 就用它；否则（空响应）再试一次
        let msg = &parsed["choices"][0]["message"];
        let has_content = msg.get("content").map(|c| !c.is_null()).unwrap_or(false);
        let has_tools = msg.get("tool_calls").map(|t| t.is_array()).unwrap_or(false);
        chat_resp = parsed;
        if has_content || has_tools || attempt == 1 {
            break;
        }
        println!("[ChatInbound] 空响应，重试一次（attempt {}）", attempt + 1);
    }

    if want_stream {
        // stream=true：回 Chat Completions SSE，否则只认 SSE delta 的客户端（hermes）
        // 拿到普通 JSON 解析不出内容 → content=None → 空响应误判。
        let sse = crate::chat_inbound::chat_completion_to_sse(&chat_resp);
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(full_body(Bytes::from(sse)))
            .unwrap()
    } else {
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(full_body(Bytes::from(chat_resp.to_string())))
            .unwrap()
    }
}

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
        // DEBUG: dump 所有 upgrade headers，找 session_id 藏在哪
        {
            println!("[Proxy DEBUG] WS upgrade headers:");
            for (name, value) in req.headers() {
                let v = value.to_str().unwrap_or("(non-ascii)");
                let lower = name.as_str().to_lowercase();
                // 只 print session 相关的，避免日志爆
                if lower.contains("session")
                    || lower.contains("codex")
                    || lower.contains("turn")
                    || lower.contains("originator")
                    || lower.contains("thread")
                    || lower.contains("conversation")
                    || lower.contains("trace")
                    || lower.contains("agent")
                {
                    println!("[Proxy DEBUG]   {}: {}", name, v);
                }
            }
        }
        // Hard route 优先：WS upgrade body 是空的，只查 headers（codex 用
        // session_id / x-session-id / Session_id header 透 session_key 出来）。
        if let Some((_sk, account_id)) = resolve_hard_route(&state, &[], req.headers()) {
            if let Some(hard_relay) = relay_route_for_account(&state, &account_id) {
                if hard_relay.protocol == "chat_completions" {
                    println!(
                        "[Proxy] Hard route WS → chat_completions Relay 适配器（{}）",
                        hard_relay.account_id
                    );
                    return handle_chat_completions_relay_websocket(state, hard_relay, req).await;
                }
                // 绑定的 Relay 是 responses 协议 —— 当成普通 WS 直接进 handle_websocket，
                // 但需要把 store.current 临时改成绑定账号。WS 路径目前不支持 per-request
                // 账号覆盖，暂不实现该分支（用户应该选 chat_completions 协议的 Relay 才 ok）。
                println!(
                    "[Proxy] Hard route 绑定的 Relay 是 responses 协议，WS 路径暂不支持 per-request 覆盖，落回 current"
                );
            }
            // 绑定的不是 Relay（订阅号），WS 路径要做 per-request 账号覆盖很复杂。
            // 当前限制：hard route 在 WS 路径只对 chat_completions Relay 生效。
        }

        if let Some(relay) = current_relay_route(&state) {
            if relay.protocol == "chat_completions" {
                println!(
                    "[Proxy] WebSocket upgrade 转入 chat_completions Relay 适配器：{}",
                    req.uri()
                );
                return handle_chat_completions_relay_websocket(state, relay, req).await;
            }
        }
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

    // ── OpenAI chat/completions 入站（glance 等 OpenAI 兼容客户端用 ChatGPT 账号的 codex 模型）──
    // 与下面 responses 路径完全独立：chat → 翻成 codex responses → 打 ChatGPT 上游 → 缓冲 SSE
    // → 组装回单条 chat/completions JSON。详见 chat_inbound 模块。
    {
        let p = path_and_query.split('?').next().unwrap_or("");
        if method == Method::POST && (p == "/v1/chat/completions" || p == "/chat/completions") {
            return Ok(handle_chat_inbound(state, &body_bytes).await);
        }
    }

    // ── Relay 路由前置处理 ──
    // 1) 当 current 是 Relay 时，重写 body 里的 `model` 字段（codex 端发的 gpt-* → glm-*）
    // 2) Relay 不能走 client→Server 转发：Server 不知道这账号，会一路 fall through 浪费时间
    let relay_route = current_relay_route(&state);
    let body_bytes = if let Some(ref r) = relay_route {
        rewrite_model_in_body(
            &body_bytes,
            r.model_map.as_ref(),
            r.model_fallback.as_deref(),
        )
    } else {
        body_bytes
    };

    // ── Hard route 检查（HTTP 路径）──
    // 如果该 session 有 enabled 路由 → 用绑定账号的 RelayRoute 覆盖 current。
    // 注意：必须在 chat_completions 翻译分支之前，否则 current 是普通号会跳过翻译。
    let hard_route_relay: Option<RelayRoute> =
        resolve_hard_route(&state, &body_bytes, &req_headers)
            .and_then(|(_sk, aid)| relay_route_for_account(&state, &aid));

    // ── chat_completions Relay 翻译分支 ──
    // Relay 上游只懂 /chat/completions（GLM Coding Plan / MiMo 等）→ 用 relay_translate 把
    // codex 的 /v1/responses 翻译成 chat 协议，调好上游再把响应（SSE 或 sync）反翻译回来。
    // 优先用 hard_route_relay（用户显式指定的路由）；否则用 current_relay_route。
    let effective_relay = hard_route_relay.as_ref().or(relay_route.as_ref());
    if let Some(r) = effective_relay {
        if r.protocol == "chat_completions" {
            return Ok(handle_chat_completions_relay(
                state.clone(),
                r.clone(),
                method.clone(),
                path_and_query.clone(),
                req_headers.clone(),
                body_bytes.clone(),
            )
            .await);
        }
    }

    // 提取 session_key：用于 affinity 路由 + cache hit 记账（client 模式 affinity 由 Server 处理）
    let session_key = crate::session_affinity::extract_session_key(&body_bytes, &req_headers);

    // ── client 模式：先转发到 Server；Server 不可达或返回 402 deactivated 时回退本地 ──
    // 注意：Relay 账号不再走 Server。Relay 用 API key 直连上游，没有账号池/保活/401-切号
    // 那套需求，多一跳 LAN 纯加延迟。让 Relay 在 client 机器本地直接打上游
    // （chat_completions 协议已在更上游分支返回；responses 协议落到下面 get_upstream
    // 直发 unity2/packycode）。
    let (remote_mode, client_direct_upstream) = state
        .store
        .lock()
        .map(|s| (s.settings.remote_mode.clone(), s.settings.client_direct_upstream))
        .unwrap_or((String::new(), false));

    // client_direct_upstream=true：HTTP 也走"本机直连上游"（跟 WS 同路）；
    // 只让 Server 管 RT/AT 轮换。跳过 forward_to_server 这一段，直接 fall through
    // 到下面的本地路径（resolve_token_with_affinity → forward_with_token）。
    // resolve_token_with_affinity 在 client 模式下会自动从 Server fetch_token，
    // 所以 token 中心化的语义保留。
    if remote_mode == "client" && relay_route.is_none() && !client_direct_upstream {
        // 先尝试 silent retry：peek 响应首 chunk，撞 usage_limit_reached 就切号重试，最多 3 次
        match forward_to_server_with_silent_retry(
            &state,
            &method,
            &path_and_query,
            &req_headers,
            &body_bytes,
            3,
        )
        .await
        {
            Ok(Some(resp)) => return Ok(resp),
            Ok(None) => {
                // 402 deactivated：走老路径，重新 forward 一次拿 body 检查
                match forward_to_server_parts(
                    &state,
                    &method,
                    &path_and_query,
                    &req_headers,
                    &body_bytes,
                )
                .await
                {
                    Ok(mini_resp) => {
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
                    Err(e) => {
                        eprintln!("[Proxy] 402 二次取 body 失败: {}", e);
                        // 落到下面 fall-through
                    }
                }
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
    //    hard_routed=true 表示命中用户主动定义的 session_routes（严格模式：不要切号/refresh）
    let (token, is_chatgpt, used_account_id, hard_routed) =
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
        // 严格模式：硬路由命中时，401/402/403 一律原样透回，不切号、不 silent_refresh、
        // 也不标记账号。用户主动指定了这个号，他需要看到上游真实错误来决定是否重登/解禁。
        if hard_routed {
            let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
            println!(
                "[Proxy] Hard route 严格模式：上游 {} 原样透回，不触发切号",
                status_code
            );
            return Ok(Response::builder()
                .status(status_code.as_u16())
                .header("content-type", "application/json")
                .body(full_body(resp_bytes))
                .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "响应构建失败")));
        }
        // 记下触发错误的账号名（在 silent_refresh / 切号污染 store.current 之前）
        let triggering_account_name: String = state
            .store
            .lock()
            .ok()
            .and_then(|s| {
                let id = s.current.clone()?;
                s.accounts.get(&id).map(|a| a.name.clone())
            })
            .unwrap_or_else(|| "未知".to_string());

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
            // current 是 Relay 时跳过 silent_refresh —— Relay 账号用静态 API Key，
            // 没有 refresh_token 也不会被轮换/过期。401 几乎只可能是上游 hiccup 或
            // key 被用户手动 revoke。silent_refresh 跑下来必返 NoRefreshToken，
            // 旧逻辑会把账号永久标 is_token_invalid → UI 显示"过期" → 用户困惑。
            let current_is_relay = state
                .store
                .lock()
                .ok()
                .and_then(|s| {
                    let id = s.current.clone()?;
                    s.accounts.get(&id).map(|a| a.is_relay())
                })
                .unwrap_or(false);
            if current_is_relay {
                println!("[Proxy] 拦截到 401（Relay 账号），跳过 silent_refresh 直接试切号");
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
                // 切号无果 → 把原 401 body 透回（用户能看到上游的真实错误信息）
                return Ok(Response::builder()
                    .status(status_code.as_u16())
                    .header("content-type", "application/json")
                    .body(full_body(resp_bytes))
                    .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "响应构建失败")));
            }
            // 非 Relay：401 可能是正常过期或被登出。
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
                SilentRefreshOutcome::LoggedOut
                | SilentRefreshOutcome::OtherError(_)
                | SilentRefreshOutcome::NoRefreshToken => {
                    // 只有"明确的 auth 信号"才持久标记失效（铁律：网络层/容量/auth 抖动一律不标）：
                    //   LoggedOut      = OAuth 端点明确 invalid_grant / 登出 → is_logged_out
                    //   NoRefreshToken = 根本没 refresh_token，无法恢复     → is_token_invalid
                    //   OtherError     = 网络抖动 / 429 / 5xx / 连不上 auth.openai.com（Server 经
                    //                    192.168.2.250→38 出口，抖动是常态）→ 瞬时，绝不标记失效，
                    //                    只切号让本次请求走通，账号状态保持原样（否则好号被误判"过期"）
                    let mark_field: Option<&str> = match &outcome {
                        SilentRefreshOutcome::LoggedOut => Some("logged_out"),
                        SilentRefreshOutcome::NoRefreshToken => Some("token_invalid"),
                        SilentRefreshOutcome::OtherError(_) => None,
                        _ => unreachable!(),
                    };
                    let log_tag = match &outcome {
                        SilentRefreshOutcome::LoggedOut => "账号已登出/RT 被轮换".to_string(),
                        SilentRefreshOutcome::NoRefreshToken => "缺 refresh_token 无法刷新".to_string(),
                        SilentRefreshOutcome::OtherError(e) => format!("瞬时刷新失败: {}", e),
                        _ => unreachable!(),
                    };
                    if let Some(field) = mark_field {
                        println!("[Proxy] silent_refresh 不可恢复（{}），标记 + 切号", log_tag);
                        if let Ok(mut store) = state.store.lock() {
                            if let Some(current_id) = store.current.clone() {
                                if let Some(acc) = store.accounts.get_mut(&current_id) {
                                    if field == "logged_out" {
                                        acc.is_logged_out = true;
                                    } else {
                                        acc.is_token_invalid = true;
                                    }
                                    let _ = store.save();
                                }
                            }
                        }
                    } else {
                        println!(
                            "[Proxy] silent_refresh 瞬时失败（{}），不标记失效，仅切号重试",
                            log_tag
                        );
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
            }
        }

        // 所有切号尝试都失败 / 账号池耗尽。
        // **不要把上游原始 401 body 透回 codex**（典型文案：
        //   "Your access token could not be refreshed because you have since
        //    logged out or signed in to another account."），用户看了一头雾水、
        //   不知道是哪个号、也不知道是切号链失败还是 codex 自己出问题。
        // 改成清楚的中文+英文错误，包含触发账号名，方便对照排查。
        let friendly = serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "code": "all_accounts_exhausted",
                "message": format!(
                    "[Codex Switcher] 触发账号「{}」失效或限额，自动切号链已经试过所有可用号都失败 / 已耗尽。请打开 Codex Switcher 检查账号面板：刷新配额、重新登录失效的号、或者手动切到健康的订阅号。原上游响应已存日志。 / Triggering account \"{}\" failed; auto-switch chain exhausted all candidates.",
                    triggering_account_name, triggering_account_name
                ),
                "param": null,
            }
        });
        let body_bytes = serde_json::to_vec(&friendly).unwrap_or(resp_bytes.to_vec());
        eprintln!(
            "[Proxy] 401/403 链路完全失败，原始上游 body 前 200 字符: {}",
            String::from_utf8_lossy(&resp_bytes).chars().take(200).collect::<String>()
        );
        let _ = state.app_handle.emit(
            "proxy-account-failed",
            &format!("账号「{}」401/限额耗尽，切号链失败", triggering_account_name),
        );
        return Ok(Response::builder()
            .status(status_code.as_u16())
            .header("content-type", "application/json")
            .body(full_body(Bytes::from(body_bytes)))
            .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "响应构建失败")));
    }

    // 7. HTTP 429：按 body 内容分流
    //   - body 含 server_is_overloaded / slow_down → **全局过载**，同号 backoff retry
    //     （切号撞同样错，烧账号没用）
    //   - body 含 usage_limit_reached / insufficient_quota / usage_not_included
    //     → per-account 配额耗尽，切号
    //   - 其他 429（无明确 code） → 默认切号
    //
    // `/responses/compact` 特殊处理：
    // 验证 codex 源码（codex-rs/codex-api/src/common.rs CompactionInput）后确认，
    // compact 请求体 **不带 response_id**，body 是 `input: Vec<ResponseItem>`（完整
    // 对话历史）+ 标准参数；headers 也明确传 `turn_state=None`。所以切到新号重试在
    // 协议层是合法的 —— 之前 0.5.14 关闭切号是基于"response_id 绑死账号"的错误假设。
    //
    // 这里改成：先乐观切号重试。
    //   - 重试拿到 2xx → 真无损切号，codex 不感知错误
    //   - 重试拿到 4xx（仍有别的服务端绑定如 session_id / 信任度问题）→ **不把 4xx 透回**
    //     codex（4xx 是 terminal，整条对话就死了），改成把原始 429 透回（transient，codex
    //     只是放弃这次 compact，session 继续工作）
    let is_compact_path = path_and_query.contains("/responses/compact");
    if status_code == reqwest::StatusCode::TOO_MANY_REQUESTS && is_compact_path {
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        if hard_routed {
            println!("[Proxy] Hard route 严格模式：compact 429 原样透回");
            return Ok(Response::builder()
                .status(429)
                .header("content-type", "application/json")
                .body(full_body(resp_bytes))
                .unwrap_or_else(|_| error_response(StatusCode::TOO_MANY_REQUESTS, "429")));
        }
        println!("[Proxy] compact 路径 429，尝试切号重试（最多 5 个号）...");
        mark_current_quota_depleted(&state);
        const COMPACT_MAX_SWITCH_ATTEMPTS: usize = 5;
        for attempt in 0..COMPACT_MAX_SWITCH_ATTEMPTS {
            let Some(retry_resp) = dispatch_quota_switch_retry(
                &state,
                &method,
                &upstream_url,
                &base_headers,
                &body_bytes,
                session_key.as_deref(),
                SwitchReason::Http429,
            )
            .await
            else {
                println!(
                    "[Proxy] compact 第 {}/{} 次切号无候选可选，停止",
                    attempt + 1,
                    COMPACT_MAX_SWITCH_ATTEMPTS
                );
                break;
            };

            let retry_status = retry_resp.status();
            if retry_status.is_success() {
                println!(
                    "[Proxy] compact 切号重试成功（{}），第 {} 次无损切号",
                    retry_status,
                    attempt + 1
                );
                return Ok(retry_resp);
            }

            // 把 4xx body 缓出来留作 log + 判断要不要再切
            let retry_headers = retry_resp.headers().clone();
            let retry_body = match retry_resp.into_body().collect().await {
                Ok(c) => c.to_bytes(),
                Err(e) => {
                    eprintln!("[Proxy] compact 重试 body 读取失败: {}", e);
                    Bytes::new()
                }
            };
            let preview: String = String::from_utf8_lossy(&retry_body)
                .chars()
                .take(400)
                .collect();
            println!(
                "[Proxy] compact 第 {} 次切号返 {} body[0:400]={:?}",
                attempt + 1,
                retry_status,
                preview
            );

            // 4xx 类账号特异性失败（plan 不够、session 绑定等）→ 当前账号也标耗尽，
            // 让 dispatch_quota_switch_retry 下一轮请求 Server 换不同的号
            if retry_status.is_client_error() {
                mark_current_quota_depleted(&state);
                if attempt + 1 < COMPACT_MAX_SWITCH_ATTEMPTS {
                    println!(
                        "[Proxy] compact 4xx 视为该号不可用，继续试下一个号 ({}/{})",
                        attempt + 2,
                        COMPACT_MAX_SWITCH_ATTEMPTS
                    );
                    continue;
                }
                // 全部用完仍 4xx → 伪装 429 给 codex（避免 4xx 触发 codex 终端崩）
                println!(
                    "[Proxy] compact 试完 {} 个号仍 4xx，伪装 429 透回（最后一次 status={}）",
                    COMPACT_MAX_SWITCH_ATTEMPTS, retry_status
                );
                break;
            }

            // 5xx 类（服务端临时） → 也透回 4xx body 让 codex 自己决定（已经能透回非 4xx）
            println!(
                "[Proxy] compact 切号后返 {} 5xx 类，透回原响应",
                retry_status
            );
            let mut builder = Response::builder().status(retry_status);
            for (k, v) in retry_headers.iter() {
                builder = builder.header(k, v);
            }
            return Ok(builder
                .body(full_body(retry_body))
                .unwrap_or_else(|_| error_response(retry_status, "5xx")));
        }
        return Ok(Response::builder()
            .status(429)
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap_or_else(|_| error_response(StatusCode::TOO_MANY_REQUESTS, "429")));
    }
    if status_code == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let resp_bytes = upstream_resp.bytes().await.unwrap_or_default();
        if hard_routed {
            println!("[Proxy] Hard route 严格模式：429 原样透回");
            return Ok(Response::builder()
                .status(429)
                .header("content-type", "application/json")
                .body(full_body(resp_bytes))
                .unwrap_or_else(|_| error_response(StatusCode::TOO_MANY_REQUESTS, "429")));
        }
        // Spark 模型 429 = Pro 专属子限额耗尽，与基础额度无关：**不切号、不标耗尽**，
        // 原样把 429 透回给 Spark 调用方（glance/hermes）。否则会把当前账号误判成没额度
        // 切走，连带把同账号上正常的 gpt-5.5 会话踢到没额度的号 → 任务停。
        if request_is_spark_model(&body_bytes) {
            println!("[Proxy] Spark 模型 429（Pro 子限额耗尽），不切号，原样透回 429");
            return Ok(Response::builder()
                .status(429)
                .header("content-type", "application/json")
                .body(full_body(resp_bytes))
                .unwrap_or_else(|_| error_response(StatusCode::TOO_MANY_REQUESTS, "429")));
        }
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
                let (retry_token, _) =
                    match resolve_token_with_affinity(&state, session_key.as_deref()).await {
                        Ok((t, c, _, _)) => (t, c),
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
            println!(
                "[Proxy] 上游 {} + 容量满，同号 backoff retry...",
                status_code
            );
            for attempt in 1..=3u64 {
                let backoff = std::time::Duration::from_secs(2 * attempt);
                tokio::time::sleep(backoff).await;
                let (retry_token, _) =
                    match resolve_token_with_affinity(&state, session_key.as_deref()).await {
                        Ok((t, c, _, _)) => (t, c),
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
        let preview: String = String::from_utf8_lossy(&resp_bytes)
            .chars()
            .take(1024)
            .collect();
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
    let resp = build_stream_response(
        upstream_resp,
        Some(state.tracker.clone()),
        session_affinity_ctx,
    );

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
        match forward_and_bootstrap(
            state,
            method,
            upstream_url,
            &headers_no_ts,
            &body_for_new,
            &new_token,
            session_key,
        )
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
    // 注意：anchor 设置时**只更新 store.current**，不动 disk auth.json，
    // 保住手机 bridge 走的 anchor 镜像。proxy 路由走 store.current 的 token，跟 disk 解耦。
    let auth_to_write = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        store.sync_account_from_auth_json(new_id, t.auth_json.clone());
        let auth = store.accounts.get(new_id).map(|acc| acc.to_codex_auth_value());
        store.current = Some(new_id.to_string());
        let _ = store.save();
        auth
    };
    if let Some(auth) = auth_to_write {
        // anchor-guarded：跳过 anchor 不匹配的写盘
        write_codex_auth_respecting_anchor(state, new_id, &auth);
    }
    // 同 do_switch 的理由：client 模式被 Server 推过来的切号也不该牵连无关 bridge。
    // 真正"该断的"那条 bridge 在 limit 检测里自己会送 Close。
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
    // Pre-flight：记录入口时的 current，以便最后兜底恢复
    let entry_current = state.store.lock().ok().and_then(|s| s.current.clone());

    // current 是 Relay 时按设置决定 401/429 是否自动切走
    {
        let store = state.store.lock().ok();
        let cur_is_relay = store
            .as_ref()
            .and_then(|s| s.current.clone())
            .and_then(|id| {
                store
                    .as_ref()
                    .unwrap()
                    .accounts
                    .get(&id)
                    .map(|a| a.is_relay())
            })
            .unwrap_or(false);
        let allow_out = store
            .map(|s| s.settings.relay_auto_switch_out)
            .unwrap_or(true);
        if cur_is_relay && !allow_out {
            println!(
                "[Proxy] current 是 Relay 且 relay_auto_switch_out=false，不自动切（{:?}）",
                reason
            );
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
                        println!(
                            "[Proxy] 第 {} 次切号目标号也容量满（全局过载），再试下一个",
                            attempt + 1
                        );
                        // 容量满不是该号的问题，不要 mark_quota_depleted；直接换下一个看运气
                        continue;
                    }
                    BootstrappedForward::Banned => {
                        println!("[Proxy] 第 {} 次切号目标号疑似封号", attempt + 1);
                        mark_current_banned(state);
                        continue;
                    }
                    BootstrappedForward::Unauthorized => {
                        // 切到的号 401：多半只是 access_token 过期（rt 还活着）——pick_next
                        // 给的是 store 里的旧 access_token，切号前没刷新。先静默刷新这个号
                        // （do_switch 后它已是 store.current）再重试一次；只有 OAuth 端点给出
                        // 明确登出 / invalid_grant 才标记，网络 / 瞬时 / 跨 IP 吊销一律不标，
                        // 避免把 rt 健康的好号误判成"过期"（团队/免费号被切号风暴扫射误伤）。
                        match silent_refresh_current(state).await {
                            SilentRefreshOutcome::Refreshed(fresh) => {
                                match forward_and_bootstrap(
                                    state,
                                    method,
                                    upstream_url,
                                    &headers_no_ts,
                                    &body_for_new,
                                    &fresh,
                                    session_key,
                                )
                                .await
                                {
                                    BootstrappedForward::Ok(resp) => {
                                        println!(
                                            "[Proxy] 第 {} 次切号目标号刷新后成功",
                                            attempt + 1
                                        );
                                        return Some(resp);
                                    }
                                    _ => {
                                        // 刷新出新 token 仍 401 = 瞬时/跨 IP 吊销，本轮跳过不标记
                                        println!(
                                            "[Proxy] 第 {} 次切号目标号刷新后仍 401（瞬时），跳过不标记",
                                            attempt + 1
                                        );
                                        continue;
                                    }
                                }
                            }
                            SilentRefreshOutcome::LoggedOut => {
                                println!(
                                    "[Proxy] 第 {} 次切号目标号已登出/RT 轮换，标记 is_logged_out",
                                    attempt + 1
                                );
                                if let Ok(mut store) = state.store.lock() {
                                    if let Some(acc) = store.accounts.get_mut(&id) {
                                        acc.is_logged_out = true;
                                        let _ = store.save();
                                    }
                                }
                                continue;
                            }
                            SilentRefreshOutcome::NoRefreshToken => {
                                println!(
                                    "[Proxy] 第 {} 次切号目标号缺 refresh_token，标记 is_token_invalid",
                                    attempt + 1
                                );
                                if let Ok(mut store) = state.store.lock() {
                                    if let Some(acc) = store.accounts.get_mut(&id) {
                                        acc.is_token_invalid = true;
                                        let _ = store.save();
                                    }
                                }
                                continue;
                            }
                            SilentRefreshOutcome::OtherError(e) => {
                                println!(
                                    "[Proxy] 第 {} 次切号目标号刷新瞬时失败（{}），跳过不标记",
                                    attempt + 1,
                                    e
                                );
                                continue;
                            }
                        }
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
                restore_current_if_flagged(state, entry_current.as_deref());
                return None;
            }
        }
    }
    restore_current_if_flagged(state, entry_current.as_deref());
    None
}

/// `try_switch_and_retry` 的 retry 循环里每次 attempt 都会先 `do_switch` 改
/// `store.current`，所以失败收尾时 store.current 通常停在最后一个被试过的（已被
/// 标记为 banned / token_invalid / logged_out 的）坏账号上。下次 proxy 收到请求
/// 会用这个坏号 → 又 401/429 → 又走 try_switch_and_retry → 死循环。
///
/// 这里收尾：如果 `store.current` 是已标记的坏号，尝试把 current 改回入口时的
/// `entry_id`（如果它还健康），否则随便找一个健康账号顶上。**不发任何请求**，
/// 纯本地 store 矫正。
fn restore_current_if_flagged(state: &ProxyState, entry_id: Option<&str>) {
    let mut store = match state.store.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let current_id = match store.current.clone() {
        Some(id) => id,
        None => return,
    };
    let current_flagged = store
        .accounts
        .get(&current_id)
        .map(|a| a.is_banned || a.is_token_invalid || a.is_logged_out)
        .unwrap_or(false);
    if !current_flagged {
        return; // current 健康，不动
    }

    // 先看 entry 那个号还能不能用
    let entry_ok = entry_id
        .filter(|eid| *eid != current_id.as_str())
        .and_then(|eid| store.accounts.get(eid))
        .map(|a| !a.is_banned && !a.is_token_invalid && !a.is_logged_out)
        .unwrap_or(false);
    // 兜底候选：遵守 `relay_auto_switch_in` 约束 —— 默认 false 时不能挑 Relay
    // 类账号（否则用户手切到订阅号、自动切链失败后会被收尾切到 GLM/MiMo 这种
    // 中转，违反"非订阅号只能手动切"的预期）。
    let allow_relay = store.settings.relay_auto_switch_in;
    let revert_to: Option<String> = if entry_ok {
        entry_id.map(String::from)
    } else {
        store
            .accounts
            .iter()
            .find(|(id, a)| {
                id.as_str() != current_id.as_str()
                    && !a.is_banned
                    && !a.is_token_invalid
                    && !a.is_logged_out
                    && (allow_relay || !a.is_relay())
            })
            .map(|(id, _)| id.clone())
    };
    if let Some(target_id) = revert_to {
        let target_name = store
            .accounts
            .get(&target_id)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        println!(
            "[Proxy] 切号链失败收尾：current 是坏号 {}, 矫正回 {}",
            current_id, target_id
        );
        if let Err(e) = store.switch_to(&target_id, true) {
            eprintln!("[Proxy] 收尾矫正 switch_to 失败: {}", e);
        } else {
            let _ = store.save();
            invalidate_remote_token_cache();
            let _ = state.app_handle.emit("proxy-account-switched", &target_name);
            let _ = state.app_handle.emit("accounts-updated", ());
        }
    } else {
        eprintln!("[Proxy] 切号链失败收尾：无健康账号可恢复，current 仍指向坏号");
    }
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
fn headers_without_turn_state(base: &reqwest::header::HeaderMap) -> reqwest::header::HeaderMap {
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

/// chat_completions Relay 专用 header 构造：只透传 Accept / User-Agent 等无害的，
/// 不带 codex 私有 header（x-codex-*、session_id、thread_id、OpenAI-Beta、
/// traceparent 等）。
///
/// 历史背景：在 GLM Coding Plan 边缘 WAF 上观测到，只要带 codex 那堆 `x-codex-*`
/// header POST `/api/coding/paas/v4/chat/completions`，请求就会被路由到一个
/// 不接 POST 的 handler，返回 `405 METHOD_NOT_ALLOWED`。curl 同样的 URL + body
/// 但不带 codex header，照样 200。chat_completions 协议本身只需要 Auth + JSON。
fn build_chat_relay_upstream_headers(upstream_host: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json, text/event-stream"),
    );
    h.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("codex-switcher-relay/1.0"),
    );
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

    let reqwest_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    // Server 永远是 LAN/ZeroTier 私有 IP，必须绕开系统代理（防 Clash 截走）。
    // state.client 是共享 client、走系统代理用于打 ChatGPT/Relay 上游，这里另起一个。
    let no_proxy_client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("构建 no_proxy client 失败: {}", e))?;
    no_proxy_client
        .request(reqwest_method, &upstream_url)
        .headers(fwd_headers)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| format!("转发到 Server 失败: {}", e))
}

/// 写 `~/.codex/auth.json` 但**尊重手机锚约束**：anchor 设置了且 account_id != anchor
/// 时跳过写盘并 log，避免把 anchor 的 disk 镜像覆盖掉、手机 bridge 鉴权挂。
///
/// 适用于"被动"写盘路径：silent_refresh、adopt_remote_current、proxy 拿 Server token
/// 后回写本机这类 background 操作。手动切号（lib.rs::switch_account）走的是"主动"路径，
/// 用户明知道在切，那条继续无视 anchor、覆盖 disk。
fn write_codex_auth_respecting_anchor(
    state: &ProxyState,
    account_id: &str,
    auth: &serde_json::Value,
) {
    let blocked = state
        .store
        .lock()
        .map(|s| !s.should_write_disk_for(account_id))
        .unwrap_or(false);
    if blocked {
        println!(
            "[Proxy] 手机锚生效，跳过写 ~/.codex/auth.json（{} != anchor）",
            account_id
        );
        return;
    }
    if let Err(e) = crate::account::AccountStore::write_codex_auth_extended_expiry(auth) {
        eprintln!("[Proxy] 写 ~/.codex/auth.json 失败: {}", e);
    }
}

/// 在 client mode 把 forward_to_server_parts 的响应 peek 一下首 chunk，看是否撞限额；
/// 撞了就调 Server /switch + retry，最多 `max_retries` 次。
///
/// **核心目的**：实现"无损切号" —— 让 codex 看不到 usage_limit_reached 错误。
/// 之前 client mode HTTP 路径裸透传响应（`build_stream_response(mini_resp, None, None)`），
/// chatgpt.com 返的 usage_limit error 直接传到 codex Desktop UI，用户看到 "已切号但还是错"
/// 的错觉 —— 实际上 WS 路径异步切了号，但这条 HTTP 请求已经把 error body 透回去了。
///
/// 返回值：
/// - `Ok(Some(resp))`：要返给 codex 的响应（成功或最后一次重试失败都走这里）
/// - `Ok(None)`：402 Deactivated，让外层走 try_local_fallback 老路径
/// - `Err(e)`：forward_to_server 本身报错（Server 不可达），让外层 fall-through 到本地直连
async fn forward_to_server_with_silent_retry(
    state: &ProxyState,
    method: &hyper::Method,
    path_and_query: &str,
    req_headers: &hyper::HeaderMap,
    body: &Bytes,
    max_retries: u32,
) -> Result<Option<Response<ProxyBody>>, String> {
    // peek 限额检测复用 PER_ACCOUNT_LIMIT_KEYWORDS：除了 *_reached / *_exceeded 等
    // wire code，还含人类可读文案 "usage limit" / "hit your usage limit" / "too many
    // requests" —— Team/组织管理员设的用量上限文案
    // ("You've hit your usage limit. ... send a request to your admin ...") 的 code
    // 不一定是 usage_limit_reached，必须靠 message 文本兜底，否则不切号。
    //
    // Spark 请求例外：Spark 是 Pro 专属子限额，跟基础额度是两个池子。它的 429 不该触发
    // 切号（Server 端 handle_request 也对 spark 429 原样透回），这里把 max_retries 归零，
    // 只 forward 一次、不调 /switch，直接把 429 透回 Spark 调用方。
    let max_retries = if request_is_spark_model(body) {
        0
    } else {
        max_retries
    };
    for attempt in 0..=max_retries {
        let mini_resp =
            match forward_to_server_parts(state, method, path_and_query, req_headers, body).await {
                Ok(r) => r,
                Err(e) => return Err(e),
            };
        let status = mini_resp.status();
        let headers = mini_resp.headers().clone();

        // 402 deactivated 走外层老路径处理 try_local_fallback
        if status == reqwest::StatusCode::PAYMENT_REQUIRED {
            // 注意：这里没消费 body —— 外层会重新调用一次拿 body，但 Server 此时
            // current 账号没变（我们没 switch），第二次调返回相同结果。可接受。
            return Ok(None);
        }

        // peek 第一个 chunk，看是否限额（限额 frame 通常 <2KB，第一个 chunk 一定能拿到）
        let mut stream = mini_resp.bytes_stream();
        let first_chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                return Err(format!("silent_retry: 读首 chunk 失败: {}", e));
            }
            None => Bytes::new(),
        };

        let peek_lower = String::from_utf8_lossy(&first_chunk).to_lowercase();
        let is_rate_limit = PER_ACCOUNT_LIMIT_KEYWORDS
            .iter()
            .any(|kw| peek_lower.contains(kw));

        if is_rate_limit && attempt < max_retries {
            println!(
                "[Proxy] silent_retry: client HTTP 响应限额，切号重试 ({}/{})",
                attempt + 1,
                max_retries
            );
            // 调 Server /switch；失败也继续 retry（也许 Server 自己切了号了）
            if let Err(e) = call_remote_switch_silently(state).await {
                eprintln!("[Proxy] silent_retry: /switch 调用失败: {}", e);
            }
            // 把这次的 stream 丢掉，下一轮重 forward
            drop(stream);
            // 给 Server 一点时间把 current 切完毕 + token 同步
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }

        // 不限额，或最后一次尝试 —— 把 first_chunk 当 prefix，剩余 stream 续上
        let rest = stream.boxed();
        let resp =
            build_stream_response_from_parts(status, headers, first_chunk, rest, None, None);
        if is_rate_limit {
            println!(
                "[Proxy] silent_retry: max_retries={} 用完仍限额，把最后一次响应透回 codex",
                max_retries
            );
        }
        return Ok(Some(resp));
    }
    unreachable!("retry loop 总要返回")
}

/// 调 Server /switch 让它切换 current 账号。
/// silent_retry 用，不暴露错误细节给上层（已经在 log 里打了）。
async fn call_remote_switch_silently(state: &ProxyState) -> Result<(), String> {
    let (primary, fallback, secret, cur_id) = {
        let s = state.store.lock().map_err(|e| e.to_string())?;
        (
            s.settings.remote_server_url.clone(),
            s.settings.remote_server_url_fallback.clone(),
            s.settings.remote_shared_secret.clone(),
            s.current.clone(),
        )
    };
    if secret.is_empty() {
        return Err("未配置 remote_shared_secret".to_string());
    }
    let base = crate::remote_client::resolve_base_url(&primary, &fallback)
        .await
        .map_err(|e| format!("resolve_base_url: {}", e))?;
    let outcome = crate::remote_client::request_switch(
        &base,
        &secret,
        cur_id.as_deref(),
        "silent retry: client HTTP usage_limit_reached",
    )
    .await?;
    if outcome.switched {
        println!(
            "[Proxy] silent_retry: Server 已切到 {} ({})",
            outcome.name.clone().unwrap_or_else(|| "?".to_string()),
            outcome
                .current
                .clone()
                .unwrap_or_else(|| "(unknown id)".to_string())
        );
    } else {
        println!(
            "[Proxy] silent_retry: Server switch returned switched=false（可能全部 exhausted）"
        );
    }
    Ok(())
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
    match forward_and_bootstrap(
        state,
        method,
        &upstream_url,
        &headers_no_ts,
        &body_for_new,
        &token,
        session_key,
    )
    .await
    {
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
            // 回退号 401：多半只是 access_token 过期（pick_next 给的是旧 token，没刷新）。
            // do_switch 后它已是 current，先静默刷新再重试一次；只有明确登出 / 缺 rt 才标记，
            // 网络 / 瞬时一律不标，避免把 rt 健康的好号误判成"过期"。
            match silent_refresh_current(state).await {
                SilentRefreshOutcome::Refreshed(fresh) => {
                    match forward_and_bootstrap(
                        state,
                        method,
                        &upstream_url,
                        &headers_no_ts,
                        &body_for_new,
                        &fresh,
                        session_key,
                    )
                    .await
                    {
                        BootstrappedForward::Ok(resp) => {
                            println!("[Proxy] 本地回退账号刷新后转发成功");
                            Some(resp)
                        }
                        _ => {
                            println!("[Proxy] 本地回退账号刷新后仍 401（瞬时），不标记");
                            None
                        }
                    }
                }
                SilentRefreshOutcome::LoggedOut => {
                    println!("[Proxy] 本地回退账号已登出/RT 轮换，标记 is_logged_out");
                    if let Ok(mut store) = state.store.lock() {
                        if let Some(acc) = store.accounts.get_mut(&id) {
                            acc.is_logged_out = true;
                            let _ = store.save();
                        }
                    }
                    None
                }
                SilentRefreshOutcome::NoRefreshToken => {
                    println!("[Proxy] 本地回退账号缺 refresh_token，标记 is_token_invalid");
                    if let Ok(mut store) = state.store.lock() {
                        if let Some(acc) = store.accounts.get_mut(&id) {
                            acc.is_token_invalid = true;
                            let _ = store.save();
                        }
                    }
                    None
                }
                SilentRefreshOutcome::OtherError(e) => {
                    println!("[Proxy] 本地回退账号刷新瞬时失败（{}），不标记", e);
                    None
                }
            }
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

type ByteStream = futures_util::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>;

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
    let resp =
        match forward_with_token(state, method, upstream_url, base_headers, body, token).await {
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
        return BootstrappedForward::Ok(build_stream_response(
            resp,
            Some(state.tracker.clone()),
            aff,
        ));
    }
    let headers = resp.headers().clone();
    let stream = resp.bytes_stream().boxed();
    let (byte_cap, time_cap_ms) = read_bootstrap_caps(state);
    match read_sse_bootstrap(stream, byte_cap, time_cap_ms).await {
        SseBootstrap::Ready { prefix, rest } => {
            BootstrappedForward::Ok(build_stream_response_from_parts(
                status,
                headers,
                prefix,
                rest,
                Some(state.tracker.clone()),
                aff,
            ))
        }
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
            let remote_result = try_remote_switch_and_retry(
                state,
                method,
                upstream_url,
                base_headers,
                body,
                session_key,
                remote_label,
            )
            .await;
            if remote_result.is_some() {
                state.switching.store(false, Ordering::SeqCst);
                return remote_result;
            }
            // 远端切号失败（Server 不可达 / Server 报"全部耗尽" / token 失效等）。
            // 已经有"原请求 Server 不可达时 fall through 本地直发"的逻辑，限额后的切号
            // 重试也应该走同样的本地兜底，否则用户就会看到原始 429 body（"hit your usage
            // limit"）而代理静默不切。
            println!("[Proxy] client 远端切号无果，降级本地 try_switch_and_retry 兜底");
            let local_result = try_switch_and_retry(
                state,
                method,
                upstream_url,
                base_headers,
                body,
                session_key,
                reason,
            )
            .await;
            state.switching.store(false, Ordering::SeqCst);
            return local_result;
        }
        // 别人正在切号 → 短等后用最新 current 直接重发
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok((new_token, _)) = get_current_token(state).await {
            if let BootstrappedForward::Ok(resp) = forward_and_bootstrap(
                state,
                method,
                upstream_url,
                base_headers,
                body,
                &new_token,
                session_key,
            )
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
        let result = try_switch_and_retry(
            state,
            method,
            upstream_url,
            base_headers,
            body,
            session_key,
            reason,
        )
        .await;
        state.switching.store(false, Ordering::SeqCst);
        return result;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if let Ok((new_token, _)) = get_current_token(state).await {
        if let BootstrappedForward::Ok(resp) = forward_and_bootstrap(
            state,
            method,
            upstream_url,
            base_headers,
            body,
            &new_token,
            session_key,
        )
        .await
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
fn wrap_with_sse_heartbeat(inner: ByteStream, interval: std::time::Duration) -> ByteStream {
    futures_util::stream::unfold(inner, move |mut s| async move {
        match tokio::time::timeout(interval, s.next()).await {
            Ok(Some(item)) => Some((item, s)),
            Ok(None) => None,
            Err(_) => Some((Ok(Bytes::from_static(b": keep-alive\n\n")), s)),
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
                    return SseBootstrap::Ready {
                        prefix: Bytes::from(buf),
                        rest: stream,
                    };
                }
            }
            Ok(Some(Err(_))) => {
                return SseBootstrap::Ready {
                    prefix: Bytes::from(buf),
                    rest: stream,
                };
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
                    if tx
                        .send(Ok(Bytes::from_static(b": keep-alive\n\n")))
                        .await
                        .is_err()
                    {
                        // client 已断
                        return SseBootstrap::Ready {
                            prefix: Bytes::from(buf),
                            rest: stream,
                        };
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
        let base = crate::remote_client::resolve_base_url(&primary, &fallback)
            .await
            .ok()?;
        let label = match &reason {
            SwitchReason::InStreamRateLimit => "in_stream_rate_limit",
            SwitchReason::InStreamBanned => "in_stream_banned",
            _ => "http_429",
        };
        let outcome =
            crate::remote_client::request_switch(&base, &secret, current_id.as_deref(), label)
                .await
                .ok()?;
        if outcome.exhausted {
            return None;
        }
        let new_current = outcome.current?;
        adopt_remote_current(state, &base, &secret, &new_current)
            .await
            .ok()?;
        invalidate_remote_token_cache();
        let (new_token, _) = get_current_token(state).await.ok()?;
        // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
        let body_for_new = rewrite_prompt_cache_key(body, &new_current);
        let headers_no_ts = headers_without_turn_state(base_headers);
        let resp = forward_with_token(
            state,
            method,
            upstream_url,
            &headers_no_ts,
            &body_for_new,
            &new_token,
        )
        .await
        .ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        return Some(resp.bytes_stream().boxed());
    }

    // 本地模式
    let pick = pick_next_account(state);
    let PickResult::Found { id, token } = pick else {
        return None;
    };
    do_switch(state, &id, reason).ok()?;
    // 切号到新账号 → prompt_cache_key 拼 account_id + 剥 x-codex-turn-state
    let body_for_new = rewrite_prompt_cache_key(body, &id);
    let headers_no_ts = headers_without_turn_state(base_headers);
    let resp = forward_with_token(
        state,
        method,
        upstream_url,
        &headers_no_ts,
        &body_for_new,
        &token,
    )
    .await
    .ok()?;
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
        let outcome =
            bootstrap_with_heartbeats(current_upstream, byte_cap, time_cap_ms, &tx, heartbeat)
                .await;

        match outcome {
            SseBootstrap::Ready { prefix, mut rest } => {
                if !prefix.is_empty() {
                    if tx.send(Ok(prefix)).await.is_err() {
                        return;
                    }
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
                            if tx.send(Ok(chunk)).await.is_err() {
                                return;
                            }
                        }
                        Ok(Some(Err(e))) => {
                            // 上游 chunk 错误，原样透给 client（让 codex 自己处理传输错误）
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                        Ok(None) => return,
                        Err(_) => {
                            if tx
                                .send(Ok(Bytes::from_static(b": keep-alive\n\n")))
                                .await
                                .is_err()
                            {
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
                    &state,
                    &method,
                    &upstream_url,
                    &base_headers,
                    &body,
                    SwitchReason::InStreamRateLimit,
                )
                .await
                {
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
                        &state,
                        &method,
                        &upstream_url,
                        &base_headers,
                        &body,
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
                    &state,
                    &method,
                    &upstream_url,
                    &base_headers,
                    &body,
                    SwitchReason::Http429,
                )
                .await
                {
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
                    &state,
                    &method,
                    &upstream_url,
                    &base_headers,
                    &body,
                    SwitchReason::InStreamBanned,
                )
                .await
                {
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
    // 仅 SSE 响应才套 keep-alive heartbeat。非 SSE（如 /compact 返回的 JSON）注入
    // `: keep-alive\n\n` 会把 0x0A 控制字节塞进 JSON 字符串内，codex 解析时报
    // `control character ( -) found while parsing a string` 直接挂掉。
    let is_sse = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_lowercase().contains("event-stream"))
        .unwrap_or(false);
    let rest_after_wrap: ByteStream = if is_sse {
        wrap_with_sse_heartbeat(rest, std::time::Duration::from_secs(30))
    } else {
        rest
    };
    let raw_stream: ByteStream = prefix_stream.chain(rest_after_wrap).boxed();

    let stream = raw_stream.map(move |result| match result {
        Ok(bytes) => {
            if let Ok(mut buf) = buf_clone.lock() {
                buf.extend_from_slice(&bytes);
                // 审查嗅探：每条 SSE 流第一次命中即异步 dump 整段 buf。
                // 扫描窗口 = 新 chunk + 16KB 尾巴（防关键词跨 chunk 切断），
                // 避免对整段缓冲做 O(n²) 重扫。
                if moderation_sniff_enabled && !moderation_dumped_clone.load(Ordering::Relaxed) {
                    let tail_overlap = 16 * 1024;
                    let scan_start = buf.len().saturating_sub(bytes.len() + tail_overlap);
                    let window_lower = String::from_utf8_lossy(&buf[scan_start..]).to_lowercase();
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
                    // 每个 response.completed 都记 affinity binding，不要求 cache 命中
                    // —— 首轮必然 cold cache，要等到第二轮才看见命中，期间 session 可能
                    // 已经被切走了。详见 session_affinity::record_cache_hit 注释。
                    if let Some(ctx) = &affinity_clone {
                        ctx.affinity.record_cache_hit(
                            &ctx.session_key,
                            &ctx.account_id,
                            usage.cached_input_tokens,
                        );
                    }
                    let session_key_for_record = affinity_clone
                        .as_ref()
                        .map(|c| c.session_key.clone())
                        .unwrap_or_default();
                    usage.account_id = account_id_for_record;
                    usage.session_key = session_key_for_record;
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
                                    // 写 quota 快照
                                    let email_snap = state
                                        .store
                                        .lock()
                                        .ok()
                                        .and_then(|s| {
                                            s.accounts.get(&id).and_then(|a| {
                                                crate::account::AccountStore::extract_email(
                                                    &a.auth_json,
                                                )
                                            })
                                        })
                                        .unwrap_or_default();
                                    crate::quota_snapshot::append_from_usage(
                                        &id,
                                        &email_snap,
                                        &usage,
                                        "ws_precheck",
                                    );
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
                                                reset_credits: usage.reset_credits,
                                                spark: usage.spark.clone(),
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
    let (http_url, _upstream_host) = get_upstream(is_chatgpt, relay_base_url.as_deref(), &path);

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
            // 切号策略（与 OpenAI 风控对齐）：**只有上游明确给出 per-account 额度信号
            // （429 / usage_limit_reached / insufficient_quota 等）才允许切号**。
            //   - global 容量满（503 / at capacity / overloaded）→ 同号 backoff retry，不切号
            //   - 401/403 auth → 原地 silent_refresh 当前账号，不切号
            //   - 纯网络层 / 502 / 504 / 未知 → 直接返回错误让 codex 自己重连，不切号
            // 频繁切号 = 频繁改写 ~/.codex/auth.json / 轮换身份，会被 OpenAI 判异常封号；
            // 而把网络抖动当成"额度耗尽"切号，还会把整个号池误标 five_hour_left=0。
            let is_global_capacity =
                err_lower.contains("503") || matches_global_capacity(&err_lower);
            let is_per_account_limit = err_lower.contains("429")
                || PER_ACCOUNT_LIMIT_KEYWORDS
                    .iter()
                    .any(|kw| err_lower.contains(kw));

            // 复用：用给定 token / 协议 / relay base 构建上游 WS 请求（透明 header + 注入 Bearer）
            let make_upstream_req = |tok: &str,
                                     chatgpt: bool,
                                     relay: Option<&str>|
             -> Option<tungstenite::http::Request<()>> {
                let (h_url, _) = get_upstream(chatgpt, relay, &path);
                let w_url = h_url
                    .replacen("https://", "wss://", 1)
                    .replacen("http://", "ws://", 1);
                let mut r = w_url.as_str().into_client_request().ok()?;
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
                    r.headers_mut().insert(name.clone(), value.clone());
                }
                if let Ok(av) = HeaderValue::from_str(&format!("Bearer {}", tok)) {
                    r.headers_mut().insert(hyper::header::AUTHORIZATION, av);
                }
                Some(r)
            };

            // ───── 1) 全局容量满 → 同号 backoff retry，绝不切号 ─────
            if is_global_capacity && !is_per_account_limit {
                println!("[Proxy] WebSocket 握手被上游拒绝（容量满），同号 backoff retry...");
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
                    let (cur_url, _) = get_upstream(cur_chatgpt, cur_relay.as_deref(), &path);
                    let ws = cur_url
                        .replacen("https://", "wss://", 1)
                        .replacen("http://", "ws://", 1);
                    if let Ok(mut r) = ws.as_str().into_client_request() {
                        for (n, v) in req.headers() {
                            let l = n.as_str().to_lowercase();
                            if matches!(
                                l.as_str(),
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
                    // 容量满是上游全局问题，切号也撞同样错且烧账号 → 不切号，直接报错让 codex 重连
                    println!("[Proxy] 容量满同号 retry 三次都失败，不切号，返回错误");
                    return Ok(error_response(
                        StatusCode::BAD_GATEWAY,
                        "上游容量满，已同号重试无果",
                    ));
                }
            } else if is_per_account_limit {
                // ───── 2) 明确 per-account 限额 → 切号（唯一允许切号的情况）─────
                println!("[Proxy] WebSocket 握手命中 per-account 限额，标记当前号耗尽并切号...");
                mark_current_quota_depleted(&state);

                // 最多试 3 个号。注意：只有候选号自己也回 per-account 限额才标它耗尽 + 继续换；
                // 候选号若是网络层 / 其它非额度错误 → 是网络坏了不是号坏了，立刻停止，绝不标耗尽
                //（否则一波网络抖动会把整个号池误标 five_hour_left=0，连其它会话都切不动）。
                let mut retry_conn = None;
                for _attempt in 0..3 {
                    let PickResult::Found { id, token: new_tok } = pick_next_account(&state) else {
                        break;
                    };
                    if do_switch(&state, &id, SwitchReason::WebSocketPrecheck).is_err() {
                        continue;
                    }
                    let new_chatgpt = new_tok.starts_with("eyJ");
                    let new_relay = account_relay_base_url(&state, &id);
                    let Some(r) = make_upstream_req(&new_tok, new_chatgpt, new_relay.as_deref())
                    else {
                        continue;
                    };
                    match tokio_tungstenite::connect_async(r).await {
                        Ok(c) => {
                            println!("[Proxy] WebSocket 切号重连成功（{}）", id);
                            retry_conn = Some(c);
                            break;
                        }
                        Err(e2) => {
                            let e2l = e2.to_string().to_lowercase();
                            let e2_is_limit = e2l.contains("429")
                                || PER_ACCOUNT_LIMIT_KEYWORDS.iter().any(|kw| e2l.contains(kw));
                            if e2_is_limit {
                                // 这个号也确实满了 → 标耗尽，换下一个
                                println!("[Proxy] 切到 {} 仍 per-account 限额，标耗尽换下一个", id);
                                mark_current_quota_depleted(&state);
                                continue;
                            }
                            // 网络层 / 其它非额度错误 → 不是号的问题，停止切号、不污染号池
                            println!(
                                "[Proxy] 切到 {} 后非额度错误（{}），停止切号（不标耗尽）",
                                id, e2
                            );
                            break;
                        }
                    }
                }

                match retry_conn {
                    Some(conn) => conn,
                    None => {
                        return Ok(error_response(
                            StatusCode::BAD_GATEWAY,
                            "per-account 限额切号后仍无可用账号",
                        ));
                    }
                }
            } else if is_auth_err {
                // ───── 3) auth（401/403）→ 原地刷新当前账号 token，不切号 ─────
                println!("[Proxy] WebSocket 握手 401/403，原地 silent_refresh 当前账号（不切号）...");
                match silent_refresh_current(&state).await {
                    SilentRefreshOutcome::Refreshed(new_tok) => {
                        let new_chatgpt = new_tok.starts_with("eyJ");
                        match make_upstream_req(&new_tok, new_chatgpt, relay_base_url.as_deref()) {
                            Some(r) => match tokio_tungstenite::connect_async(r).await {
                                Ok(c) => c,
                                Err(e2) => {
                                    return Ok(error_response(
                                        StatusCode::BAD_GATEWAY,
                                        &format!("刷新后 WebSocket 仍连接失败: {}", e2),
                                    ));
                                }
                            },
                            None => {
                                return Ok(error_response(
                                    StatusCode::BAD_GATEWAY,
                                    "刷新后 WebSocket 请求构建失败",
                                ));
                            }
                        }
                    }
                    _ => {
                        // LoggedOut / NoRefreshToken / OtherError：不在 WS 路径切号（避免烧号），
                        // 报错让 codex 自己重连；真要换号交给用户或 HTTP 路径决策
                        return Ok(error_response(
                            StatusCode::BAD_GATEWAY,
                            "WebSocket 认证失败（已尝试刷新当前账号，未切号）",
                        ));
                    }
                }
            } else {
                // ───── 4) 纯网络层 / 502 / 504 / 未知 → 不切号，直接返回让 codex 重连 ─────
                eprintln!("[Proxy] WebSocket 上游连接失败（网络层/非额度，不切号）: {}", e);
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    "WebSocket 上游连接失败",
                ));
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
    "server_is_overloaded", // ★ codex 真正认的错误码
    "slow_down",            // ★ 同上
    "at capacity",
    "selected model is at capacity",
    "try a different model",
    "model_overloaded",
    "model overloaded",
    "service unavailable",
];

/// 任意限额/容量信号（任一类都算"上游不让发"）
fn matches_any_limit_signal(lower: &str) -> bool {
    PER_ACCOUNT_LIMIT_KEYWORDS
        .iter()
        .any(|kw| lower.contains(kw))
        || GLOBAL_CAPACITY_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

fn matches_global_capacity(lower: &str) -> bool {
    GLOBAL_CAPACITY_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// 老 const，保留是为了避免改太多调用点；语义保持"任何限额/容量信号"。
const RATE_LIMIT_KEYWORDS: &[&str] = &[
    // codex 二进制实际识别的错误码（codex-rs/codex-api/src/sse/responses.rs:543-570）
    "server_is_overloaded", // ★ 全局 capacity（中间有 _is_）
    "slow_down",            // ★ 全局 capacity
    "usage_limit_reached",  // ★ per-account 配额耗尽
    "usage_not_included",   // ★ 该号订阅不含 codex
    "insufficient_quota",   // ★ per-account 配额
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

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // response.failed / error 类型：必须看内层 error.code / error.type，
            // 不能假设所有 response.failed 都是限额。上游因 model_error /
            // content_policy / invalid_request_error / 等非限额原因也会发
            // response.failed，盲目切号会浪费账号 + 让 codex 看到「ws closed
            // before response.completed」+ Reconnecting。
            if msg_type == "response.failed" || msg_type == "error" {
                let error_obj = val
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| val.get("error"));
                let inner_code = error_obj
                    .and_then(|e| e.get("code").or_else(|| e.get("type")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let inner_msg = error_obj
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // 已知的限额 / 容量 code（is_capacity_only 后面再细分全局 vs 单号）
                const RATE_LIMIT_CODES: &[&str] = &[
                    "usage_limit_reached",
                    "rate_limit_exceeded",
                    "rate_limit_error",
                    "insufficient_quota",
                    "billing_hard_limit_reached",
                    "server_is_overloaded",
                    "model_overloaded",
                    "slow_down",
                ];

                let code_match = RATE_LIMIT_CODES
                    .iter()
                    .any(|c| inner_code.eq_ignore_ascii_case(c));
                let inner_msg_lower = inner_msg.to_lowercase();
                let msg_match = inner_msg_lower.contains("usage limit")
                    || inner_msg_lower.contains("rate limit")
                    || inner_msg_lower.contains("too many requests")
                    || inner_msg_lower.contains("hit your usage limit");

                if code_match || msg_match {
                    println!("[Proxy] WS 限额: type={} code={}", msg_type, inner_code);
                    return true;
                }

                // 非限额错误（model_error / invalid_request / content_policy 等）
                // → 透传给 codex 让它按错误自己处理，不要切号 + 关 WS
                let preview: String = inner_msg.chars().take(120).collect();
                println!(
                    "[Proxy] WS 非限额错误（type={} code={} msg={:?}），透传不切号",
                    msg_type, inner_code, preview
                );
                return false;
            }

            // 有 error 字段且非 null 才判定（response.created 等 in_progress 事件
            // 协议规定带 "error":null，serde_json 的 get() 对 null 也返回 Some(&Null)，
            // 必须显式排除 null，否则会把正常 in_progress 误判成限额、无限切号）
            let response_error_real = val
                .get("response")
                .and_then(|r| r.get("error"))
                .map(|e| !e.is_null())
                .unwrap_or(false);
            let outer_error_real = val.get("error").map(|e| !e.is_null()).unwrap_or(false);
            if response_error_real || outer_error_real {
                println!("[Proxy] WS 限额: 有 error 字段");
                return true;
            }

            // JSON 解析成功但没有 error 字段 → 是合法事件（response.created /
            // codex.rate_limits / response.output_text.delta 等都会嵌 codex
            // system prompt，prompt 里含有 "rate limit" "too many requests"
            // 等子串，绝对不能再走文本兜底，否则任何 in_progress 都会被误判
            // 成限额，触发无限切号循环）
            let _ = msg_type;
            return false;
        }

        // JSON 解析失败时才走文本兜底（极少见：上游发了非 JSON 文本帧）
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
            println!("[Proxy] WS 限额/容量满: 非 JSON 文本兜底匹配");
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

            // response.failed / error 类型：必须看内层 error.code / error.message
            // 是否真的是封号。盲目按 type 切号会误判（同 rate_limit 的修复）。
            if msg_type == "response.failed" || msg_type == "error" {
                let error_obj = val
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| val.get("error"));
                let inner_code = error_obj
                    .and_then(|e| e.get("code").or_else(|| e.get("type")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let inner_msg = error_obj
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();

                let hit = BANNED_KEYWORDS
                    .iter()
                    .any(|kw| inner_code.contains(kw) || inner_msg.contains(kw));
                return hit;
            }

            // error 字段里包含封号关键词（同样要排除 null，避免 in_progress 误判）
            let response_error_real = val
                .get("response")
                .and_then(|r| r.get("error"))
                .map(|e| !e.is_null())
                .unwrap_or(false);
            let outer_error_real = val.get("error").map(|e| !e.is_null()).unwrap_or(false);
            if response_error_real || outer_error_real {
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

    // 本 WS 会话是否在请求 Spark 模型（client→upstream 首帧里嗅探）。Spark 429 不该切号。
    let ws_is_spark = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ws_is_spark_w = ws_is_spark.clone();
    let ws_is_spark_r = ws_is_spark.clone();

    // 诊断：每边的帧计数，用于定位"bridge 立刻退出"（Broken pipe 重连噪声）。
    let c2u_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let u2c_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let c2u_count_b = c2u_count.clone();
    let u2c_count_b = u2c_count.clone();

    let client_to_upstream = async {
        while let Some(msg) = client_read.next().await {
            match msg {
                Ok(msg) => {
                    let cnt = c2u_count_b
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    if cnt == 1 {
                        println!(
                            "[Proxy] WS bridge: 首帧 client→upstream ({})",
                            describe_ws_msg(&msg)
                        );
                    }
                    if msg.is_close() {
                        println!("[Proxy] WS bridge: client 发送 Close → 转发 upstream");
                        let _ = upstream_write.send(msg).await;
                        break;
                    }
                    // 嗅探 client→upstream 的 JSON 文本，提取 session_key（首次命中即固定）
                    if let tungstenite::Message::Text(ref t) = msg {
                        // 标记 Spark 模型会话：它的 429 不该触发切号（见 request_is_spark_model）
                        if !ws_is_spark_w.load(std::sync::atomic::Ordering::Relaxed)
                            && request_is_spark_model(t.as_bytes())
                        {
                            ws_is_spark_w.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        if ws_session_key_w
                            .lock()
                            .map(|g| g.is_none())
                            .unwrap_or(false)
                        {
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
                    if let Err(e) = upstream_write.send(msg).await {
                        println!("[Proxy] WS bridge: upstream_write.send 失败: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    println!("[Proxy] WS bridge: client_read Err: {}", e);
                    break;
                }
            }
        }
    };

    let state_clone = state.clone();
    let ws_session_key_r = ws_session_key.clone();
    let upstream_to_client = async {
        // 跟踪本轮 response 状态，用于 mid-stream drop 时合成 response.completed
        //
        // 问题背景：codex Desktop 走 WS 看 chatgpt.com 的 Responses 流式输出。
        // 上游 Clash 节点偶尔在 stream 80-95% 处掉 TLS → bridge 退出 → client 收 Close。
        // codex 看不到 response.completed → **从头重发整个 prompt + 重新接收完整输出** →
        // 等于已经产出的内容白白烧 token。
        //
        // 这里追踪三个状态：
        //   - response_id: 从 response.created 抓到的 id
        //   - saw_completed: 已经收到正经的 response.completed
        //   - has_function_call: 看到 tool_call 类事件（这种情况不能合成 completed，
        //     codex 会按 tool_call 路径走，缺数据会卡死）
        //
        // 流结束时（None / Err）若 response_id 存在、未完成、无 tool_call、
        // 且已经流出足够内容（>=20 帧），就给 client 合成一条 response.completed。
        // 用户看到的回答会比真实截止 + 几句，但 **codex 不再重发**。
        let mut response_id: Option<String> = None;
        let mut saw_completed = false;
        let mut has_function_call = false;
        while let Some(msg) = upstream_read.next().await {
            match msg {
                Ok(msg) => {
                    let cnt = u2c_count_b
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    if cnt == 1 {
                        println!(
                            "[Proxy] WS bridge: 首帧 upstream→client ({})",
                            describe_ws_msg(&msg)
                        );
                    }
                    // 提取 response 状态：仅扫描 Text 帧关键事件
                    if let tungstenite::Message::Text(ref t) = msg {
                        if response_id.is_none() && t.contains("\"response.created\"") {
                            // 从 JSON 抽 response.id，失败也不影响主流程
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
                                if let Some(id) = v
                                    .get("response")
                                    .and_then(|r| r.get("id"))
                                    .and_then(|i| i.as_str())
                                {
                                    response_id = Some(id.to_string());
                                }
                            }
                        }
                        if t.contains("\"response.completed\"") {
                            saw_completed = true;
                        }
                        // tool/function call 进行中：不能合成 completed（codex 等 tool 输出）
                        if !has_function_call
                            && (t.contains("\"function_call\"")
                                || t.contains("\"tool_call\"")
                                || t.contains("response.function_call")
                                || t.contains("response.tool_call"))
                        {
                            has_function_call = true;
                        }
                    }
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
                                && !PER_ACCOUNT_LIMIT_KEYWORDS
                                    .iter()
                                    .any(|kw| lower.contains(kw))
                        } else {
                            false
                        };
                        if is_capacity_only {
                            println!(
                                "[Proxy] WebSocket 上游容量满（全局过载），关 WS 不切号 → 等 codex App 重连"
                            );
                        } else if ws_is_spark_r.load(std::sync::atomic::Ordering::Relaxed) {
                            // Spark 模型 429 = Pro 子限额耗尽，与基础额度无关：不切号、不标耗尽，
                            // 只关这条 Spark WS，别把同账号上正常的 gpt-5.5 会话踢走。
                            println!(
                                "[Proxy] WebSocket Spark 模型 429（Pro 子限额耗尽），不切号，仅关此 Spark WS"
                            );
                        } else {
                            println!("[Proxy] WebSocket 单号限额，静默切号 + 关 WS");
                            mark_current_quota_depleted(&state_clone);
                            if let PickResult::Found { id, .. } = pick_next_account(&state_clone) {
                                let _ =
                                    do_switch(&state_clone, &id, SwitchReason::WebSocketRateLimit);
                            }
                        }
                        // 关 client 侧 WS，让 Codex App 干净断开 + 自动重连
                        let _ = client_write.send(tungstenite::Message::Close(None)).await;
                        break;
                    }
                    // 检测封号
                    if detect_ws_banned(&msg) {
                        println!(
                            "[Proxy] WebSocket 检测到封号，静默切号 + 关 WS（不透回 codex）..."
                        );
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
                            if let Some(mut usage) =
                                crate::token_tracker::extract_usage_from_sse(wrapped.as_bytes(), "")
                            {
                                let cur_id = state_clone
                                    .store
                                    .lock()
                                    .ok()
                                    .and_then(|s| s.current.clone())
                                    .unwrap_or_default();
                                let cache_pct = if usage.input_tokens > 0 {
                                    (usage.cached_input_tokens as f64 / usage.input_tokens as f64)
                                        * 100.0
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
                                // WS 路径也一样：每次 response.completed 就记 binding，
                                // 不要求 cache 命中（详见 session_affinity::record_cache_hit）。
                                let sk_for_record = ws_session_key_r
                                    .lock()
                                    .ok()
                                    .and_then(|g| g.as_ref().cloned())
                                    .unwrap_or_default();
                                if !sk_for_record.is_empty() && !cur_id.is_empty() {
                                    state_clone.session_affinity.record_cache_hit(
                                        &sk_for_record,
                                        &cur_id,
                                        usage.cached_input_tokens,
                                    );
                                }
                                usage.account_id = cur_id;
                                usage.session_key = sk_for_record;
                                state_clone.tracker.record(usage);
                            }
                        }
                    }

                    if msg.is_close() {
                        if let tungstenite::Message::Close(ref cf) = msg {
                            println!(
                                "[Proxy] WS bridge: upstream 发送 Close: {:?}",
                                cf
                            );
                        }
                        let _ = client_write.send(msg).await;
                        break;
                    }

                    if let Err(e) = client_write.send(msg).await {
                        println!("[Proxy] WS bridge: client_write.send 失败: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    println!("[Proxy] WS bridge: upstream_read Err: {}", e);
                    break;
                }
            }
        }
        // 流结束：判断要不要给 client 合成一条 response.completed 防止 codex 全量 retry
        let frames = u2c_count_b.load(std::sync::atomic::Ordering::Relaxed);
        println!(
            "[Proxy] WS bridge: upstream loop ended — frames={} response_id={:?} saw_completed={} has_function_call={}",
            frames, response_id, saw_completed, has_function_call
        );
        if !saw_completed && !has_function_call && frames >= 20 {
            // 即使 response_id 没抓到，也合成一条（用 generated id）—— 反正 codex 看的是
            // type=response.completed 就当一轮 done，不会拒绝
            let rid = response_id
                .clone()
                .unwrap_or_else(|| format!("resp_synthetic_{}", chrono::Utc::now().timestamp_millis()));
            let synthetic = serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": rid,
                    "object": "response",
                    "status": "completed",
                    "incomplete_details": {
                        "reason": "upstream_stream_dropped_proxy_synthesized"
                    }
                },
            });
            let text = synthetic.to_string();
            println!(
                "[Proxy] WS bridge: upstream 中途断（u→c={} 帧 rid={}），\
                 合成 response.completed 防止 codex 全量 retry 烧 token",
                frames, rid
            );
            let _ = client_write
                .send(tungstenite::Message::Text(text.into()))
                .await;
            // 主动发 Close 让 codex 干净收尾（不会 reconnect）
            let _ = client_write.send(tungstenite::Message::Close(None)).await;
        } else if !saw_completed {
            println!(
                "[Proxy] WS bridge: upstream 中途断（u→c={} 帧 has_fn_call={} → 不合成，codex 会 retry）",
                frames, has_function_call
            );
        }
    };

    tokio::select! {
        _ = client_to_upstream => {
            println!(
                "[Proxy] WS bridge 退出: client_to_upstream（c→u={} u→c={}）",
                c2u_count.load(std::sync::atomic::Ordering::Relaxed),
                u2c_count.load(std::sync::atomic::Ordering::Relaxed)
            );
        },
        _ = upstream_to_client => {
            println!(
                "[Proxy] WS bridge 退出: upstream_to_client（c→u={} u→c={}）",
                c2u_count.load(std::sync::atomic::Ordering::Relaxed),
                u2c_count.load(std::sync::atomic::Ordering::Relaxed)
            );
        },
        _ = disconnect.notified() => {
            println!(
                "[Proxy] WS bridge 退出: disconnect notified（c→u={} u→c={}）",
                c2u_count.load(std::sync::atomic::Ordering::Relaxed),
                u2c_count.load(std::sync::atomic::Ordering::Relaxed)
            );
        },
    }
}

/// 把 WS Message 简单描述成一行：诊断 log 不要打全文（response.created 单帧 >20KB）。
fn describe_ws_msg(msg: &tungstenite::Message) -> String {
    match msg {
        tungstenite::Message::Text(t) => {
            let bytes = t.as_bytes();
            let preview_end = bytes.len().min(80);
            let preview = String::from_utf8_lossy(&bytes[..preview_end]);
            format!("Text {}B preview={:?}", bytes.len(), preview)
        }
        tungstenite::Message::Binary(b) => format!("Binary {}B", b.len()),
        tungstenite::Message::Ping(b) => format!("Ping {}B", b.len()),
        tungstenite::Message::Pong(b) => format!("Pong {}B", b.len()),
        tungstenite::Message::Close(cf) => format!("Close {:?}", cf),
        tungstenite::Message::Frame(_) => "Frame(raw)".to_string(),
    }
}

// ────────────────────────────────────────────────────────────────
// chat_completions Relay 翻译路径
// ────────────────────────────────────────────────────────────────

/// 取 body 里的 `model` 字段（已经经过 model_map / fallback 重写）。
fn extract_model_from_body(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| "glm-5.1".to_string())
}

/// 把 base_url（如 `https://open.bigmodel.cn/api/coding/paas/v4`）拼成
/// `<base>/chat/completions`。返回 (URL, host)。
fn build_chat_completions_url(base_url: &str) -> Option<(String, String)> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let host = url::Url::parse(trimmed)
        .ok()
        .and_then(|u| u.host_str().map(String::from))?;
    Some((format!("{}/chat/completions", trimmed), host))
}

/// 把厂商自家 chat_completions 错误体翻译成 codex 能识别的标准 OpenAI 错误。
///
/// codex CLI 收到 `/v1/responses` 4xx 时按 OpenAI 错误格式 `{error:{code,message,type}}`
/// 解析；命中 `context_length_exceeded` 会触发自动 compact、命中 `usage_limit_reached`
/// 会显示限额提示。各家厂商的 400 错误体格式不一（GLM 1261 / MiMo 自己的 code / DeepSeek
/// 又是另一套），不归一化的话 codex 解析失败 → "正在思考一下然后退出"。
///
/// 命中规则按 message 关键字（按厂商收集到的实际错误文案）；都不命中 → 原样透回。
/// 如果传入 `provider`（chat_completions 路径都能 detect_provider 拿到），会进一步从
/// `provider_quirks::enhance_error_hint` 取一条用户向操作提示，追加到 message 末尾。
fn normalize_chat_completions_error(
    status: reqwest::StatusCode,
    body: &[u8],
    provider: crate::provider_quirks::RelayProvider,
) -> (reqwest::StatusCode, Bytes) {
    // 解析失败 / 上游本来就没返回 JSON → 透传
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (status, Bytes::copy_from_slice(body)),
    };
    let msg = v
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let code = v
        .pointer("/error/code")
        .and_then(|v| {
            v.as_str().map(String::from).or_else(|| {
                v.as_i64().map(|n| n.to_string())
            })
        })
        .unwrap_or_default();
    let hint = crate::provider_quirks::enhance_error_hint(provider, &msg);

    // 关键字探测（按已知厂商错误文案积累）
    let is_context_overflow = msg.contains("prompt exceeds max length")    // GLM 1261
        || msg.contains("maximum context length")                          // OpenAI/DeepSeek
        || msg.contains("context length")
        || msg.contains("token limit")
        || msg.contains("too many tokens")
        || msg.contains("input too long")
        || msg.contains("超出")
        || msg.contains("超过")
        || code == "1261";

    let is_rate_limit = msg.contains("rate limit")
        || msg.contains("usage limit")
        || msg.contains("quota")
        || msg.contains("rate_limit");

    let with_hint = |base: &str| -> String {
        match hint {
            Some(h) => format!("{} {}", base, h),
            None => base.to_string(),
        }
    };
    let original_msg = v
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let normalized: Option<serde_json::Value> = if is_context_overflow {
        Some(serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
                "message": with_hint(&format!("Upstream rejected request: prompt is too long for this model. (Original: {})",
                    if original_msg.is_empty() { "context length exceeded".to_string() } else { original_msg.clone() })),
                "param": null,
            }
        }))
    } else if is_rate_limit {
        Some(serde_json::json!({
            "error": {
                "type": "rate_limit_error",
                "code": "usage_limit_reached",
                "message": with_hint(if original_msg.is_empty() { "Rate limit reached" } else { &original_msg }),
                "param": null,
            }
        }))
    } else if hint.is_some() {
        // 没命中 context_overflow / rate_limit，但 provider hint 命中（比如 MiMo
        // webSearchEnabled 这种）：保留原 error type，把 hint 追加到 message。
        let original_obj = v
            .get("error")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let mut error_obj = original_obj.as_object().cloned().unwrap_or_default();
        error_obj.insert("message".to_string(), serde_json::Value::String(with_hint(&original_msg)));
        Some(serde_json::json!({"error": error_obj}))
    } else {
        None
    };

    match normalized {
        Some(json) => {
            let new_status = if is_rate_limit && status.as_u16() != 429 {
                reqwest::StatusCode::TOO_MANY_REQUESTS
            } else if is_context_overflow {
                reqwest::StatusCode::BAD_REQUEST
            } else {
                status
            };
            let bytes = serde_json::to_vec(&json).unwrap_or_default();
            let reason = if is_context_overflow {
                "context_length_exceeded"
            } else if is_rate_limit {
                "usage_limit_reached"
            } else {
                "provider_hint_added"
            };
            eprintln!(
                "[Proxy] 归一化上游错误 {} → {} ({}{})",
                status.as_u16(),
                new_status.as_u16(),
                reason,
                if hint.is_some() { ", with provider hint" } else { "" }
            );
            (new_status, Bytes::from(bytes))
        }
        None => (status, Bytes::copy_from_slice(body)),
    }
}

async fn handle_chat_completions_relay_websocket(
    state: Arc<ProxyState>,
    relay: RelayRoute,
    mut req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    let ws_key = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let accept_key = tungstenite::handshake::derive_accept_key(ws_key.as_bytes());
    let on_upgrade = hyper::upgrade::on(&mut req);
    let req_headers = req.headers().clone();

    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key)
        .body(full_body(Bytes::new()))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "101 构建失败"));

    let disconnect = state.ws_disconnect.clone();
    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[Proxy] chat relay WS upgrade 失败: {}", e);
                return;
            }
        };
        let io = TokioIo::new(upgraded);
        let mut client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        println!("[Proxy] chat_completions Relay WS 适配器已连接");
    { let _ = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/codex-relay-debug.log")
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"[ENTRY] WS adapter connected\n")); }

        loop {
            // 同时监听：client 下一个消息 / 切号或路由变更触发的 ws_disconnect
            let next_msg = tokio::select! {
                m = client_ws.next() => m,
                _ = disconnect.notified() => {
                    println!("[Proxy] chat_completions Relay WS 收到 ws_disconnect，主动关闭让 codex 重连");
                    let _ = client_ws
                        .send(tungstenite::Message::Close(None))
                        .await;
                    break;
                }
            };
            let Some(msg) = next_msg else { break; };
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[Proxy] chat relay WS 读取失败: {}", e);
                    break;
                }
            };
            // DEBUG: print every message type received
            let kind = match &msg {
                tungstenite::Message::Text(t) => format!("Text({}B)", t.len()),
                tungstenite::Message::Binary(b) => format!("Binary({}B)", b.len()),
                tungstenite::Message::Ping(_) => "Ping".to_string(),
                tungstenite::Message::Pong(_) => "Pong".to_string(),
                tungstenite::Message::Close(c) => format!("Close({:?})", c),
                tungstenite::Message::Frame(_) => "Frame".to_string(),
            };
            println!("[Proxy] chat relay WS 收到 client msg: {}", kind);
            match msg {
                tungstenite::Message::Text(text) => {
                    println!("[Proxy] chat relay WS Text preview: {}", &text[..text.len().min(180)]);
                    if let Err(e) = handle_chat_relay_ws_text(
                        &state,
                        &relay,
                        &req_headers,
                        text.as_str(),
                        &mut client_ws,
                    )
                    .await
                    {
                        eprintln!("[Proxy] chat relay WS handler 返错: {}", e);
                        let err_evt = serde_json::json!({
                            "type": "error",
                            "error": {
                                "type": "proxy_error",
                                "code": "chat_relay_ws_error",
                                "message": e,
                            }
                        });
                        let _ = client_ws
                            .send(tungstenite::Message::Text(err_evt.to_string().into()))
                            .await;
                    }
                }
                tungstenite::Message::Ping(p) => {
                    let _ = client_ws.send(tungstenite::Message::Pong(p)).await;
                }
                tungstenite::Message::Close(c) => {
                    let _ = client_ws.send(tungstenite::Message::Close(c)).await;
                    break;
                }
                _ => {}
            }
        }
        println!("[Proxy] chat_completions Relay WS 适配器已关闭");
    });

    Ok(response)
}

async fn handle_chat_relay_ws_text<S>(
    state: &Arc<ProxyState>,
    relay: &RelayRoute,
    req_headers: &hyper::HeaderMap,
    text: &str,
    client_ws: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let event: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid websocket json: {}", e))?;
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type != "response.create" {
        println!("[Proxy] chat relay WS 忽略客户端事件 type={}", event_type);
        return Ok(());
    }

    // codex 0.x WS 协议有两种 response.create 形态：
    //   旧（≤0.129）：{ type: "response.create", response: { model, input, ... } }
    //   新（≥0.130 Desktop）：{ type: "response.create", model, input, instructions, ... } 平铺
    // 兼容两种：优先 nested `response`，否则把整个 event 去掉 type 字段当 body。
    let mut response_body = if let Some(nested) = event.get("response").cloned() {
        nested
    } else {
        // 平铺：clone 整个 event，去掉 type 字段
        let mut v = event.clone();
        if let Some(obj) = v.as_object_mut() {
            obj.remove("type");
        }
        v
    };
    if let Some(obj) = response_body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let body_bytes = Bytes::from(
        serde_json::to_vec(&response_body).map_err(|e| format!("serialize response: {}", e))?,
    );
    let body_for_translate = rewrite_model_in_body(
        &body_bytes,
        relay.model_map.as_ref(),
        relay.model_fallback.as_deref(),
    );
    let model = extract_model_from_body(&body_for_translate);
    let (mut chat_body, mut translator_state) =
        crate::relay_translate::translate_request(&body_for_translate, &model)
            .map_err(|e| format!("translator 请求处理失败: {}", e))?;

    eprintln!("[Proxy] chat relay WS 翻译后 body 前200字符: {}", String::from_utf8_lossy(&chat_body[..chat_body.len().min(200)]));

    let base = relay
        .base_url
        .as_deref()
        .ok_or_else(|| "Relay base_url 未配置".to_string())?;
    crate::provider_quirks::preprocess_chat_body(
        crate::provider_quirks::detect_provider(Some(base)),
        &mut chat_body,
    );
    let (upstream_url, upstream_host) =
        build_chat_completions_url(base).ok_or_else(|| "Relay base_url 解析失败".to_string())?;

    let mut headers = build_chat_relay_upstream_headers(&upstream_host);
    let api_key = relay.api_key.clone().unwrap_or_default();
    if !api_key.is_empty() {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key)) {
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.remove(reqwest::header::CONTENT_LENGTH);
    headers.remove(reqwest::header::CONTENT_ENCODING);

    println!(
        "[Proxy] → relay translate (chat_completions WS): {} (model={})",
        upstream_url, translator_state.model
    );
    // DEBUG: dump request to log file
    {
        let debug_line = format!("[WS] url={} model={} body_len={} webSearchEnabled_in_body={}\n",
            upstream_url, translator_state.model, chat_body.len(),
            String::from_utf8_lossy(&chat_body).contains("webSearchEnabled"));
        let _ = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/codex-relay-debug.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f, debug_line.as_bytes()));
        // Dump first 500 chars of body
        let preview = String::from_utf8_lossy(&chat_body[..chat_body.len().min(500)]);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/codex-relay-debug.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("  body_preview: {}\n\n", preview).as_bytes()));
    }
    let upstream_resp = state
        .client
        .post(&upstream_url)
        .headers(headers)
        .body(chat_body)
        .send()
        .await
        .map_err(|e| format!("relay 上游连接失败: {}", e))?;

    let status = upstream_resp.status();
    if status != reqwest::StatusCode::OK {
        let bytes = upstream_resp.bytes().await.unwrap_or_default();
        let preview: String = String::from_utf8_lossy(&bytes).chars().take(512).collect();
        return Err(format!("relay 上游 {}: {}", status.as_u16(), preview));
    }

    send_sse_events_as_ws_json(
        client_ws,
        crate::relay_translate::emit_created(&translator_state),
    )
    .await?;

    if !is_sse_response(&upstream_resp) {
        let bytes = upstream_resp.bytes().await.unwrap_or_default();
        let out = crate::relay_translate::translate_sync_response(&translator_state, &bytes)
            .map_err(|e| format!("sync 响应翻译失败: {}", e))?;
        let response_obj: serde_json::Value =
            serde_json::from_slice(&out).map_err(|e| format!("sync json parse: {}", e))?;
        let completed = serde_json::json!({
            "type": "response.completed",
            "response": response_obj,
        });
        client_ws
            .send(tungstenite::Message::Text(completed.to_string().into()))
            .await
            .map_err(|e| format!("websocket send failed: {}", e))?;
        return Ok(());
    }

    let mut upstream_stream = upstream_resp.bytes_stream();
    let mut buf = crate::relay_translate::ChatSseBuffer::new();
    let mut saw_done = false;
    while !saw_done {
        match upstream_stream.next().await {
            Some(Ok(chunk)) => {
                buf.push(&chunk);
                for e in buf.drain_events() {
                    match e {
                        crate::relay_translate::ChatSseEvent::Done => {
                            saw_done = true;
                            break;
                        }
                        crate::relay_translate::ChatSseEvent::Data(payload) => {
                            for translated in
                                crate::relay_translate::handle_chunk(&mut translator_state, &payload)
                            {
                                send_sse_events_as_ws_json(client_ws, translated).await?;
                            }
                        }
                    }
                }
            }
            Some(Err(e)) => return Err(format!("relay SSE 上游错误: {}", e)),
            None => break,
        }
    }
    send_sse_events_as_ws_json(
        client_ws,
        crate::relay_translate::emit_completed(&mut translator_state),
    )
    .await?;
    Ok(())
}

async fn send_sse_events_as_ws_json<S>(
    client_ws: &mut tokio_tungstenite::WebSocketStream<S>,
    bytes: Vec<u8>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let text = String::from_utf8_lossy(&bytes);
    for block in text.split("\n\n") {
        for line in block.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                client_ws
                    .send(tungstenite::Message::Text(data.to_string().into()))
                    .await
                    .map_err(|e| format!("websocket send failed: {}", e))?;
            }
        }
    }
    Ok(())
}

async fn handle_chat_completions_relay(
    state: Arc<ProxyState>,
    relay: RelayRoute,
    method: hyper::Method,
    path_and_query: String,
    req_headers: hyper::HeaderMap,
    body_bytes: Bytes,
) -> Response<ProxyBody> {
    let path_lc = path_and_query.split('?').next().unwrap_or("").to_string();

    // GET /v1/models → 本地合成最小响应，不打上游
    if method == hyper::Method::GET && (path_lc == "/v1/models" || path_lc.ends_with("/models")) {
        let default_model = relay
            .model_fallback
            .clone()
            .unwrap_or_else(|| "glm-5.1".to_string());
        let body = crate::relay_translate::synthetic_models_response(&default_model);
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(full_body(Bytes::from(body)))
            .unwrap_or_else(|_| {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "models 响应构建失败")
            });
    }

    // 仅翻译 /v1/responses；其它 path 透传到 base_url + path（保留历史行为）。
    let is_responses_call = path_lc == "/v1/responses" || path_lc.ends_with("/responses");
    let base = match relay.base_url.as_deref() {
        Some(b) if !b.is_empty() => b,
        _ => return error_response(StatusCode::SERVICE_UNAVAILABLE, "Relay base_url 未配置"),
    };

    if !is_responses_call {
        // 透传：直接打 base + path（不翻译）
        let trimmed = base.trim_end_matches('/');
        let upstream_url = format!("{}{}", trimmed, path_and_query);
        let host = match url::Url::parse(trimmed)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
        {
            Some(h) => h,
            None => return error_response(StatusCode::BAD_GATEWAY, "Relay base_url 无法解析 host"),
        };
        let base_headers = build_upstream_headers(&req_headers, &host);
        let token = relay.api_key.clone().unwrap_or_default();
        match forward_with_token(
            &state,
            &method,
            &upstream_url,
            &base_headers,
            &body_bytes,
            &token,
        )
        .await
        {
            Ok(resp) => return build_stream_response(resp, Some(state.tracker.clone()), None),
            Err(e) => {
                return error_response(StatusCode::BAD_GATEWAY, &format!("上游连接失败: {}", e))
            }
        }
    }

    // DEBUG: log HTTP relay entry
    { let _ = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/codex-relay-debug.log")
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("[HTTP-ENTRY] path={} body_len={}\n", path_and_query, body_bytes.len()).as_bytes())); }
    // 翻译请求体
    if body_bytes.is_empty() {
        eprintln!(
            "[Proxy] relay translate 收到空 body | method={} path={} headers.content_type={:?} content_length={:?}",
            method,
            path_and_query,
            req_headers.get(hyper::header::CONTENT_TYPE),
            req_headers.get(hyper::header::CONTENT_LENGTH),
        );
        return error_response(
            StatusCode::BAD_REQUEST,
            "translator: empty request body on /v1/responses (expected codex Responses payload)",
        );
    }
    // codex CLI 0.130+ 默认 zstd 压缩 request body（也可能 gzip）。先按 Content-Encoding 解压。
    let content_encoding = req_headers
        .get(hyper::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase());
    let body_for_translate: Bytes = match content_encoding.as_deref() {
        Some("zstd") | Some("x-zstd") => match zstd::decode_all(body_bytes.as_ref()) {
            Ok(out) => Bytes::from(out),
            Err(e) => {
                eprintln!("[Proxy] relay translate zstd 解压失败: {} body_len={}", e, body_bytes.len());
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("translator: zstd decompress failed: {}", e),
                );
            }
        },
        Some("gzip") | Some("x-gzip") => {
            use std::io::Read;
            let mut decoder = flate2::read::GzDecoder::new(body_bytes.as_ref());
            let mut out = Vec::with_capacity(body_bytes.len() * 4);
            match decoder.read_to_end(&mut out) {
                Ok(_) => Bytes::from(out),
                Err(e) => {
                    eprintln!("[Proxy] relay translate gzip 解压失败: {} body_len={}", e, body_bytes.len());
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        &format!("translator: gzip decompress failed: {}", e),
                    );
                }
            }
        }
        Some("deflate") => {
            use std::io::Read;
            let mut decoder = flate2::read::DeflateDecoder::new(body_bytes.as_ref());
            let mut out = Vec::with_capacity(body_bytes.len() * 4);
            match decoder.read_to_end(&mut out) {
                Ok(_) => Bytes::from(out),
                Err(e) => {
                    eprintln!("[Proxy] relay translate deflate 解压失败: {}", e);
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        &format!("translator: deflate decompress failed: {}", e),
                    );
                }
            }
        }
        _ => body_bytes.clone(),
    };
    // codex CLI 0.130+ 把 body 用 zstd 压缩 → 顶层 rewrite_model_in_body 那次没改成
    // （它在解压之前跑，JSON 解析失败原样返回）。这里解压后必须再 rewrite 一次，
    // 否则上游会收到 codex 的 gpt-5.5 / gpt-5-codex 报"模型不存在"。
    let body_for_translate = rewrite_model_in_body(
        &body_for_translate,
        relay.model_map.as_ref(),
        relay.model_fallback.as_deref(),
    );
    let model = extract_model_from_body(&body_for_translate);
    let (mut chat_body, mut translator_state) =
        match crate::relay_translate::translate_request(&body_for_translate, &model) {
            Ok(x) => x,
            Err(e) => {
                let head_len = body_for_translate.len().min(160);
                let head_str = String::from_utf8_lossy(&body_for_translate[..head_len]);
                eprintln!(
                    "[Proxy] relay translate 请求失败: {} | method={} path={} encoding={:?} body_len={} body_head={:?}",
                    e,
                    method,
                    path_and_query,
                    content_encoding,
                    body_for_translate.len(),
                    head_str,
                );
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("translator 请求处理失败: {}", e),
                );
            }
        };

    let (upstream_url, upstream_host) = match build_chat_completions_url(base) {
        Some(x) => x,
        None => return error_response(StatusCode::BAD_GATEWAY, "Relay base_url 解析失败"),
    };
    crate::provider_quirks::preprocess_chat_body(
        crate::provider_quirks::detect_provider(Some(base)),
        &mut chat_body,
    );

    // chat_completions 专用：tight whitelist，不带 codex 私有 header（GLM 等 WAF
    // 看到 codex 私有 header 会 405），强制注入 Relay api_key
    let mut headers = build_chat_relay_upstream_headers(&upstream_host);
    let api_key = relay.api_key.clone().unwrap_or_default();
    if !api_key.is_empty() {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key)) {
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    // chat/completions 上游永远要 application/json
    if let Ok(ct) = reqwest::header::HeaderValue::from_str("application/json") {
        headers.insert(reqwest::header::CONTENT_TYPE, ct);
    }
    headers.remove(reqwest::header::CONTENT_LENGTH);
    // 我们已经把客户端的 zstd/gzip 请求体解压并重新编码成 plain JSON，
    // 必须把原 Content-Encoding 头去掉，否则上游会按 zstd 再尝试解码失败。
    headers.remove(reqwest::header::CONTENT_ENCODING);

    let body_bytes_chat = Bytes::from(chat_body);
    println!(
        "[Proxy] → relay translate (chat_completions): {} {} (model={}, stream={})",
        method, upstream_url, translator_state.model, translator_state.stream_requested
    );
    // DEBUG: dump HTTP request to log file
    {
        let debug_line = format!("[HTTP] url={} model={} body_len={} webSearchEnabled_in_body={}\n",
            upstream_url, translator_state.model, body_bytes_chat.len(),
            String::from_utf8_lossy(&body_bytes_chat).contains("webSearchEnabled"));
        let _ = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/codex-relay-debug.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f, debug_line.as_bytes()));
        let preview = String::from_utf8_lossy(&body_bytes_chat[..body_bytes_chat.len().min(500)]);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/codex-relay-debug.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("  body_preview: {}\n\n", preview).as_bytes()));
    }

    let upstream_resp = match state
        .client
        .request(
            reqwest::Method::from_bytes(method.as_str().as_bytes())
                .unwrap_or(reqwest::Method::POST),
            &upstream_url,
        )
        .headers(headers)
        .body(body_bytes_chat.to_vec())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("relay 上游连接失败: {}", e),
            )
        }
    };

    let status = upstream_resp.status();
    if status != reqwest::StatusCode::OK {
        let bytes = upstream_resp.bytes().await.unwrap_or_default();
        let preview: String = String::from_utf8_lossy(&bytes).chars().take(512).collect();
        eprintln!(
            "[Proxy] relay 上游 {} {} body: {}",
            status.as_u16(),
            upstream_url,
            preview
        );
        // 归一化：把厂商自家的 400 错误体翻译成 codex 能识别的 OpenAI 标准格式。
        // 否则 codex 解析失败会"思考一下然后退出"（GLM 1261 / DeepSeek too_long_input
        // / MiMo 自有错误码 都不是 OpenAI 格式 → codex 不认）。
        let provider = crate::provider_quirks::detect_provider(relay.base_url.as_deref());
        let (status_out, body_out) = normalize_chat_completions_error(status, &bytes, provider);
        return Response::builder()
            .status(status_out.as_u16())
            .header("content-type", "application/json")
            .body(full_body(body_out))
            .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "上游响应构建失败"));
    }

    let is_sse = is_sse_response(&upstream_resp);
    if !is_sse {
        // sync /chat/completions 响应 → 翻译成 Responses-shape JSON
        let bytes = upstream_resp.bytes().await.unwrap_or_default();
        match crate::relay_translate::translate_sync_response(&translator_state, &bytes) {
            Ok(out) => {
                return Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(full_body(Bytes::from(out)))
                    .unwrap_or_else(|_| {
                        error_response(StatusCode::INTERNAL_SERVER_ERROR, "sync 响应构建失败")
                    });
            }
            Err(e) => {
                eprintln!("[Proxy] relay sync 翻译失败: {}", e);
                // 翻译失败 → 透传原 chat 响应（codex 端可能不认识，但起码有信号）
                return Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(full_body(bytes))
                    .unwrap_or_else(|_| {
                        error_response(StatusCode::INTERNAL_SERVER_ERROR, "sync 透传失败")
                    });
            }
        }
    }

    // SSE → 启 task 边读边翻译，channel 推到下游
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    let mut upstream_stream = upstream_resp.bytes_stream();

    // 先发 response.created
    let _ = tx
        .send(Bytes::from(crate::relay_translate::emit_created(
            &translator_state,
        )))
        .await;

    tokio::spawn(async move {
        let mut buf = crate::relay_translate::ChatSseBuffer::new();
        let mut saw_done = false;
        while !saw_done {
            let next = upstream_stream.next().await;
            match next {
                Some(Ok(chunk)) => {
                    buf.push(&chunk);
                    let evts = buf.drain_events();
                    for e in evts {
                        match e {
                            crate::relay_translate::ChatSseEvent::Done => {
                                saw_done = true;
                                break;
                            }
                            crate::relay_translate::ChatSseEvent::Data(payload) => {
                                let translated = crate::relay_translate::handle_chunk(
                                    &mut translator_state,
                                    &payload,
                                );
                                for tev in translated {
                                    if tx.send(Bytes::from(tev)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    eprintln!("[Proxy] relay SSE 上游错误: {}", e);
                    break;
                }
                None => break,
            }
        }
        let done = crate::relay_translate::emit_completed(&mut translator_state);
        let _ = tx.send(Bytes::from(done)).await;
    });

    let body_stream: ByteStream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (Ok::<Bytes, reqwest::Error>(item), rx))
    })
    .boxed();

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(
        reqwest::header::CACHE_CONTROL,
        reqwest::header::HeaderValue::from_static("no-cache"),
    );

    build_stream_response_from_parts(
        reqwest::StatusCode::OK,
        headers,
        Bytes::new(),
        body_stream,
        Some(state.tracker.clone()),
        None,
    )
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
        let (url, host) = get_upstream(false, Some("https://unity2.ai"), "/v1/chat/completions");
        assert_eq!(url, "https://unity2.ai/v1/chat/completions");
        assert_eq!(host, "unity2.ai");
    }

    #[test]
    fn upstream_relay_strips_trailing_slash_on_base_url() {
        let (url, host) = get_upstream(false, Some("https://unity2.ai/"), "/v1/responses");
        assert_eq!(url, "https://unity2.ai/v1/responses");
        assert_eq!(host, "unity2.ai");
    }

    #[test]
    fn upstream_relay_with_port_extracts_host_only() {
        let (url, host) =
            get_upstream(false, Some("http://127.0.0.1:9080"), "/v1/chat/completions");
        assert_eq!(url, "http://127.0.0.1:9080/v1/chat/completions");
        assert_eq!(host, "127.0.0.1");
    }

    #[test]
    fn upstream_chatgpt_takes_precedence_over_relay_url() {
        // 防呆：is_chatgpt=true 时 relay_base_url 应该被忽略（理论上不会同时设置，但要稳）
        let (url, host) = get_upstream(true, Some("https://unity2.ai"), "/v1/responses");
        assert_eq!(url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(host, "chatgpt.com");
    }
}
