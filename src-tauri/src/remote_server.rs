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

fn antigravity_refresh_locks(
) -> &'static tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<
        tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    > = OnceLock::new();
    LOCKS.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()))
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

    if path == "/antigravity/oauth/complete" && method == Method::POST {
        return handle_antigravity_oauth_complete(&state, req).await;
    }

    if path == "/antigravity/accounts" && method == Method::GET {
        let store = match state.store.lock() {
            Ok(store) => store,
            Err(error) => return err_resp(error.to_string()),
        };
        let accounts: Vec<Account> = store
            .accounts
            .values()
            .filter(|account| account.is_antigravity_oauth())
            .cloned()
            .map(antigravity_account_mirror)
            .collect();
        return json_resp(StatusCode::OK, json!({"accounts": accounts}));
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
                (Method::GET, Some("antigravity-token")) => {
                    return handle_antigravity_token(&state, id, false).await;
                }
                (Method::POST, Some("antigravity-token")) => {
                    return handle_antigravity_token(&state, id, true).await;
                }
                (Method::POST, Some("antigravity-quota")) => {
                    return match refresh_antigravity_quota_local(
                        &state.store,
                        &state.app_handle,
                        id,
                    )
                    .await
                    {
                        Ok(quotas) => json_resp(StatusCode::OK, json!({"model_quotas": quotas})),
                        Err(error) => json_resp(StatusCode::BAD_GATEWAY, json!({"error": error})),
                    };
                }
                (Method::POST, Some("refresh")) => {
                    return handle_refresh_account(&state, id).await;
                }
                (Method::POST, Some("refresh-token")) => {
                    return handle_refresh_token(&state, id).await;
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
                "antigravity_model_quotas": a.auth_json.get("model_quotas").cloned(),
                "window_priming": a.window_priming,
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

/// Client 的 Gemini 推理本机直出，但 Google RT/ST 仍由 Mini Server 唯一管理。
/// 该端点在 per-account 锁内按需刷新，只返回短期 access token，永不返回 RT。
async fn handle_antigravity_token(
    state: &ApiState,
    id: &str,
    force_refresh: bool,
) -> Response<ResponseBody> {
    antigravity_token_for_store(&state.store, id, force_refresh).await
}

async fn antigravity_token_for_store(
    store_handle: &Arc<Mutex<AccountStore>>,
    id: &str,
    force_refresh: bool,
) -> Response<ResponseBody> {
    let account_lock = {
        let mut locks = antigravity_refresh_locks().lock().await;
        locks
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = account_lock.lock().await;

    let (mut access_token, mut refresh_token, mut expires_at, project_id) = {
        let store = match store_handle.lock() {
            Ok(store) => store,
            Err(error) => return err_resp(error.to_string()),
        };
        let Some(account) = store.accounts.get(id) else {
            return json_resp(StatusCode::NOT_FOUND, json!({"error":"account not found"}));
        };
        if !account.is_antigravity_oauth() {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error":"account is not an Antigravity provider"}),
            );
        }
        let tokens = account.auth_json.get("tokens").unwrap_or(&Value::Null);
        (
            tokens
                .get("access_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tokens
                .get("refresh_token")
                .and_then(Value::as_str)
                .or(account.refresh_token.as_deref())
                .unwrap_or_default()
                .to_string(),
            tokens
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|value| value.with_timezone(&chrono::Utc)),
            account
                .auth_json
                .get("project_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    };

    if project_id.is_empty() || refresh_token.is_empty() {
        return json_resp(
            StatusCode::UNAUTHORIZED,
            json!({"error":"Google credential is incomplete"}),
        );
    }
    let refresh_needed = force_refresh
        || access_token.is_empty()
        || expires_at
            .map(|expires| expires <= chrono::Utc::now() + chrono::Duration::minutes(5))
            .unwrap_or(true);
    if refresh_needed {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(45))
            .build()
        {
            Ok(client) => client,
            Err(error) => return err_resp(error.to_string()),
        };
        let config = crate::antigravity::oauth::OAuthClientConfig::from_environment();
        let refreshed =
            match crate::antigravity::oauth::refresh_access_token(&client, &config, &refresh_token)
                .await
            {
                Ok(tokens) => tokens,
                Err(error) => {
                    return json_resp(StatusCode::UNAUTHORIZED, json!({"error":error}));
                }
            };
        access_token = refreshed.access_token;
        if let Some(rotated) = refreshed.refresh_token {
            refresh_token = rotated;
        }
        expires_at = Some(chrono::Utc::now() + chrono::Duration::seconds(refreshed.expires_in));
        let mut store = match store_handle.lock() {
            Ok(store) => store,
            Err(error) => return err_resp(error.to_string()),
        };
        let Some(account) = store.accounts.get_mut(id) else {
            return json_resp(StatusCode::NOT_FOUND, json!({"error":"account not found"}));
        };
        account.refresh_token = Some(refresh_token.clone());
        if let Some(object) = account.auth_json.as_object_mut() {
            let tokens = object.entry("tokens").or_insert_with(|| json!({}));
            if let Some(tokens) = tokens.as_object_mut() {
                tokens.insert(
                    "access_token".to_string(),
                    Value::String(access_token.clone()),
                );
                tokens.insert("refresh_token".to_string(), Value::String(refresh_token));
                if let Some(expires_at) = expires_at {
                    tokens.insert(
                        "expires_at".to_string(),
                        Value::String(expires_at.to_rfc3339()),
                    );
                }
            }
            object.insert(
                "last_refresh".to_string(),
                Value::String(chrono::Utc::now().to_rfc3339()),
            );
        }
        if let Err(error) = store.save() {
            return err_resp(error);
        }
    }

    json_resp(
        StatusCode::OK,
        json!({
            "account_id": id,
            "access_token": access_token,
            "project_id": project_id,
            "expires_at": expires_at.map(|value| value.to_rfc3339()),
        }),
    )
}

/// Reuse the serialized ST refresh path; only quota data leaves this operation.
/// Called in-process by the native UI, or by the authenticated Server quota endpoint.
pub(crate) async fn refresh_antigravity_quota_local(
    store: &Arc<Mutex<AccountStore>>,
    app: &tauri::AppHandle,
    id: &str,
) -> Result<std::collections::HashMap<String, crate::antigravity::quota::ModelQuota>, String> {
    let lease_response = antigravity_token_for_store(store, id, false).await;
    let status = lease_response.status();
    let bytes = lease_response
        .into_body()
        .collect()
        .await
        .map_err(|error| error.to_string())?
        .to_bytes();
    let lease: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(lease
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Google ST 获取失败")
            .to_string());
    }
    let token = lease
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("Google ST 缺失")?;
    let project = lease
        .get("project_id")
        .and_then(Value::as_str)
        .ok_or("Google project_id 缺失")?;
    let client = crate::antigravity::native::build_http_client()?;
    let quotas = crate::antigravity::quota::fetch_model_quotas(&client, token, project).await?;
    {
        let mut guard = store.lock().map_err(|error| error.to_string())?;
        let account = guard.accounts.get_mut(id).ok_or("账号已被删除")?;
        crate::antigravity::quota::write_model_quotas(&mut account.auth_json, &quotas);
        // Google has its own refresh loop; never enroll it in OpenAI keepalive.
        account.keepalive.inactive_refresh_enabled = false;
        if account
            .keepalive
            .last_error
            .as_deref()
            .is_some_and(|error| {
                error.contains("Could not validate your token") && error.contains("token_expired")
            })
        {
            // Only clear the proven cross-provider misroute after Google succeeds.
            account.keepalive.last_error = None;
            account.keepalive.last_success_at = Some(chrono::Utc::now());
        }
        guard.save()?;
    }
    let _ = app.emit("accounts-updated", ());
    Ok(quotas)
}

/// 强制刷新某账号的 access_token，返回刷新后的 auth_json（与 /token 同形）。
///
/// 走 `refresh_access_token_locked_fresh`（per-account rt 锁 + 锁内重读最新 rt + 写回
/// store）——**单刷新者路径,与 Server 自身的 keepalive/anchor/proxy 刷新串行,不新增
/// reused 源**。供 client 手机锚保活调用:client 拉到锚 token 发现快过期时打这个端点,
/// 让 Server 就地把锚账号的 token 刷新,再由 client 拉回本机写盘。
///
/// reused 错误 = 瞬时并发(此刻 store 已被赢家刷成最新)——不当失败,直接回读 store 里
/// 当前(大概率已新鲜)的 auth_json 返回。
async fn handle_refresh_token(state: &ApiState, id: &str) -> Response<ResponseBody> {
    let is_openai_account = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.accounts.get(id).map(|a| a.is_openai_account()))
        .unwrap_or(false);
    if !is_openai_account {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({"error": "provider account does not use OpenAI refresh_token"}),
        );
    }

    let refresh_res = crate::oauth::refresh_access_token_locked_fresh(&state.store, id).await;
    if let Err(ref e) = refresh_res {
        // reused 之外的真失败才报错;reused 时 store 已是最新,继续回读返回
        if !crate::scheduler::is_reused_error(e) {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("refresh failed: {}", e)}),
            );
        }
    }
    // 成功(_locked_fresh 已写回 store)或 reused(赢家已写回)——都回读 store 最新 auth_json
    schedule_save(state.store.clone());
    match state.store.lock() {
        Ok(s) => match s.accounts.get(id) {
            Some(a) => json_resp(
                StatusCode::OK,
                json!({
                    "auth_json": a.auth_json,
                    "refresh_token": a.refresh_token,
                }),
            ),
            None => json_resp(StatusCode::NOT_FOUND, json!({"error": "account not found"})),
        },
        Err(e) => err_resp(format!("锁获取失败: {}", e)),
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

#[derive(Deserialize)]
struct AntigravityOAuthCompletePayload {
    code: String,
    redirect_uri: String,
}

async fn handle_antigravity_oauth_complete(
    state: &ApiState,
    req: Request<Incoming>,
) -> Response<ResponseBody> {
    let body = match req.collect().await {
        Ok(body) => body.to_bytes(),
        Err(error) => return err_resp(format!("读取 OAuth body 失败: {error}")),
    };
    let payload: AntigravityOAuthCompletePayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("OAuth JSON 解析失败: {error}")}),
            )
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()
    {
        Ok(client) => client,
        Err(error) => return err_resp(error.to_string()),
    };
    let config = crate::antigravity::oauth::OAuthClientConfig::from_environment();
    let credential = match crate::antigravity::oauth::complete_credential(
        &client,
        &config,
        &payload.code,
        &payload.redirect_uri,
    )
    .await
    {
        Ok(credential) => credential,
        Err(error) => return json_resp(StatusCode::BAD_REQUEST, json!({"error": error})),
    };
    let mut auth_json = credential.to_auth_json();
    if let Ok(quotas) = crate::antigravity::quota::fetch_model_quotas(
        &client,
        &credential.access_token,
        &credential.project_id,
    )
    .await
    {
        crate::antigravity::quota::write_model_quotas(&mut auth_json, &quotas);
    }
    let account = {
        let mut store = match state.store.lock() {
            Ok(store) => store,
            Err(error) => return err_resp(error.to_string()),
        };
        let account = store.add_antigravity_account(
            credential.email.clone(),
            auth_json,
            Some("Google Antigravity OAuth".to_string()),
        );
        if let Err(error) = store.save() {
            return err_resp(error);
        }
        account
    };
    let mirror = antigravity_account_mirror(account);
    let _ = state.app_handle.emit("accounts-updated", ());
    json_resp(StatusCode::OK, json!({"account": mirror}))
}

fn antigravity_account_mirror(account: Account) -> Account {
    let mut mirror = account;
    mirror.refresh_token = None;
    mirror.auth_json = json!({
        "provider": "antigravity",
        "email": mirror.name,
        "project_id": mirror.auth_json.get("project_id").cloned().unwrap_or(Value::Null),
        "model_quotas": mirror.auth_json.get("model_quotas").cloned().unwrap_or(Value::Null),
    });
    mirror
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
        if let Some(old) = store.accounts.get(&final_id) {
            // Client 只保存 Google 账号的无密钥镜像。用户在 Client 修改备注/
            // 到期日或手动点“推送 Server”时，普通 upsert 也可能被触发。
            // Server 是 Google refresh token 的唯一写者：无 token 镜像只允许更新
            // 非密钥字段，绝不能覆盖 Server 上的认证材料。
            if old.is_antigravity_oauth()
                && to_write.is_antigravity_oauth()
                && to_write
                    .auth_json
                    .pointer("/tokens/refresh_token")
                    .and_then(Value::as_str)
                    .is_none()
            {
                let incoming_quotas = to_write.auth_json.get("model_quotas").cloned();
                to_write.auth_json = old.auth_json.clone();
                if let (Some(quotas), Some(object)) =
                    (incoming_quotas, to_write.auth_json.as_object_mut())
                {
                    object.insert("model_quotas".to_string(), quotas);
                }
                to_write.refresh_token = old.refresh_token.clone();
            }
            if action == "merged" {
                to_write.created_at = old.created_at.clone();
                if to_write.notes.is_none() {
                    to_write.notes = old.notes.clone();
                }
                if to_write.account_expires_at.is_none() {
                    to_write.account_expires_at = old.account_expires_at.clone();
                }
                // identity merge 代表旧账号换 id/重新导入；新客户端没有显式配置时，
                // 保留 Server 上已经启用的开关。普通同-id update 接受客户端开关，
                // 所以用户仍可以正常关闭。
                if !to_write.window_priming.configured
                    && !to_write.window_priming.enabled()
                    && old.window_priming.enabled()
                {
                    to_write.window_priming.five_hour_enabled =
                        old.window_priming.five_hour_enabled;
                    to_write.window_priming.weekly_enabled = old.window_priming.weekly_enabled;
                }
            }

            // last_* 是 Server 执行真实请求前写入的防重水位，只能单调向前。
            // client 可能在下一次 /quotas 同步前推来旧 Account，绝不能回滚水位后再发一次。
            to_write
                .window_priming
                .merge_runtime_watermarks_from(&old.window_priming);
        } else if to_write.is_antigravity_oauth()
            && to_write
                .auth_json
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str)
                .is_none()
        {
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({"error": "tokenless Google mirror cannot create a Server credential"}),
            );
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

    // 非 OpenAI Provider 账号不走 OpenAI usage/refresh 接口。
    let is_openai_account = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.accounts.get(&id).map(|a| a.is_openai_account()))
        .unwrap_or(false);

    let access_token = if !is_openai_account {
        None // 跳过下面的 OpenAI fetch_usage_direct 分支
    } else {
        match access_token_opt {
            Some(t) => Some(t),
            None => {
                if refresh_token.is_some() {
                    match crate::oauth::refresh_access_token_locked_fresh(&state.store, &id).await {
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
                            let terminal = crate::scheduler::is_logged_out_error(&e)
                                || crate::scheduler::is_revoked_error(&e);
                            if crate::scheduler::is_logged_out_error(&e) {
                                if let Ok(mut s) = state.store.lock() {
                                    if let Some(acc) = s.accounts.get_mut(&id) {
                                        acc.is_logged_out = true;
                                        acc.is_token_invalid = false;
                                    }
                                }
                            } else if crate::scheduler::is_revoked_error(&e) {
                                if let Ok(mut s) = state.store.lock() {
                                    if let Some(acc) = s.accounts.get_mut(&id) {
                                        acc.is_token_invalid = true;
                                        acc.is_logged_out = false;
                                    }
                                }
                            }
                            if terminal {
                                schedule_save(state.store.clone());
                            }
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
        match crate::usage::UsageFetcher::fetch_usage_direct(
            at,
            account_id,
            refresh_token,
            true,
            Some(id.to_string()),
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
                            primary_window_seconds: usage.primary_window_seconds,
                            five_hour_label: usage.five_hour_label.clone(),
                            weekly_left: usage.weekly_left as f64,
                            weekly_reset: usage.weekly_reset.clone(),
                            weekly_reset_at: usage.weekly_reset_at,
                            secondary_window_seconds: usage.secondary_window_seconds,
                            weekly_label: usage.weekly_label.clone(),
                            plan_type: usage.plan_type.clone(),
                            is_valid_for_cli: usage.is_valid_for_cli,
                            reset_credits: usage.reset_credits,
                            spark: usage.spark.clone(),
                            updated_at: chrono::Utc::now(),
                        });
                        acc.is_banned = false;
                        acc.is_token_invalid = false;
                        acc.is_logged_out = false;
                        crate::scheduler::clear_recovered_reused_error(acc);
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

    if let Some(error) = quota_error.as_deref() {
        if error.contains("invalid_grant")
            || crate::scheduler::is_logged_out_error(error)
            || crate::scheduler::is_revoked_error(error)
        {
            let _ = state.app_handle.emit("accounts-updated", ());
            return json_resp(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": if crate::scheduler::is_logged_out_error(error) {
                        "ACCOUNT_LOGGED_OUT:登录已失效，refresh_token 已过期或被撤销，请重新登录"
                    } else {
                        error
                    }
                }),
            );
        }
    }

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

    let (access_token_opt, account_id, refresh_token, is_openai_account) = {
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
                a.is_openai_account(),
            ),
            None => {
                return json_resp(StatusCode::NOT_FOUND, json!({"error": "account not found"}));
            }
        }
    };

    // 非 OpenAI Provider 账号：Server 不查 OpenAI usage。
    if !is_openai_account {
        return json_resp(
            StatusCode::OK,
            json!({"ok": true, "skipped": "provider account; no OpenAI usage refresh"}),
        );
    }

    let access_token = match access_token_opt {
        Some(t) => t,
        None => {
            if refresh_token.is_none() {
                return json_resp(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "TOKEN_INVALID:无 access_token 且无 refresh_token"}),
                );
            }
            match crate::oauth::refresh_access_token_locked_fresh(&state.store, &id).await {
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
        Some(id.to_string()),
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
                        primary_window_seconds: usage.primary_window_seconds,
                        five_hour_label: usage.five_hour_label.clone(),
                        weekly_left: usage.weekly_left as f64,
                        weekly_reset: usage.weekly_reset.clone(),
                        weekly_reset_at: usage.weekly_reset_at,
                        secondary_window_seconds: usage.secondary_window_seconds,
                        weekly_label: usage.weekly_label.clone(),
                        plan_type: usage.plan_type.clone(),
                        is_valid_for_cli: usage.is_valid_for_cli,
                        reset_credits: usage.reset_credits,
                        spark: usage.spark.clone(),
                        updated_at: chrono::Utc::now(),
                    });
                    acc.is_banned = false;
                    acc.is_token_invalid = false;
                    acc.is_logged_out = false;
                    crate::scheduler::clear_recovered_reused_error(acc);
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

#[cfg(test)]
mod google_quota_tests {
    use super::*;

    #[test]
    fn oauth_mirror_keeps_model_quotas_without_tokens() {
        let mut store = AccountStore::default();
        let quotas = json!({"gemini-3.7-flash-high": {
            "remaining_fraction": 0.75, "reset_time": "later", "updated_at": "now"
        }});
        let account = store.add_antigravity_account(
            "test@example.com".into(),
            json!({
                "provider": "antigravity", "project_id": "test-project",
                "tokens": {"access_token": "test-st", "refresh_token": "test-rt"},
                "model_quotas": quotas
            }),
            None,
        );
        let mirror = antigravity_account_mirror(account.clone());
        assert_eq!(mirror.auth_json["model_quotas"], quotas);
        assert_eq!(mirror.auth_json["project_id"], "test-project");
        assert!(mirror.auth_json.get("tokens").is_none());
        assert!(mirror.refresh_token.is_none());
        assert_eq!(
            store.accounts[&account.id].auth_json["tokens"]["refresh_token"],
            "test-rt"
        );
    }
}
