//! Codex Switcher - Tauri 主入口
//!
//! 暴露所有 Tauri 命令供前端调用

pub mod account;
mod bulk_import;
mod codex_sessions;
mod codex_ua;
mod deep_link;
mod ide_control;
pub mod mailbox;
pub mod oauth;
mod oauth_server;
pub mod otp_login;
mod provider_quirks;
mod proxy;
mod quota_snapshot;
mod refresh_lock;
pub mod relay_translate;
mod remote_client;
mod remote_server;
mod scheduler;
pub mod sentinel;
mod session_affinity;
mod session_import;
mod session_routes;
mod skills;
mod switch_log;
mod token_tracker;
mod tray;
mod usage;

use account::{Account, AccountStore};
use chrono::Utc;
use refresh_lock::RefreshLockManager;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::process::Command;
use tauri::{Emitter, Manager, State};
use usage::{UsageDisplay, UsageFetcher};

const QUARANTINE_FIX_TICKET_TTL_SECS: i64 = 120;

#[derive(Clone, Debug)]
struct QuarantineFixTicket {
    value: String,
    expires_at: chrono::DateTime<Utc>,
}

fn allow_local_refresh_for_quota(is_current: bool) -> bool {
    let _ = is_current;
    // 统一禁用配额查询路径下的本地 refresh。防止非当前账号消耗旧 refresh_token。
    false
}

fn detect_sync_conflict_for_current(
    account: &Account,
    disk_auth: &serde_json::Value,
) -> Option<String> {
    // 身份不一致时不应提示“Token 冲突”，避免误判
    if !AccountStore::auth_identity_matches(&account.auth_json, disk_auth) {
        return None;
    }

    let official_rt = AccountStore::extract_refresh_token(disk_auth);
    let local_rt = AccountStore::extract_refresh_token(&account.auth_json);

    // 如果官方 Token 存在且与本地不同（通常是更新了），则视为冲突
    if official_rt.is_some() && official_rt != local_rt {
        let disk_email =
            AccountStore::extract_email(disk_auth).unwrap_or_else(|| "未知账号".to_string());
        if disk_email == account.name {
            return Some(account.name.clone());
        } else {
            return Some(format!("{} ({})", account.name, disk_email));
        }
    }

    None
}

/// 全局 store 句柄，供 panic_hook / 退出兜底使用（panic hook 拿不到 Tauri 的
/// `State`，所以只能借这条侧通道）。在 `AppState::new()` 里写一次。
static GLOBAL_STORE_FOR_EXIT: std::sync::OnceLock<
    std::sync::Arc<std::sync::Mutex<AccountStore>>,
> = std::sync::OnceLock::new();

/// 应用状态
pub struct AppState {
    pub store: std::sync::Arc<std::sync::Mutex<AccountStore>>,
    pub scheduler: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub proxy_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub proxy_stats: std::sync::Arc<proxy::ProxyStats>,
    pub token_tracker: std::sync::Arc<token_tracker::TokenTracker>,
    /// 切号时通知所有 WebSocket 连接断开重连
    pub ws_disconnect: std::sync::Arc<tokio::sync::Notify>,
    pub switch_logger: std::sync::Arc<switch_log::SwitchLogger>,
    pub session_affinity: std::sync::Arc<session_affinity::SessionAffinity>,
    /// 用户级硬路由：session_id → account 强绑定。持久化到 ~/.codex-switcher/session_routes.json
    pub session_routes: std::sync::Arc<std::sync::Mutex<session_routes::SessionRoutesStore>>,
    pub quota_refresh_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub refresh_locks: RefreshLockManager,
    pub remote_server_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub solo_heartbeat_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    quarantine_fix_ticket: std::sync::Mutex<Option<QuarantineFixTicket>>,
}

impl AppState {
    pub fn new() -> Self {
        let store = std::sync::Arc::new(std::sync::Mutex::new(AccountStore::load()));
        // 注册到全局侧通道，供 panic hook / RunEvent::Exit 在 Tauri State 不可达
        // 的位置使用。第二次调用会被忽略（OnceLock 语义）—— 多实例非预期场景下
        // 也只会保留第一份。
        let _ = GLOBAL_STORE_FOR_EXIT.set(store.clone());
        Self {
            store,
            scheduler: std::sync::Mutex::new(None),
            proxy_handle: std::sync::Mutex::new(None),
            proxy_stats: std::sync::Arc::new(proxy::ProxyStats::default()),
            token_tracker: token_tracker::TokenTracker::new(),
            ws_disconnect: std::sync::Arc::new(tokio::sync::Notify::new()),
            switch_logger: switch_log::SwitchLogger::new(),
            session_affinity: std::sync::Arc::new(session_affinity::SessionAffinity::new()),
            session_routes: std::sync::Arc::new(std::sync::Mutex::new(
                session_routes::SessionRoutesStore::load(),
            )),
            quota_refresh_handle: std::sync::Mutex::new(None),
            refresh_locks: RefreshLockManager::default(),
            remote_server_handle: std::sync::Mutex::new(None),
            solo_heartbeat_handle: std::sync::Mutex::new(None),
            quarantine_fix_ticket: std::sync::Mutex::new(None),
        }
    }

    fn issue_quarantine_fix_ticket(&self) -> Result<String, String> {
        let ticket = uuid::Uuid::new_v4().to_string();
        let expires_at = Utc::now() + chrono::Duration::seconds(QUARANTINE_FIX_TICKET_TTL_SECS);
        let mut slot = self
            .quarantine_fix_ticket
            .lock()
            .map_err(|e| e.to_string())?;
        *slot = Some(QuarantineFixTicket {
            value: ticket.clone(),
            expires_at,
        });
        Ok(ticket)
    }

    fn consume_quarantine_fix_ticket(&self, provided_ticket: &str) -> Result<(), String> {
        let mut slot = self
            .quarantine_fix_ticket
            .lock()
            .map_err(|e| e.to_string())?;
        let now = Utc::now();
        match slot.take() {
            Some(stored) if stored.expires_at < now => {
                Err("安全确认已过期，请重新点击修复".to_string())
            }
            Some(stored) if stored.value != provided_ticket => {
                Err("安全确认无效，请重新点击修复".to_string())
            }
            Some(_) => Ok(()),
            None => Err("缺少安全确认，请重新点击修复".to_string()),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// 获取所有账号
#[tauri::command]
fn get_accounts(state: State<AppState>) -> Result<Vec<Account>, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    Ok(store.list_accounts().into_iter().cloned().collect())
}

/// 获取当前激活的账号 ID
#[tauri::command]
fn get_current_account_id(state: State<AppState>) -> Result<Option<String>, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    Ok(store.current.clone())
}

/// 获取全局设置
#[tauri::command]
fn get_settings(state: State<AppState>) -> Result<account::AppSettings, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    Ok(store.settings.clone())
}

/// 代理状态信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProxyStatus {
    pub enabled: bool,
    pub port: u16,
    pub is_running: bool,
    pub base_url: String,
    pub allow_lan: bool,
    pub lan_base_url: Option<String>,
    pub total_requests: u64,
    pub auto_switches: u64,
}

fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

fn detect_zerotier_ipv4() -> Option<Ipv4Addr> {
    let output = Command::new("ifconfig").output().ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("inet ") {
            continue;
        }

        let ip = trimmed.split_whitespace().nth(1)?;
        let parsed = ip.parse::<Ipv4Addr>().ok()?;

        // 优先 ZeroTier 常见的 172.16.0.0/12 网段
        let octets = parsed.octets();
        if octets[0] == 172 && (16..=31).contains(&octets[1]) {
            return Some(parsed);
        }
    }

    None
}

fn detect_client_ipv4() -> Option<Ipv4Addr> {
    detect_zerotier_ipv4().or_else(detect_lan_ipv4)
}

/// 获取代理状态
#[tauri::command]
fn get_proxy_status(state: State<AppState>) -> Result<ProxyStatus, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    let is_running = state
        .proxy_handle
        .lock()
        .map(|h| h.is_some())
        .unwrap_or(false);
    Ok(ProxyStatus {
        enabled: store.settings.proxy_enabled,
        port: store.settings.proxy_port,
        is_running,
        base_url: format!("http://localhost:{}/v1", store.settings.proxy_port),
        allow_lan: store.settings.proxy_allow_lan,
        lan_base_url: if store.settings.proxy_allow_lan {
            detect_client_ipv4().map(|ip| format!("http://{}:{}/v1", ip, store.settings.proxy_port))
        } else {
            None
        },
        total_requests: state
            .proxy_stats
            .total_requests
            .load(std::sync::atomic::Ordering::Relaxed),
        auto_switches: state
            .proxy_stats
            .auto_switches
            .load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// 更新全局设置
#[tauri::command]
fn update_settings(
    state: State<AppState>,
    app: tauri::AppHandle,
    mut settings: account::AppSettings,
) -> Result<(), String> {
    // client 模式硬约束：本机不做保活（保活由 Server 负责）
    // quota_refresh_enabled 在 client 模式下被用作"Server 状态同步循环"的开关；
    // 即使用户把它关掉，我们也始终会启动该循环（见下面启动条件）。
    if settings.remote_mode == "client" {
        settings.background_refresh = false;
    }
    let (
        prev_bg_refresh,
        prev_proxy_enabled,
        prev_proxy_port,
        prev_proxy_allow_lan,
        prev_quota_refresh,
        prev_remote_mode,
    ) = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        let prev = (
            store.settings.background_refresh,
            store.settings.proxy_enabled,
            store.settings.proxy_port,
            store.settings.proxy_allow_lan,
            store.settings.quota_refresh_enabled,
            store.settings.remote_mode.clone(),
        );
        store.settings = settings.clone();
        store.save()?;
        prev
    };

    // 联动刷新托盘菜单文案 (同步更新“下个账号”预览)
    crate::tray::update_tray_menu(&app);

    // 后台刷新生命周期
    let mut scheduler_handle = state.scheduler.lock().map_err(|e| e.to_string())?;
    match (prev_bg_refresh, settings.background_refresh) {
        (false, true) => {
            if scheduler_handle.is_none() {
                let handle = scheduler::start(state.store.clone(), app.clone());
                *scheduler_handle = Some(handle);
            }
        }
        (true, false) => {
            if let Some(handle) = scheduler_handle.take() {
                handle.abort();
            }
        }
        _ => {}
    }

    // 代理生命周期
    let mut proxy_handle = state.proxy_handle.lock().map_err(|e| e.to_string())?;
    let proxy_config_changed =
        prev_proxy_port != settings.proxy_port || prev_proxy_allow_lan != settings.proxy_allow_lan;
    match (prev_proxy_enabled, settings.proxy_enabled) {
        (false, true) => {
            if proxy_handle.is_none() {
                let handle = proxy::start(
                    state.store.clone(),
                    settings.proxy_port,
                    settings.proxy_allow_lan,
                    app.clone(),
                    state.proxy_stats.clone(),
                    state.token_tracker.clone(),
                    state.ws_disconnect.clone(),
                    state.switch_logger.clone(),
                    state.session_affinity.clone(),
                    state.session_routes.clone(),
                );
                *proxy_handle = Some(handle);
                println!("[Proxy] 代理已启动 (端口 {})", settings.proxy_port);
            }
        }
        (true, false) => {
            if let Some(handle) = proxy_handle.take() {
                handle.abort();
                println!("[Proxy] 代理已停止");
            }
        }
        (true, true) if proxy_config_changed => {
            if let Some(handle) = proxy_handle.take() {
                handle.abort();
            }
            let handle = proxy::start(
                state.store.clone(),
                settings.proxy_port,
                settings.proxy_allow_lan,
                app.clone(),
                state.proxy_stats.clone(),
                state.token_tracker.clone(),
                state.ws_disconnect.clone(),
                state.switch_logger.clone(),
                state.session_affinity.clone(),
                state.session_routes.clone(),
            );
            *proxy_handle = Some(handle);
            println!(
                "[Proxy] 代理已重启 (端口 {}, 局域网访问: {})",
                settings.proxy_port, settings.proxy_allow_lan
            );
        }
        _ => {}
    }

    // 定时额度刷新生命周期
    // - 非 client 模式：遵循 quota_refresh_enabled
    // - client 模式：无条件运行（它同时承担 Server 状态同步的职责）
    let mut qr_handle = state
        .quota_refresh_handle
        .lock()
        .map_err(|e| e.to_string())?;
    let is_client = settings.remote_mode == "client";
    let prev_should_run = prev_quota_refresh; // 旧语义里 client 模式被强制 false，所以这里就是 enabled
    let next_should_run = settings.quota_refresh_enabled || is_client;
    match (prev_should_run, next_should_run) {
        (false, true) => {
            if qr_handle.is_none() {
                let handle = start_quota_refresh(state.store.clone(), app.clone());
                *qr_handle = Some(handle);
                println!(
                    "[QuotaRefresh] 循环已启动（enabled={} client={}）",
                    settings.quota_refresh_enabled, is_client
                );
            }
        }
        (true, false) => {
            if let Some(handle) = qr_handle.take() {
                handle.abort();
                println!("[QuotaRefresh] 循环已停止");
            }
        }
        _ => {}
    }

    // solo 心跳循环生命周期：remote_mode 进/出 "solo" 时启停
    {
        let mut slot = state
            .solo_heartbeat_handle
            .lock()
            .map_err(|e| e.to_string())?;
        let was_solo = prev_remote_mode == "solo";
        let is_solo = settings.remote_mode == "solo";
        match (was_solo, is_solo) {
            (false, true) => {
                if slot.is_none() {
                    let h = start_solo_heartbeat(state.store.clone(), app.clone());
                    *slot = Some(h);
                    println!("[Solo] 心跳循环启动（settings）");
                }
            }
            (true, false) => {
                if let Some(h) = slot.take() {
                    h.abort();
                    println!("[Solo] 心跳循环停止（settings）");
                }
            }
            _ => {}
        }
    }

    app.emit("settings-updated", ()).ok();
    Ok(())
}

/// 从当前 Codex 登录状态导入账号
#[tauri::command]
fn import_current_account(
    state: State<AppState>,
    app: tauri::AppHandle,
    name: String,
    notes: Option<String>,
) -> Result<Account, String> {
    let auth_json = AccountStore::read_codex_auth()?;
    if AccountStore::extract_refresh_token(&auth_json).is_none() {
        return Err("当前 auth.json 缺少 refresh_token，无法自动续期，请重新登录".to_string());
    }

    let account = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store.add_account(name, auth_json, notes);
        store.save()?;
        account
    };
    crate::tray::update_tray_menu(&app);
    Ok(account)
}

// is_token_expired removed: align with Codex last_refresh-based refresh

/// 检查当前 IDE 中的账号是否有未同步的 Token 更新
#[tauri::command]
fn check_sync_conflict(state: State<AppState>) -> Result<Option<String>, String> {
    let auth_json = match AccountStore::read_codex_auth() {
        Ok(a) => a,
        Err(_) => return Ok(None), // 如果由于文件不存在等原因读取失败，视为无冲突
    };

    let store = state.store.lock().map_err(|e| e.to_string())?;

    // 检查这个 auth.json 是否属于我们当前的活跃账号，且内容是否有变
    if let Some(current_id) = &store.current {
        if let Some(account) = store.accounts.get(current_id) {
            if let Some(name) = detect_sync_conflict_for_current(account, &auth_json) {
                return Ok(Some(name));
            }
        }
    }

    Ok(None)
}

/// 删除账号
#[tauri::command]
async fn delete_account(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    // 先取一份快照：client 模式下需要把删号指令同步给 Server
    let (remote_mode, primary, fallback, secret) = {
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
        if store.current.as_deref() == Some(&id) {
            store.current = None;
        }
        store.delete_account(&id)?;
        store.save()?;
    }

    // client / solo 模式：同步删除 Server 上的对应账号（失败不影响本地删除已完成的事实）
    if account::pushes_to_server(&remote_mode) && !secret.is_empty() {
        match remote_client::resolve_base_url(&primary, &fallback).await {
            Ok(base) => {
                if let Err(e) = remote_client::delete_account(&base, &secret, &id).await {
                    eprintln!("[DeleteAccount] Server 端联动删除失败（本地已删除）: {}", e);
                }
            }
            Err(e) => eprintln!("[DeleteAccount] Server 不可达（本地已删除）: {}", e),
        }
    }

    crate::tray::update_tray_menu(&app);
    Ok(())
}

/// 更新账号信息
#[tauri::command]
fn update_account(
    state: State<AppState>,
    app: tauri::AppHandle,
    id: String,
    name: Option<String>,
    notes: Option<String>,
) -> Result<(), String> {
    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        store.update_account(&id, name, notes)?;
        store.save()?;
    }
    crate::tray::update_tray_menu(&app);
    Ok(())
}

/// 更新 Relay usage 专用 Cookie（MiMo Token Plan 等控制台配额接口使用）。
#[tauri::command]
fn update_relay_usage_cookie(
    state: State<AppState>,
    id: String,
    usage_cookie: Option<String>,
) -> Result<(), String> {
    let normalized = usage_cookie
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    let mut store = state.store.lock().map_err(|e| e.to_string())?;
    store.update_relay_usage_cookie(&id, normalized)?;
    store.save()?;
    Ok(())
}

/// 设置账号级”非活跃保活刷新”开关
#[tauri::command]
fn set_account_inactive_refresh_enabled(
    state: State<AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let mut store = state.store.lock().map_err(|e| e.to_string())?;
    store.set_inactive_refresh_enabled(&id, enabled)?;
    store.save()?;
    Ok(())
}

/// 设置 / 取消 手机锚账号（Codex.app 手机远程连接绑定）。
///
/// 副作用：
/// - 开启时：立刻把该账号的 auth_json 写盘到 `~/.codex/auth.json`（让 Codex.app 看到 anchor 身份）
/// - 关闭时：把"当前 current"账号的 auth_json 写盘（回到旧的"current = disk"语义）
///
/// 后台 scheduler 的 anchor refresh tick 会接管之后的 token 保活。
#[tauri::command]
fn set_session_anchor(
    state: State<AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let (disk_auth, anchor_after, action) = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        store.set_session_anchor(&id, enabled)?;
        store.save()?;

        // 决定本次该把哪个账号写盘
        let after = store.session_anchor_id();
        if enabled {
            // 设为 anchor → 立刻把 anchor 的 auth_json 落盘
            let acc = store
                .accounts
                .get(&id)
                .ok_or_else(|| format!("账号不存在: {}", id))?;
            (Some(acc.to_codex_auth_value()), after, "set")
        } else {
            // 取消 anchor → 把当前 current 写盘（无 current 则跳过）
            let candidate = store
                .current
                .clone()
                .and_then(|cid| store.accounts.get(&cid).map(|a| a.to_codex_auth_value()));
            (candidate, after, "clear")
        }
    };

    if let Some(auth) = disk_auth {
        AccountStore::write_codex_auth(&auth)?;
        println!(
            "[Anchor] {} 完成；当前 anchor = {:?}",
            action, anchor_after
        );
    } else {
        println!("[Anchor] {} 完成，但无可写盘的候选账号", action);
    }

    // 切了 anchor 等于换了磁盘上的 chatgpt_account_id，proxy 远端 token 缓存必须失效
    crate::proxy::invalidate_remote_token_cache();
    Ok(())
}

/// 导出所有账号配置
#[tauri::command]
fn export_accounts(state: State<AppState>) -> Result<String, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    store.export()
}

/// 批量导入：自动嗅探 cpa / sub2api / cockpit / 四段RT / native 这 5 种格式。
/// 前端把每个文件读成 base64（binary 安全）传过来，filename 用来辅助嗅探（zip 后缀等）。
/// 返回每文件的 summary + 总账号详情，UI 用来给用户预览导入结果。
#[derive(Debug, serde::Deserialize)]
pub struct BulkImportFile {
    pub filename: String,
    /// 文件内容 base64
    pub content_b64: String,
}

#[tauri::command]
fn bulk_import_accounts(
    state: State<AppState>,
    app: tauri::AppHandle,
    files: Vec<BulkImportFile>,
) -> Result<bulk_import::BulkImportResult, String> {
    let mut summaries = Vec::new();
    let mut all_parsed: Vec<bulk_import::ParsedAccount> = Vec::new();
    let mut fatal = Vec::new();

    for f in files {
        match bulk_import::parse_one_file(&f.filename, &f.content_b64) {
            Ok((format, accounts)) => {
                summaries.push(bulk_import::ImportSummary {
                    format,
                    parsed: accounts.len(),
                    errors: Vec::new(),
                });
                all_parsed.extend(accounts);
            }
            Err(e) => {
                fatal.push(format!("{}: {}", f.filename, e));
            }
        }
    }

    // 落库：按 email 去重 —— 已有同名账号就跳过（不覆盖现有 token，避免误伤）
    let mut info = Vec::new();
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
        let existing_emails: std::collections::HashSet<String> =
            store.accounts.values().map(|a| a.name.clone()).collect();
        for p in &all_parsed {
            if existing_emails.contains(&p.email) {
                continue;
            }
            let acc = store.add_account(p.email.clone(), p.auth_json.clone(), None);
            newly_added_ids.push(acc.id.clone());
            info.push(bulk_import::BulkParsedAccountInfo {
                email: p.email.clone(),
                plan_type: p.plan_type.clone(),
                account_id: p.account_id.clone(),
                needs_refresh: p.needs_refresh,
            });
        }
        store.save()?;
    }
    crate::tray::update_tray_menu(&app);

    // client / solo 模式：把新导入的账号推到 Server，让 Server 接管刷新 + 配额查询
    // 否则后续 UI 刷新会调 remote_refresh_account_quota → Server 找不到账号
    if account::pushes_to_server(&remote_mode) && !secret.is_empty() && !newly_added_ids.is_empty()
    {
        let store_arc = state.store.clone();
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let base =
                match remote_client::resolve_base_url(&server_url, &server_url_fallback).await {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("[BulkImport] Server 不可达，跳过 push: {}", e);
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
                match remote_client::upsert_account(&base, &secret, &account_clone).await {
                    Ok(_) => pushed += 1,
                    Err(e) => eprintln!("[BulkImport] push {} 失败: {}", account_clone.name, e),
                }
            }
            if pushed > 0 {
                println!("[BulkImport] 批量导入后已推 {} 个账号到 Server", pushed);
                let _ = app_clone.emit("accounts-updated", ());
            }
        });
    }

    Ok(bulk_import::BulkImportResult {
        summaries,
        accounts: info,
        fatal,
    })
}

/// 导入账号配置
/// 添加中转站账号（手动表单 / deep link 共用）。
///
/// `base_url` 必须是 `http(s)://` 完整 URL；保存时尾斜杠会被去除。
/// `usage_preset` 命中内置 fetcher 名（如 `"openai_compat"`），None=不拉 usage。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn add_relay_account(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    name: String,
    base_url: String,
    api_key: String,
    homepage: Option<String>,
    usage_preset: Option<String>,
    usage_cookie: Option<String>,
    notes: Option<String>,
    model_map: Option<std::collections::HashMap<String, String>>,
    model_fallback: Option<String>,
    relay_protocol: Option<String>,
    relay_category: Option<String>,
) -> Result<Account, String> {
    let trimmed_url = base_url.trim();
    if !(trimmed_url.starts_with("https://") || trimmed_url.starts_with("http://")) {
        return Err("base_url 必须以 http:// 或 https:// 开头".to_string());
    }
    if api_key.trim().is_empty() {
        return Err("api_key 不能为空".to_string());
    }
    if name.trim().is_empty() {
        return Err("name 不能为空".to_string());
    }

    let (account, should_push) = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        let acc = store.add_relay_account(
            name.trim().to_string(),
            trimmed_url.to_string(),
            api_key.trim().to_string(),
            homepage
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty()),
            usage_preset
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            usage_cookie
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            notes,
            model_map,
            model_fallback
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty()),
            relay_protocol
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            relay_category
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
        );
        store.save()?;
        let push = account::pushes_to_server(&store.settings.remote_mode);
        (acc, push)
    };

    // client/solo 模式：把新建的 Relay 账号推到 Server，让 mini mac 也持有。
    // 这样 fast_auth_sync / quota_refresh 不会把这个账号当"本地残留"删掉。
    if should_push {
        match client_settings_snapshot(&state).await {
            Ok((url, secret)) => {
                let snapshot = state
                    .store
                    .lock()
                    .ok()
                    .and_then(|s| s.accounts.get(&account.id).cloned());
                if let Some(acc_snapshot) = snapshot {
                    match remote_client::upsert_account(&url, &secret, &acc_snapshot).await {
                        Ok(outcome) => println!(
                            "[Relay] upsert to server: id={} status={}",
                            outcome.id, outcome.upserted
                        ),
                        Err(e) => eprintln!("[Relay] 推送 Server 失败（账号已本地保存）: {}", e),
                    }
                }
            }
            Err(e) => eprintln!("[Relay] 读取 client 配置失败，未推送 Server: {}", e),
        }
    }

    crate::tray::update_tray_menu(&app);
    Ok(account)
}

/// 更新 Relay 账号的模型映射 / 兜底 / 上游协议（编辑功能用）。
#[tauri::command]
fn update_relay_model_map(
    state: State<AppState>,
    id: String,
    model_map: Option<std::collections::HashMap<String, String>>,
    model_fallback: Option<String>,
    relay_protocol: Option<String>,
) -> Result<(), String> {
    let mut store = state.store.lock().map_err(|e| e.to_string())?;
    let acc = store
        .accounts
        .get_mut(&id)
        .ok_or_else(|| format!("账号 {} 不存在", id))?;
    if !acc.is_relay() {
        return Err("不是中转站账号".to_string());
    }
    acc.relay_model_map = model_map;
    acc.relay_model_fallback = model_fallback
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty());
    if let Some(p) = relay_protocol {
        let trimmed = p.trim().to_string();
        if trimmed.is_empty() || trimmed == "responses" {
            acc.relay_protocol = None;
        } else {
            acc.relay_protocol = Some(trimmed);
        }
    }
    store.save()?;
    Ok(())
}

/// 主动刷新中转站账号的余额（用户点 UI 刷新按钮时调）。
///
/// 仅 Relay 类型可用。fetcher 由 `relay_usage_preset` 字段选定；为空时默认 `openai_compat`。
#[tauri::command]
async fn refresh_relay_usage(
    state: State<'_, AppState>,
    id: String,
) -> Result<account::RelayUsageCache, String> {
    let (base_url, api_key, preset, usage_cookie) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let acc = store.accounts.get(&id).ok_or("账号不存在")?;
        if !acc.is_relay() {
            return Err("不是中转站账号".into());
        }
        let base = acc.relay_base_url.clone().ok_or("中转站账号缺 base_url")?;
        let key =
            AccountStore::extract_access_token(&acc.auth_json).ok_or("中转站账号缺 api_key")?;
        let preset = acc.relay_usage_preset.clone();
        let usage_cookie = acc.relay_usage_cookie.clone();
        (base, key, preset, usage_cookie)
    };

    // "auto" 或缺省 → 探测：先 new-api dashboard，再 openai_compat；
    // 探测命中后把策略写回 acc.relay_usage_preset，下次直接走对应 fetcher。
    let needs_probe = matches!(preset.as_deref(), None | Some("auto"));
    let effective_preset: Option<String> = if needs_probe {
        match UsageFetcher::probe_relay_usage_preset(&base_url, &api_key).await {
            Some(p) => {
                if let Ok(mut store) = state.store.lock() {
                    if let Some(acc) = store.accounts.get_mut(&id) {
                        acc.relay_usage_preset = Some(p.clone());
                        let _ = store.save();
                    }
                }
                Some(p)
            }
            None => return Err("自动探测未命中：上游不支持 /v1/dashboard/billing 或 /v1/usage（可手动选 usage 策略，或保持「不拉取」）".to_string()),
        }
    } else {
        preset
    };

    let cache = match effective_preset.as_deref() {
        Some("openai_compat") => {
            UsageFetcher::fetch_relay_usage_openai_compat(&base_url, &api_key).await?
        }
        Some("new_api_dashboard") => {
            UsageFetcher::fetch_relay_usage_new_api_dashboard(&base_url, &api_key).await?
        }
        Some("glm_zhipu") => UsageFetcher::fetch_relay_usage_glm_zhipu(&base_url, &api_key).await?,
        Some("mimo_token_plan") => {
            let cookie = usage_cookie
                .ok_or("MiMo 配额查询需要登录 platform.xiaomimimo.com 后复制 Cookie header")?;
            UsageFetcher::fetch_relay_usage_mimo_token_plan(&cookie).await?
        }
        Some(other) => return Err(format!("未支持的 usage_preset: {}", other)),
        None => return Err("usage 策略未确定".to_string()),
    };

    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(acc) = store.accounts.get_mut(&id) {
            acc.relay_usage_cache = Some(cache.clone());
            store.save()?;
        }
    }
    Ok(cache)
}

#[tauri::command]
fn import_accounts(
    state: State<AppState>,
    app: tauri::AppHandle,
    json: String,
) -> Result<(), String> {
    let new_store = AccountStore::import(&json)?;
    let missing = new_store.accounts_missing_refresh_token();
    if !missing.is_empty() {
        return Err(format!(
            "以下账号缺少 refresh_token，无法自动续期，请重新登录后再导入: {}",
            missing.join(", ")
        ));
    }
    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        *store = new_store;
        store.save()?;
    }
    crate::tray::update_tray_menu(&app);
    Ok(())
}

/// 把已经拿到的 OAuth Token 落进账号库 + 推 Server + 刷托盘。
/// 浏览器登录和 OTP 自动登录都走这一条路。
async fn save_token_as_account(
    state: &tauri::State<'_, AppState>,
    app: &tauri::AppHandle,
    token_res: oauth::TokenResponse,
    notes: Option<String>,
) -> Result<Account, String> {
    if token_res.refresh_token.is_none() {
        return Err("OAuth 未返回 refresh_token，无法自动续期".to_string());
    }

    let user_info = token_res
        .id_token
        .as_ref()
        .and_then(|id_t| oauth::parse_user_info(id_t))
        .ok_or("无法从授权响应中解析用户信息 (Missing ID Token)")?;

    let (account, is_client_mode) = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;

        let expires_at = token_res
            .expires_in
            .map(|secs| (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339());

        let auth_json = serde_json::json!({
            "tokens": {
                "access_token": token_res.access_token,
                "refresh_token": token_res.refresh_token,
                "id_token": token_res.id_token,
                "account_id": user_info.account_id,
                "expires_at": expires_at
            },
            "last_refresh": chrono::Utc::now().to_rfc3339()
        });

        let mut account = store.add_account(user_info.email, auth_json, notes);

        account.refresh_token = token_res.refresh_token.clone();
        if let Some(acc) = store.accounts.get_mut(&account.id) {
            acc.refresh_token = token_res.refresh_token;
        }

        store.save()?;
        let should_push = account::pushes_to_server(&store.settings.remote_mode);
        (account, should_push)
    };

    if is_client_mode {
        let (url, secret) = client_settings_snapshot(state).await?;
        let to_push = {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            store.accounts.get(&account.id).cloned()
        };
        if let Some(acc_snapshot) = to_push {
            match remote_client::upsert_account(&url, &secret, &acc_snapshot).await {
                Ok(outcome) => {
                    if outcome.upserted == "merged" && outcome.id != account.id {
                        let new_id = outcome.id.clone();
                        if let Ok(mut store) = state.store.lock() {
                            if let Some(mut a) = store.accounts.remove(&account.id) {
                                a.id = new_id.clone();
                                store.accounts.insert(new_id.clone(), a);
                                if store.current.as_deref() == Some(account.id.as_str()) {
                                    store.current = Some(new_id.clone());
                                }
                                let _ = store.save();
                            }
                        }
                        let _ = app.emit("accounts-updated", ());
                    }
                    println!(
                        "[Login] 已推送新账号到 Server：id={} action={} quota_refreshed={}",
                        outcome.id, outcome.upserted, outcome.quota_refreshed
                    );
                }
                Err(e) => eprintln!("[Login] 推送新账号到 Server 失败: {}", e),
            }
        }
    }

    crate::tray::update_tray_menu(app);
    Ok(account)
}

/// 强制把当前激活账号的 auth_json 覆盖到 ~/.codex/auth.json。
/// 用于"switcher 当前账号 ↔ 磁盘 auth.json 身份不匹配"时的兜底：用户明确表态"我要保住 switcher 这一个"。
/// 不动任何 codex 进程；前端按 auto_reload_ide 设置决定是否再调 reload_ide_windows。
#[tauri::command]
async fn force_overwrite_disk_with_current(
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let auth_json = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let current_id = store
            .current
            .clone()
            .ok_or_else(|| "没有当前激活账号".to_string())?;
        let account = store
            .accounts
            .get(&current_id)
            .ok_or_else(|| format!("账号 {} 不存在", current_id))?;
        account.auth_json.clone()
    };
    AccountStore::write_codex_auth(&auth_json)?;
    Ok("已覆盖 ~/.codex/auth.json".to_string())
}

/// 完成 OAuth 登录并保存账号
#[tauri::command]
async fn finalize_oauth_login(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    code: String,
) -> Result<Account, String> {
    let token_res = oauth_server::complete_oauth_login(code).await?;
    save_token_as_account(
        &state,
        &app,
        token_res,
        Some("OpenAI OAuth 登录".to_string()),
    )
    .await
}

// ============================================================================
// 邮箱 OTP 批量自动授权
// ============================================================================

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OtpBatchProgress {
    index: usize,
    total: usize,
    email: String,
    /// "pending" | "running" | "ok" | "fail"
    status: &'static str,
    /// provider tag (前端进度行徽章用)
    provider: String,
    /// 当 status="running" 时阶段文字
    stage: Option<String>,
    /// 成功时账号 id
    account_id: Option<String>,
    /// 失败 message
    error: Option<String>,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OtpBatchResult {
    success: Vec<String>,
    failed: Vec<(String, String)>,
}

/// 前端传进来的一条 OTP 任务。
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct OtpEntry {
    email: String,
    /// "usmail" | "sorryios"，为空按 "usmail" 处理
    #[serde(default)]
    provider: Option<String>,
    /// sorryios 必须提供（32 位 token）
    #[serde(default)]
    token: Option<String>,
}

fn build_mailbox(entry: &OtpEntry) -> Result<Option<mailbox::MailboxProvider>, String> {
    let provider = entry
        .provider
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match provider.as_str() {
        "" | "usmail" => Ok(None), // None = run_login 默认 usmail
        "sorryios" => {
            let token = entry
                .token
                .as_deref()
                .ok_or_else(|| "sorryios provider 缺少 token".to_string())?
                .trim()
                .to_string();
            if token.is_empty() {
                return Err("sorryios provider token 为空".into());
            }
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|e| format!("构建 sorryios HTTP client 失败: {e}"))?;
            Ok(Some(mailbox::MailboxProvider::Sorryios(
                mailbox::SorryiosNet::new(client, token).since_now(),
            )))
        }
        "nissanserena" => {
            // 需要 cookie store 维持 session
            let client = reqwest::Client::builder()
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|e| format!("构建 nissanserena HTTP client 失败: {e}"))?;
            Ok(Some(mailbox::MailboxProvider::NissanSerena(
                mailbox::NissanSerena::new(client).since_now(),
            )))
        }
        other => Err(format!("未知的 mailbox provider: {other}")),
    }
}

/// 批量邮箱 OTP 自动授权。串行跑，每条都通过 save_token_as_account 落账号。
/// 进度通过事件 "otp-batch-progress" 实时推到前端。
#[tauri::command]
async fn start_otp_login_batch(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    entries: Vec<OtpEntry>,
    timeout_secs: Option<u64>,
) -> Result<OtpBatchResult, String> {
    use tauri::Emitter;
    let timeout = timeout_secs.unwrap_or(180);
    let total = entries.len();
    let mut success = Vec::new();
    let mut failed = Vec::new();

    let provider_tag = |e: &OtpEntry| -> String {
        e.provider
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "usmail".to_string())
    };

    // 先把全部以 pending 状态推一次，前端立刻看到列表
    for (i, entry) in entries.iter().enumerate() {
        let _ = app.emit(
            "otp-batch-progress",
            OtpBatchProgress {
                index: i,
                total,
                email: entry.email.clone(),
                status: "pending",
                provider: provider_tag(entry),
                stage: None,
                account_id: None,
                error: None,
            },
        );
    }

    for (i, entry) in entries.iter().enumerate() {
        let email = entry.email.clone();
        let tag = provider_tag(entry);
        let _ = app.emit(
            "otp-batch-progress",
            OtpBatchProgress {
                index: i,
                total,
                email: email.clone(),
                status: "running",
                provider: tag.clone(),
                stage: Some("starting".into()),
                account_id: None,
                error: None,
            },
        );

        let mailbox = match build_mailbox(entry) {
            Ok(mb) => mb,
            Err(e) => {
                failed.push((email.clone(), e.clone()));
                let _ = app.emit(
                    "otp-batch-progress",
                    OtpBatchProgress {
                        index: i,
                        total,
                        email,
                        status: "fail",
                        provider: tag,
                        stage: Some("provider".into()),
                        account_id: None,
                        error: Some(e),
                    },
                );
                continue;
            }
        };

        let result = otp_login::run_login(
            otp_login::LoginInput {
                email: email.clone(),
                otp_timeout_secs: timeout,
            },
            mailbox,
        )
        .await;

        match result {
            Ok(out) => {
                match save_token_as_account(
                    &state,
                    &app,
                    out.token,
                    Some("邮箱 OTP 自动授权".to_string()),
                )
                .await
                {
                    Ok(acc) => {
                        success.push(email.clone());
                        let _ = app.emit(
                            "otp-batch-progress",
                            OtpBatchProgress {
                                index: i,
                                total,
                                email: email.clone(),
                                status: "ok",
                                provider: tag.clone(),
                                stage: None,
                                account_id: Some(acc.id),
                                error: None,
                            },
                        );
                        let _ = app.emit("accounts-updated", ());
                    }
                    Err(e) => {
                        failed.push((email.clone(), e.clone()));
                        let _ = app.emit(
                            "otp-batch-progress",
                            OtpBatchProgress {
                                index: i,
                                total,
                                email: email.clone(),
                                status: "fail",
                                provider: tag.clone(),
                                stage: Some("save".into()),
                                account_id: None,
                                error: Some(e),
                            },
                        );
                    }
                }
            }
            Err(e) => {
                failed.push((email.clone(), e.clone()));
                let _ = app.emit(
                    "otp-batch-progress",
                    OtpBatchProgress {
                        index: i,
                        total,
                        email: email.clone(),
                        status: "fail",
                        provider: tag.clone(),
                        stage: Some("login".into()),
                        account_id: None,
                        error: Some(e),
                    },
                );
            }
        }
    }

    Ok(OtpBatchResult { success, failed })
}

// 补充 AppState 的辅助方法以方便在 finalize_oauth_login 中获取 AppHandle 是不行的，
// 因为 finalize_oauth_login 是 async 且 Command 宏会处理。
// 我们直接给 finalize_oauth_login 增加 AppHandle 参数。

/// 切换到指定账号（异步版本，不做本地 Token 续期）
#[tauri::command]
async fn switch_account(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    id: String,
) -> Result<(), String> {
    // 0. 切换前仅同步“当前激活账号”与官方 auth.json，避免全表匹配导致串号
    if let Ok(current_auth) = AccountStore::read_codex_auth() {
        if let Ok(mut store) = state.store.lock() {
            if let Some(current_id) = store.current.clone() {
                if store.sync_account_from_auth_json(&current_id, current_auth) {
                    if let Err(e) = store.save() {
                        eprintln!("[Sync] 保存当前账号失败: {}", e);
                    }
                }
            }
        }
    }

    // 0.5 提前判断 Relay 类型 —— 用于跳过 OpenAI usage 预检
    let is_target_relay = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.accounts.get(&id).map(|a| a.is_relay()))
        .unwrap_or(false);

    // 1. 获取目标账号的校验凭据
    let (target_id, access_token, refresh_token, account_id) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store
            .accounts
            .get(&id)
            .ok_or_else(|| format!("账号 {} 不存在", id))?;

        let access_token = account
            .auth_json
            .get("tokens")
            .and_then(|t| t.get("access_token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or("账号缺少 access_token")?;

        let refresh_token = account.refresh_token.clone();

        let account_id = account
            .auth_json
            .get("account_id")
            .or_else(|| {
                account
                    .auth_json
                    .get("tokens")
                    .and_then(|t| t.get("account_id"))
            })
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        (account.id.clone(), access_token, refresh_token, account_id)
    };

    // 1.5. 检查 JWT 是否过期，如果过期则尝试刷新
    // client/solo 模式：rt 是 Server 的权威，本机不刷。AT 即使过期也照样把请求交出去，
    // 让 proxy.silent_refresh_current 走 Server 路径换 token。
    let is_client_or_solo = matches!(
        state
            .store
            .lock()
            .map(|s| s.settings.remote_mode.clone())
            .unwrap_or_default()
            .as_str(),
        "client" | "solo"
    );
    let (access_token, refresh_token) = {
        let mut needs_refresh = false;
        if let Ok(claims) = AccountStore::extract_jwt_claims_from_token(&access_token) {
            if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
                let now = Utc::now().timestamp();
                // 如果剩余时间小于 5 分钟，则触发刷新
                if exp - now < 300 {
                    println!("[Switch] JWT 已过期或即将过期 ({}), 触发自动刷新", exp);
                    needs_refresh = true;
                }
            }
        } else {
            println!("[Switch] 无法解析 JWT Claims，尝试盲刷");
            needs_refresh = true;
        }

        if needs_refresh && is_client_or_solo {
            println!("[Switch] client/solo 模式：跳过本机 rt 刷新，由 proxy 走 Server 路径");
        }

        if needs_refresh && !is_client_or_solo && refresh_token.is_some() {
            if let Some(ref rt) = refresh_token {
                match oauth::refresh_access_token_locked(&target_id, rt).await {
                    Ok(token_res) => {
                        println!("[Switch] 自动刷新 Token 成功");
                        let mut store = state.store.lock().map_err(|e| e.to_string())?;
                        if let Some(account) = store.accounts.get_mut(&target_id) {
                            AccountStore::apply_refreshed_tokens(
                                account,
                                token_res.access_token.clone(),
                                token_res.refresh_token.clone(),
                                token_res.id_token,
                                token_res.expires_in,
                            );
                            if let Err(e) = store.save() {
                                eprintln!("[Store] 保存失败: {}", e);
                            }
                            (token_res.access_token, token_res.refresh_token)
                        } else {
                            (access_token, refresh_token)
                        }
                    }
                    Err(e) => {
                        println!("[Switch] 自动刷新 Token 失败: {}", e);
                        (access_token, refresh_token)
                    }
                }
            } else {
                (access_token, refresh_token)
            }
        } else {
            (access_token, refresh_token)
        }
    };

    // 2. 预检（非阻断）：仅尝试读取配额缓存，不触发本地 refresh_token 刷新。
    // 失败不阻断切换，交由 Codex 在实际请求中按需维护 token 生命周期。
    if is_target_relay {
        println!("[Switch] Relay 类型，跳过 OpenAI usage 预检: {}", target_id);
    } else {
        println!(
            "[Switch] 预检目标账号配额（不触发本地 refresh）: {}",
            target_id
        );
        match usage::UsageFetcher::fetch_usage_direct(
            access_token,
            account_id,
            refresh_token,
            false,
        )
        .await
        {
            Ok((usage, _)) => {
                // 写 quota 快照（先取 email 不锁 store）
                let email_for_snap = state
                    .store
                    .lock()
                    .ok()
                    .and_then(|s| {
                        s.accounts
                            .get(&target_id)
                            .and_then(|a| AccountStore::extract_email(&a.auth_json))
                    })
                    .unwrap_or_default();
                quota_snapshot::append_from_usage(
                    &target_id,
                    &email_for_snap,
                    &usage,
                    "switch_precheck",
                );
                let mut store = state.store.lock().map_err(|e| e.to_string())?;
                if let Some(account) = store.accounts.get_mut(&target_id) {
                    account.cached_quota = Some(account::CachedQuota {
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
                        updated_at: chrono::Utc::now(),
                    });
                    if let Err(e) = store.save() {
                        eprintln!("[Store] 保存失败: {}", e);
                    }
                }
            }
            Err(e) => {
                println!("[Switch] 预检配额失败（忽略，不阻断切换）: {}", e);
            }
        }
    } // end if !is_target_relay

    // 3. 执行切换：根据 switch_mode + 代理运行状态决定热/冷切
    let proxy_running = state
        .proxy_handle
        .lock()
        .map(|h| h.is_some())
        .unwrap_or(false);
    let hot = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        account::should_hot_switch(&store.settings, proxy_running)
    };
    println!(
        "[Switch] 执行切换...（模式={}）",
        if hot { "热切" } else { "冷切" }
    );
    if !state
        .refresh_locks
        .acquire(&target_id, tokio::time::Duration::from_secs(5))
        .await
    {
        return Err("该账号正在被其他流程刷新，请稍后重试".to_string());
    }
    let switch_result: Result<(), String> = {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        match store.switch_to(&target_id, hot) {
            Ok(()) => store.save(),
            Err(e) => Err(e),
        }
    };
    state.refresh_locks.release(&target_id).await;
    switch_result?;
    // 切号后代理的远端 token 缓存需失效
    proxy::invalidate_remote_token_cache();
    println!("[Switch] 切换完成！");

    // 记录切号日志
    {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let from_name = store
            .accounts
            .values()
            .find(|a| Some(&a.id) != store.current.as_ref() && a.last_used.is_some())
            .map(|a| a.name.clone());
        let to_name = store
            .accounts
            .get(&target_id)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let to_quota = store
            .accounts
            .get(&target_id)
            .and_then(|a| a.cached_quota.as_ref())
            .map(|q| q.five_hour_left);
        state.switch_logger.log_switch(
            from_name,
            to_name,
            switch_log::SwitchReason::Manual,
            None,
            to_quota,
        );
    }

    // 断开所有代理 WebSocket 连接，强制 Codex App 重连使用新 token
    state.ws_disconnect.notify_waiters();
    println!("[Switch] 已通知代理断开 WebSocket 连接");

    // 联动刷新托盘菜单
    crate::tray::update_tray_menu(&app);

    // solo 模式：把新的 current 推给 Server（仅归档，失败不回滚）
    push_solo_current_if_needed(state, &target_id).await;
    Ok(())
}

/// 手动一键同号：拉 Server 的 current 并在本地热切到它。
/// 无视 solo_auto_sync_current 开关，给用户"在关了自动同步后还能手工对齐"的能力。
#[tauri::command]
async fn solo_sync_current(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Option<String>, String> {
    let (mode, primary, fallback, secret) = {
        let s = state.store.lock().map_err(|e| e.to_string())?;
        (
            s.settings.remote_mode.clone(),
            s.settings.remote_server_url.clone(),
            s.settings.remote_server_url_fallback.clone(),
            s.settings.remote_shared_secret.clone(),
        )
    };
    if mode != "solo" {
        return Err("仅 solo 模式支持同号操作".to_string());
    }
    if secret.is_empty() {
        return Err("未配置共享密钥".to_string());
    }
    let base = remote_client::resolve_base_url(&primary, &fallback).await?;
    let before = { state.store.lock().ok().and_then(|s| s.current.clone()) };
    solo_try_align_current(&state.store, &app, &base, &secret).await?;
    let after = { state.store.lock().ok().and_then(|s| s.current.clone()) };
    if before == after {
        Ok(None) // 已经是 Server 的 current
    } else {
        Ok(after)
    }
}

/// 手工切号后把 current 同步推给 Server（solo + client 模式都需要）。
/// 这样 Server.current = 用户选的号，fast_auth_sync 30s 拉到的也是同一个，
/// 不会再"用户切到 X，30 秒后又被 Server 拉回 Y"。
/// fire-and-forget，不阻塞调用方；Server 不可达只记日志。
async fn push_solo_current_if_needed(state: tauri::State<'_, AppState>, new_id: &str) {
    let (mode, primary, fallback, secret) = {
        match state.store.lock() {
            Ok(s) => (
                s.settings.remote_mode.clone(),
                s.settings.remote_server_url.clone(),
                s.settings.remote_server_url_fallback.clone(),
                s.settings.remote_shared_secret.clone(),
            ),
            Err(_) => return,
        }
    };
    // solo + client 都要 push（off / server 模式没 Server 可推）
    if !matches!(mode.as_str(), "solo" | "client") || secret.is_empty() {
        return;
    }
    // client 模式 = 两端协作，让 Server 也写 disk（apply_to_disk=true）
    // solo 模式 = 本机自治，Server 仅记录 current 指针归档（apply_to_disk=false）
    let apply_to_disk = mode == "client";
    match remote_client::resolve_base_url(&primary, &fallback).await {
        Ok(base) => {
            if let Err(e) =
                remote_client::push_solo_switch(&base, &secret, new_id, apply_to_disk).await
            {
                eprintln!("[Switch] push /solo/current 失败（已本地生效）: {}", e);
            } else {
                println!(
                    "[Switch] 手工切号已同步到 Server (mode={}, apply_to_disk={})",
                    mode, apply_to_disk
                );
            }
        }
        Err(e) => eprintln!("[Switch] Server 不可达，切号未同步: {}", e),
    }
}

/// solo 模式心跳循环：固定间隔向 Server 发心跳，让 Server 知道"本机正在接管保活"。
/// 只要心跳还在滴答，Server 的 quota_refresh 循环就会让位，避免并发 refresh 撞 rotate。
/// 若 solo_auto_sync_current 打开，心跳后顺带把本机 current 对齐到 Server 的 current。
pub fn start_solo_heartbeat(
    store: std::sync::Arc<std::sync::Mutex<AccountStore>>,
    app_handle: tauri::AppHandle,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        loop {
            let (mode, primary, fallback, secret, auto_sync) = {
                let s = store.lock().unwrap();
                (
                    s.settings.remote_mode.clone(),
                    s.settings.remote_server_url.clone(),
                    s.settings.remote_server_url_fallback.clone(),
                    s.settings.remote_shared_secret.clone(),
                    s.settings.solo_auto_sync_current,
                )
            };
            if mode != "solo" {
                println!("[Solo] 模式已非 solo（={}），心跳退出", mode);
                return;
            }
            if !secret.is_empty() {
                match remote_client::resolve_base_url(&primary, &fallback).await {
                    Ok(base) => {
                        if let Err(e) = remote_client::send_solo_heartbeat(
                            &base,
                            &secret,
                            account::SOLO_HEARTBEAT_TTL_SECS,
                        )
                        .await
                        {
                            eprintln!("[Solo] 心跳失败: {}", e);
                        } else if auto_sync {
                            if let Err(e) =
                                solo_try_align_current(&store, &app_handle, &base, &secret).await
                            {
                                eprintln!("[Solo] 自动同号本轮跳过: {}", e);
                            }
                        }
                    }
                    Err(e) => eprintln!("[Solo] Server 不可达，本轮跳过: {}", e),
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(
                account::SOLO_HEARTBEAT_INTERVAL_SECS,
            ))
            .await;
        }
    })
}

/// 拉 Server 的 /current，若与本机不一致则本地切号（不回推，避免环）。
/// 代理未开时退化为冷切写 auth.json。
async fn solo_try_align_current(
    store: &std::sync::Arc<std::sync::Mutex<AccountStore>>,
    app: &tauri::AppHandle,
    base: &str,
    secret: &str,
) -> Result<(), String> {
    let cur = remote_client::get_current(base, secret).await?;
    let Some(mini_cur) = cur.current else {
        return Ok(()); // Server 没 current，不动本地
    };
    let (local_cur, mode, proxy_enabled, exists_locally) = {
        let s = store.lock().map_err(|e| e.to_string())?;
        (
            s.current.clone(),
            s.settings.remote_mode.clone(),
            s.settings.proxy_enabled,
            s.accounts.contains_key(&mini_cur),
        )
    };
    if mode != "solo" {
        return Ok(());
    }
    if local_cur.as_deref() == Some(mini_cur.as_str()) {
        return Ok(());
    }
    if !exists_locally {
        return Err(format!(
            "Server current={} 在本机不存在（可能还没 push 过账号）",
            mini_cur
        ));
    }
    let hot = {
        let s = store.lock().map_err(|e| e.to_string())?;
        account::should_hot_switch(&s.settings, proxy_enabled)
    };
    {
        let mut s = store.lock().map_err(|e| e.to_string())?;
        s.switch_to(&mini_cur, hot)?;
        s.save()?;
    }
    crate::proxy::invalidate_remote_token_cache();
    let _ = app.emit("proxy-account-switched", cur.name.unwrap_or_default());
    let _ = app.emit("accounts-updated", ());
    crate::tray::update_tray_menu(app);
    println!("[Solo] 自动同号 → {}", mini_cur);
    Ok(())
}

/// 预测下一个拟切换的账号信息 (仅基于缓存，不发起网络请求)
/// 共享评分选号算法：基于 CachedQuota 的 reset_at 时间戳和剩余额度评分
/// 返回 (account_id, account_name, score) 按得分从高到低排序
/// 启动定时额度刷新调度器
/// 拉一次 Server 上 current 账号的 token，写本机 store + ~/.codex/auth.json。
/// 同时顺带做"store/disk 不一致"的自愈：磁盘上的 sub 和 store.current 的 sub 不匹配时，
/// 用 Server 拉到的覆写。
/// 仅在 client 模式 + 配置了 secret 时生效。返回 true 表示真的写盘了，false 表示跳过/失败。
pub async fn do_one_fast_auth_sync(store: &std::sync::Arc<std::sync::Mutex<AccountStore>>) -> bool {
    let (mode, primary, fallback, secret, current_id) = {
        let s = match store.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        (
            s.settings.remote_mode.clone(),
            s.settings.remote_server_url.clone(),
            s.settings.remote_server_url_fallback.clone(),
            s.settings.remote_shared_secret.clone(),
            s.current.clone(),
        )
    };
    if mode != "client" || secret.is_empty() {
        return false;
    }
    let local_cid = current_id;

    let base = match remote_client::resolve_base_url(&primary, &fallback).await {
        Ok(b) => b,
        Err(_) => return false,
    };

    // 1) 先看 Server 的 current 是不是跟本机 store.current 一致，不一致 → 优先对齐到 Server
    let target_cid = match remote_client::get_current(&base, &secret).await {
        Ok(cur) => match cur.current {
            Some(server_cid) => {
                if local_cid.as_deref() != Some(server_cid.as_str()) {
                    println!(
                        "[FastAuthSync] Server current ({}) 与本机 ({:?}) 不一致，对齐到 Server",
                        server_cid, local_cid
                    );
                }
                Some(server_cid)
            }
            None => local_cid.clone(),
        },
        Err(_) => local_cid.clone(),
    };
    let Some(cid) = target_cid else {
        return false;
    };

    // 2) 拉 cid 的最新 token 写盘
    match remote_client::fetch_token(&base, &secret, &cid).await {
        Ok(t) => {
            let allow_disk = {
                let mut s = match store.lock() {
                    Ok(g) => g,
                    Err(_) => return false,
                };
                s.sync_account_from_auth_json(&cid, t.auth_json.clone());
                // 把本机 current 也对齐上（如果之前不一致）
                s.current = Some(cid.clone());
                let _ = s.save();
                s.should_write_disk_for(&cid)
            };
            if !allow_disk {
                println!("[FastAuthSync] 手机锚生效，跳过写 ~/.codex/auth.json（current != anchor）");
                crate::proxy::invalidate_remote_token_cache();
                return true;
            }
            // 扩展 expires_at 到 +24h，codex CLI 看到"很新鲜"就不会自己 refresh，
            // 真过期时 proxy 这边接管处理
            if let Err(e) = AccountStore::write_codex_auth_extended_expiry(&t.auth_json) {
                eprintln!("[FastAuthSync] 写 ~/.codex/auth.json 失败: {}", e);
                return false;
            }
            crate::proxy::invalidate_remote_token_cache();
            true
        }
        Err(_) => false,
    }
}

/// 快速 auth.json 同步循环（仅 client 模式）：每 30s 拉一次 Server 上 current 的最新 token
/// 并写盘到 ~/.codex/auth.json。把"Server 已轮换 RT vs 本机 auth.json 滞后"的窗口压到 30s。
/// 与 start_quota_refresh 解耦：quota 5 分钟级，token 30 秒级，互不阻塞。
pub fn start_fast_auth_sync(
    store: std::sync::Arc<std::sync::Mutex<AccountStore>>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        println!("[FastAuthSync] 快速同步循环已启动（30s，仅 client 模式生效）");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            do_one_fast_auth_sync(&store).await;
        }
    })
}

pub fn start_quota_refresh(
    store: std::sync::Arc<std::sync::Mutex<AccountStore>>,
    app_handle: tauri::AppHandle,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        println!("[QuotaRefresh] 定时额度刷新已启动");

        loop {
            let (enabled, interval_minutes, batch_size, remote_mode, primary, fallback, secret, client_owns_current) = {
                let s = store.lock().unwrap();
                (
                    s.settings.quota_refresh_enabled,
                    s.settings.quota_refresh_interval.max(1),
                    s.settings.quota_refresh_batch.max(1),
                    s.settings.remote_mode.clone(),
                    s.settings.remote_server_url.clone(),
                    s.settings.remote_server_url_fallback.clone(),
                    s.settings.remote_shared_secret.clone(),
                    s.settings.client_owns_current,
                )
            };

            // client / solo 模式：强制从 Server 拉 /quotas，忽略 enabled 开关；
            // 下方 fall-through 的本地 batch refresh（含 oauth::refresh_access_token）
            // 在这两种模式下都不能跑（rt 是 Server 的权威）。solo 同样进这条 Server-sync
            // 路径，但**不跟随 Server 的 current 指针**（solo 本机自己管 current）。
            // server/off 模式：遵循 enabled 开关，走下方 batch refresh。
            let is_client_or_solo = matches!(remote_mode.as_str(), "client" | "solo");
            if is_client_or_solo {
                if secret.is_empty() {
                    println!(
                        "[QuotaRefresh] {} 模式但未配置 secret，跳过本轮",
                        remote_mode
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(
                        u64::from(interval_minutes) * 60,
                    ))
                    .await;
                    continue;
                }
                match crate::remote_client::resolve_base_url(&primary, &fallback).await {
                    Ok(base) => {
                        match crate::remote_client::fetch_all_quota(&base, &secret).await {
                            Ok(entries) => {
                                let mut remote_ids: std::collections::HashSet<String> =
                                    entries.iter().map(|e| e.id.clone()).collect();

                                // 1.5) 后台补推：本机有 / Server 没有的账号，先尝试推一次到 Server。
                                //      场景：OAuth 登录瞬间 push 失败（Mini Mac 短暂 502 / 网络抖动）
                                //      → quota_refresh 自动补推，永不丢号。推成功的 id 加入 remote_ids，
                                //      下一步 prune 自动跳过。
                                //      只对 client 模式下、本地有 refresh_token 或 access_token 的账号 push；
                                //      Relay 账号也参与（它们靠 access_token 进 auth_json.tokens）。
                                let push_candidates: Vec<crate::account::Account> = {
                                    if let Ok(s) = store.lock() {
                                        s.accounts
                                            .values()
                                            .filter(|a| !remote_ids.contains(&a.id))
                                            .cloned()
                                            .collect()
                                    } else {
                                        Vec::new()
                                    }
                                };
                                let mut pushed_now = 0usize;
                                for cand in push_candidates {
                                    match crate::remote_client::upsert_account(
                                        &base, &secret, &cand,
                                    )
                                    .await
                                    {
                                        Ok(outcome) => {
                                            println!(
                                                "[QuotaRefresh] 后台补推成功: id={} name={} action={}",
                                                cand.id, cand.name, outcome.upserted
                                            );
                                            remote_ids.insert(cand.id.clone());
                                            if outcome.upserted == "merged"
                                                && outcome.id != cand.id
                                            {
                                                remote_ids.insert(outcome.id.clone());
                                            }
                                            pushed_now += 1;
                                        }
                                        Err(e) => {
                                            // 不丢，由 10 分钟宽限期保护；下个 5 分钟周期继续重试
                                            eprintln!(
                                                "[QuotaRefresh] 后台补推失败 id={} name={}: {}",
                                                cand.id, cand.name, e
                                            );
                                        }
                                    }
                                }
                                if pushed_now > 0 {
                                    println!(
                                        "[QuotaRefresh] 后台补推完成: {} 条新账号同步到 Server",
                                        pushed_now
                                    );
                                }

                                let (updated, pruned) = {
                                    let mut updated = 0usize;
                                    let mut pruned = 0usize;
                                    if let Ok(mut s) = store.lock() {
                                        // 1) 同步 quota/封禁/失效状态
                                        //    Relay 账号的 is_token_invalid / is_logged_out 不
                                        //    跟 Server 同步——Relay 用静态 API Key，本地是权威；
                                        //    Server 上的 stale flag（比如旧版本误标过的）不应
                                        //    污染本地。is_banned 也同样跳过（Relay 没有"封号"概念）。
                                        for e in &entries {
                                            if let Some(acc) = s.accounts.get_mut(&e.id) {
                                                if let Some(q) = e.cached_quota.clone() {
                                                    acc.cached_quota = Some(q);
                                                    updated += 1;
                                                }
                                                if !acc.is_relay() {
                                                    acc.is_banned = e.is_banned;
                                                    acc.is_token_invalid = e.is_token_invalid;
                                                    acc.is_logged_out = e.is_logged_out;
                                                }
                                            }
                                        }
                                        // 2) 删除 Server 上已不存在的账号（多端删号同步）
                                        // Relay 账号也参与 prune：add_relay_account 现在会 upsert 到 Server，
                                        // Server 上有这账号就不会被 prune。
                                        //
                                        // 但是新加的账号给 10 分钟宽限期：登录成功瞬间 push_account_to_server
                                        // 失败（例如 Server 502 / 短暂网络抖动）的情况下，原逻辑会在下一次
                                        // quota_refresh 5 分钟周期到时把这个本地账号悄悄删掉，用户视角是
                                        // "刚登的号过一会就不见了"。宽限窗内不 prune，给后续 push 重试机会，
                                        // 也给用户手动重推留时间。
                                        let now = chrono::Utc::now();
                                        let grace = chrono::Duration::minutes(10);
                                        let local_ids: Vec<String> =
                                            s.accounts.keys().cloned().collect();
                                        for id in local_ids {
                                            if remote_ids.contains(&id) {
                                                continue;
                                            }
                                            // 宽限：created_at 在 10 分钟内的不 prune
                                            if let Some(acc) = s.accounts.get(&id) {
                                                let age = now.signed_duration_since(acc.created_at);
                                                if age < grace {
                                                    println!(
                                                        "[QuotaRefresh] 跳过 prune（{}s 宽限期内）: id={} name={}",
                                                        age.num_seconds(),
                                                        id,
                                                        acc.name
                                                    );
                                                    continue;
                                                }
                                            }
                                            s.accounts.remove(&id);
                                            pruned += 1;
                                            if s.current.as_deref() == Some(id.as_str()) {
                                                s.current = None;
                                            }
                                        }
                                        let _ = s.save();
                                    }
                                    (updated, pruned)
                                };
                                // 3) 同步 Server 的 current 到本机（仅 client 模式）
                                //    - 拉 Server 最新 token
                                //    - 写本机 store（accounts.json） + 官方 ~/.codex/auth.json
                                //    - 更新本机 current 指针
                                //    规则：若 Server 正常，client 始终跟随 Server 的 current。
                                //    client_owns_current=true（旧 solo 迁过来的）：本机 codex
                                //    直接跑，current 由本机用户决定，不被 Server 反向同步。
                                if remote_mode == "client" && !client_owns_current {
                                  if let Ok(cur) =
                                    crate::remote_client::get_current(&base, &secret).await
                                {
                                    if let Some(cid) = cur.current.clone() {
                                        let exists_locally = {
                                            if let Ok(s) = store.lock() {
                                                s.accounts.contains_key(&cid)
                                            } else {
                                                false
                                            }
                                        };
                                        if exists_locally {
                                            match crate::remote_client::fetch_token(
                                                &base, &secret, &cid,
                                            )
                                            .await
                                            {
                                                Ok(t) => {
                                                    let allow_disk = if let Ok(mut s) = store.lock() {
                                                        s.sync_account_from_auth_json(
                                                            &cid,
                                                            t.auth_json.clone(),
                                                        );
                                                        let prev_current = s.current.clone();
                                                        s.current = Some(cid.clone());
                                                        let _ = s.save();
                                                        if prev_current.as_deref()
                                                            != Some(cid.as_str())
                                                        {
                                                            println!(
                                                                "[QuotaRefresh] client 对齐 current → {}",
                                                                cur.name.clone().unwrap_or_default()
                                                            );
                                                        }
                                                        s.should_write_disk_for(&cid)
                                                    } else {
                                                        true
                                                    };
                                                    if !allow_disk {
                                                        println!(
                                                            "[QuotaRefresh] 手机锚生效，跳过写 ~/.codex/auth.json（{} != anchor）",
                                                            cur.name.clone().unwrap_or_default()
                                                        );
                                                    } else if let Err(e) =
                                                        account::AccountStore::write_codex_auth_extended_expiry(
                                                            &t.auth_json,
                                                        )
                                                    {
                                                        // client 模式：用 extended_expiry 防 codex 自刷
                                                        eprintln!(
                                                            "[QuotaRefresh] 写 ~/.codex/auth.json 失败: {}",
                                                            e
                                                        );
                                                    } else {
                                                        println!(
                                                            "[QuotaRefresh] client 已写 ~/.codex/auth.json（{}）",
                                                            cur.name.unwrap_or_default()
                                                        );
                                                    }
                                                    // 切号后清掉 proxy 端的远端 token 缓存
                                                    crate::proxy::invalidate_remote_token_cache();
                                                }
                                                Err(e) => {
                                                    eprintln!(
                                                        "[QuotaRefresh] 拉 Server current token 失败: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                } // end: if remote_mode == "client" && !client_owns_current
                                println!(
                                    "[QuotaRefresh] {} 从 Server 同步 {} 个额度，删除本地残留 {} 个",
                                    remote_mode, updated, pruned
                                );
                                let _ = app_handle.emit("accounts-updated", ());
                            }
                            Err(e) => println!("[QuotaRefresh] client 拉取 /quotas 失败: {}", e),
                        }
                    }
                    Err(e) => println!("[QuotaRefresh] client Server 不可达: {}", e),
                }
                let sync_minutes = u64::from(interval_minutes.max(5)); // client 模式最少 5 分钟
                tokio::time::sleep(tokio::time::Duration::from_secs(sync_minutes * 60)).await;
                continue;
            }

            // 非 client 模式才尊重 enabled 开关
            if !enabled {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                continue;
            }

            // Server（server 模式）若检测到有活跃 solo client，让位：跳过本轮保活，避免
            // 双端并发 refresh 同一账号的 refresh_token 造成 rotate 冲突。
            if remote_mode == "server" && crate::remote_server::solo_is_active() {
                println!("[QuotaRefresh] 检测到活跃 solo client，跳过本轮（让位）");
                tokio::time::sleep(tokio::time::Duration::from_secs(
                    u64::from(interval_minutes) * 60,
                ))
                .await;
                continue;
            }

            // 按 cached_quota.updated_at 排序，最旧的优先；但「窗口已 reset 但缓存还停留在
            // reset 前」的账号是高优：默认 batch=1 时它们会被 N×interval 才轮到一次，
            // 用户视角是「到点了也不刷新」。这里把 expired 账号集合优先取，再用普通
            // batch 兜底，保证窗口刚 reset 的账号最迟下一个 5min 周期就被刷上。
            // Relay 账号的 quota 通过专属 fetcher 拉取，这里跳过避免无谓打 OpenAI usage API
            let targets: Vec<(String, String)> = {
                let s = store.lock().unwrap();
                let now_ts = chrono::Utc::now().timestamp();
                let candidates: Vec<_> = s
                    .accounts
                    .values()
                    .filter(|a| {
                        !a.is_banned && !a.is_token_invalid && !a.is_logged_out && !a.is_relay()
                    })
                    .map(|a| {
                        let cq = a.cached_quota.as_ref();
                        let updated = cq
                            .map(|q| q.updated_at)
                            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
                        let five_reset = cq.and_then(|q| q.five_hour_reset_at).unwrap_or(0);
                        let weekly_reset = cq.and_then(|q| q.weekly_reset_at).unwrap_or(0);
                        let updated_ts = updated.timestamp();
                        // expired = reset_at 已过 AND 缓存抓取早于 reset_at
                        let five_expired = five_reset > 0
                            && five_reset <= now_ts
                            && updated_ts < five_reset;
                        let weekly_expired = weekly_reset > 0
                            && weekly_reset <= now_ts
                            && updated_ts < weekly_reset;
                        let expired = five_expired || weekly_expired;
                        (a.id.clone(), a.name.clone(), updated, expired)
                    })
                    .collect();

                // 1) expired 全部入选（不受 batch_size 限制 — reset 后必须及时刷）
                let mut expired: Vec<_> =
                    candidates.iter().filter(|(_, _, _, e)| *e).cloned().collect();
                expired.sort_by_key(|(_, _, t, _)| *t);

                // 2) 剩下的按 updated_at 最旧排序，取 batch_size 个
                let mut rest: Vec<_> = candidates
                    .iter()
                    .filter(|(_, _, _, e)| !*e)
                    .cloned()
                    .collect();
                rest.sort_by_key(|(_, _, t, _)| *t);

                if !expired.is_empty() {
                    println!(
                        "[QuotaRefresh] 检测到 {} 个 window 已 reset 的账号，优先刷新",
                        expired.len()
                    );
                }

                expired
                    .into_iter()
                    .chain(rest.into_iter().take(batch_size as usize))
                    .map(|(id, name, _, _)| (id, name))
                    .collect()
            };

            for (id, name) in &targets {
                println!("[QuotaRefresh] 刷新 {} ...", name);

                let (at, aid, rt) = {
                    let s = store.lock().unwrap();
                    let acc = match s.accounts.get(id) {
                        Some(a) => a,
                        None => continue,
                    };
                    (
                        AccountStore::extract_access_token(&acc.auth_json),
                        AccountStore::extract_account_id(&acc.auth_json),
                        acc.refresh_token.clone(),
                    )
                };

                // 没有 access_token 先用 refresh_token 换
                let access_token = match at {
                    Some(t) => t,
                    None => {
                        if let Some(ref rt_val) = rt {
                            match crate::oauth::refresh_access_token_locked(id, rt_val).await {
                                Ok(res) => {
                                    if let Ok(mut s) = store.lock() {
                                        if let Some(acc) = s.accounts.get_mut(id) {
                                            AccountStore::apply_refreshed_tokens(
                                                acc,
                                                res.access_token.clone(),
                                                res.refresh_token.clone(),
                                                res.id_token,
                                                res.expires_in,
                                            );
                                            let _ = s.save();
                                        }
                                    }
                                    res.access_token
                                }
                                Err(e) => {
                                    println!("[QuotaRefresh] {} token 刷新失败: {}", name, e);
                                    continue;
                                }
                            }
                        } else {
                            continue;
                        }
                    }
                };

                match usage::UsageFetcher::fetch_usage_direct(access_token, aid, rt, false).await {
                    Ok((usage, _)) => {
                        let email_for_snap = if let Ok(s) = store.lock() {
                            s.accounts
                                .get(id)
                                .and_then(|a| AccountStore::extract_email(&a.auth_json))
                                .unwrap_or_else(|| name.clone())
                        } else {
                            name.clone()
                        };
                        // 写 quota 快照，让"每号 Token 历史"的估算上限有数据可用
                        quota_snapshot::append_from_usage(
                            id,
                            &email_for_snap,
                            &usage,
                            "quota_refresh",
                        );
                        if let Ok(mut s) = store.lock() {
                            if let Some(acc) = s.accounts.get_mut(id) {
                                acc.cached_quota = Some(account::CachedQuota {
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
                                    updated_at: chrono::Utc::now(),
                                });
                                // quota 拉到了 = token 没过期，清掉历史 stale 失效标记
                                acc.is_token_invalid = false;
                                acc.is_logged_out = false;
                                let _ = s.save();
                            }
                        }
                        println!(
                            "[QuotaRefresh] {} → 5h:{}% 周:{}%",
                            name, usage.five_hour_left, usage.weekly_left
                        );

                        // 记录自动刷新额度日志
                        use tauri::Manager;
                        if let Some(logger) = app_handle
                            .try_state::<std::sync::Arc<crate::switch_log::SwitchLogger>>()
                        {
                            logger.inner().log_switch(
                                None,
                                name.clone(),
                                crate::switch_log::SwitchReason::AutoQuotaRefresh,
                                None,
                                Some(usage.five_hour_left as f64),
                            );
                        }

                        let _ = app_handle.emit("accounts-updated", ());
                    }
                    Err(e) => {
                        println!("[QuotaRefresh] {} 额度查询失败: {}", name, e);
                        // 封号/失效标记
                        if e.contains("ACCOUNT_BANNED") {
                            if let Ok(mut s) = store.lock() {
                                if let Some(acc) = s.accounts.get_mut(id) {
                                    acc.is_banned = true;
                                    let _ = s.save();
                                }
                            }
                        } else if e.contains("TOKEN_INVALID") {
                            if let Ok(mut s) = store.lock() {
                                if let Some(acc) = s.accounts.get_mut(id) {
                                    acc.is_token_invalid = true;
                                    acc.is_logged_out = false;
                                    let _ = s.save();
                                }
                            }
                        }
                    }
                }

                // 仅在账号之间隔一个温和的间隔（500ms），不再卡 interval_minutes —
                // 之前是每个账号之间 5 分钟，10 个账号要 50 分钟，等于「窗口 reset 后
                // 半小时配额还没刷出来」。整轮 batch 跑完之后才睡 interval_minutes。
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            // 整轮跑完后再 sleep 到下一周期（没有目标的时候缩短到 60s）
            let next_sleep_secs = if targets.is_empty() {
                60
            } else {
                u64::from(interval_minutes) * 60
            };
            tokio::time::sleep(tokio::time::Duration::from_secs(next_sleep_secs)).await;
        }
    })
}

pub fn score_candidate_accounts(store: &AccountStore) -> Vec<(String, String, f64)> {
    let current_id = store.current.as_deref().unwrap_or("");
    let allow_free = store.settings.allow_auto_switch_to_free;
    let allow_switch_in_relay = store.settings.relay_auto_switch_in;
    let now = chrono::Utc::now().timestamp();

    let mut scored: Vec<(String, String, f64)> = Vec::new();

    for account in store.accounts.values() {
        if account.id == current_id
            || account.is_banned
            || account.is_token_invalid
            || account.is_logged_out
        {
            continue;
        }
        // 默认不"切到 Relay"：自动选号跳过 Relay 候选
        if !allow_switch_in_relay && account.is_relay() {
            continue;
        }

        let score = match &account.cached_quota {
            None => 50.0,
            Some(q) => {
                let plan = q.plan_type.to_lowercase();
                let is_free = plan == "free" || plan == "unknown";

                if is_free && !allow_free {
                    continue;
                }

                // Plan 优先级加分：pro > plus/team > free
                let plan_bonus = match plan.as_str() {
                    "pro" => 30.0,
                    "plus" | "team" | "enterprise" => 20.0,
                    "edu" | "business" => 15.0,
                    "free" | "unknown" => 0.0,
                    _ => 10.0,
                };

                // 5h 可用度
                let five_h = if q.five_hour_left <= 0.0 {
                    match q.five_hour_reset_at {
                        Some(reset_at) if now >= reset_at => 50.0,
                        _ => 0.0,
                    }
                } else {
                    q.five_hour_left
                };

                // 周可用度
                let weekly = if q.weekly_left <= 0.0 {
                    match q.weekly_reset_at {
                        Some(reset_at) if now >= reset_at => 50.0,
                        _ => 0.0,
                    }
                } else {
                    q.weekly_left
                };

                let effective = if is_free { five_h } else { five_h.min(weekly) };
                if effective <= 0.0 {
                    continue;
                }
                // 最终评分 = 额度分 + Plan 加分
                effective + plan_bonus
            }
        };

        scored.push((account.id.clone(), account.name.clone(), score));
    }

    // 按得分从高到低排序
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// 预测下一个最优账号（tray 菜单预览）
pub fn predict_next_account_internal(state: tauri::State<'_, AppState>) -> Option<(String, i32)> {
    let store = state.store.lock().ok()?;
    let candidates = score_candidate_accounts(&store);
    candidates
        .first()
        .map(|(_, name, score)| (name.clone(), *score as i32))
}

/// 智能切号：选最优账号并切换
pub async fn switch_to_next_account_internal(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // 1. 用评分算法选出最优候选
    let candidates = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        score_candidate_accounts(&store)
    };

    if candidates.is_empty() {
        return Err("没有可用账号".to_string());
    }

    // 2. 按得分从高到低尝试，查 API 确认额度后切换
    for (target_id, target_name, score) in &candidates {
        println!("[SmartSwitch] 候选: {} (评分 {:.0})", target_name, score);

        // Relay 类型不走 OpenAI usage API（中转站不支持），直接接受候选
        let is_relay = state
            .store
            .lock()
            .ok()
            .and_then(|s| s.accounts.get(target_id).map(|a| a.is_relay()))
            .unwrap_or(false);
        if is_relay {
            println!(
                "[SmartSwitch] Relay 类型，跳过 quota 检查直接切换: {}",
                target_name
            );
            return switch_account(state, app.clone(), target_id.clone()).await;
        }

        // 查 API 确认最新额度
        let quota = match get_quota_internal(&state, target_id.clone()).await {
            Ok(u) => u,
            Err(e) => {
                // 封号/失效/登出检测
                if e.contains("ACCOUNT_BANNED")
                    || e.contains("TOKEN_INVALID")
                    || e.contains("ACCOUNT_LOGGED_OUT")
                {
                    println!("[SmartSwitch] 账号 {} 已封禁/失效/登出，跳过", target_name);
                    continue;
                }
                println!(
                    "[SmartSwitch] 账号 {} 额度查询失败: {}，跳过",
                    target_name, e
                );
                continue;
            }
        };

        let plan = quota.plan_type.to_lowercase();
        let is_free = plan == "free" || plan == "unknown";

        let has_quota = if is_free {
            quota.five_hour_left > 0
        } else {
            quota.five_hour_left > 0 && quota.weekly_left > 0
        };

        if has_quota {
            println!(
                "[SmartSwitch] 选中最优账号: {} ({}, 5h={}%, 周={}%)",
                target_name, quota.plan_type, quota.five_hour_left, quota.weekly_left
            );
            return switch_account(state, app.clone(), target_id.clone()).await;
        } else {
            println!("[SmartSwitch] 账号 {} 额度已耗尽，继续找", target_name);
        }
    }

    Err("遍历完所有账号，未发现可用配额的账号".to_string())
}

/// 内部辅助：获取额度数据
async fn get_quota_internal(state: &AppState, id: String) -> Result<UsageDisplay, String> {
    // Relay 账号没有 OpenAI 5h+周窗口模型；上层应改用 refresh_relay_usage
    {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(acc) = store.accounts.get(&id) {
            if acc.is_relay() {
                return Err(
                    "RELAY_ACCOUNT:中转站账号不支持 OpenAI usage 查询，请用「中转站余额刷新」"
                        .to_string(),
                );
            }
        }
    }
    let (access_token, account_id, refresh_token, is_client_or_solo) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store.accounts.get(&id).ok_or("账号不存在")?;
        let at = AccountStore::extract_access_token(&account.auth_json);
        let aid = AccountStore::extract_account_id(&account.auth_json);
        let rt = account.refresh_token.clone();
        let solo_or_client =
            matches!(store.settings.remote_mode.as_str(), "client" | "solo");
        (at, aid, rt, solo_or_client)
    };

    // 如果没有 access_token，先用 refresh_token 换一个
    // client/solo 模式：rt 是 Server 的权威，本机不刷 —— 没有 at 就直接报错给上层
    // （上层若是 remote_refresh_account_quota，会走 Server fetch_token；若是 fallback
    // 走到本地，说明 Server 也没这个号，那只能等用户重登或重推到 Server）。
    let access_token = if let Some(at) = access_token {
        at
    } else if is_client_or_solo {
        return Err(
            "TOKEN_INVALID:client/solo 模式禁止本机 rt 刷新；请确认账号已同步到 Server".to_string(),
        );
    } else if let Some(ref rt) = refresh_token {
        match crate::oauth::refresh_access_token_locked(&id, rt).await {
            Ok(token_res) => {
                // 保存新 token
                let mut store = state.store.lock().map_err(|e| e.to_string())?;
                if let Some(account) = store.accounts.get_mut(&id) {
                    AccountStore::apply_refreshed_tokens(
                        account,
                        token_res.access_token.clone(),
                        token_res.refresh_token.clone(),
                        token_res.id_token,
                        token_res.expires_in,
                    );
                    if let Err(e) = store.save() {
                        eprintln!("[Store] 保存失败: {}", e);
                    }
                }
                token_res.access_token
            }
            Err(e) => return Err(format!("TOKEN_INVALID:刷新 token 失败: {}", e)),
        }
    } else {
        return Err("TOKEN_INVALID:无 access_token 且无 refresh_token".to_string());
    };

    // client/solo 模式禁止 fetch_usage_direct 内部本地 rt 刷新（usage.rs:102）
    let result =
        UsageFetcher::fetch_usage_direct(access_token, account_id, refresh_token, !is_client_or_solo).await;

    // 检测封号/失效：分开标记
    if let Err(ref e) = result {
        if e.contains("ACCOUNT_BANNED") {
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            if let Some(account) = store.accounts.get_mut(&id) {
                account.is_banned = true;
                account.is_token_invalid = false;
                account.is_logged_out = false;
                if let Err(e) = store.save() {
                    eprintln!("[Store] 保存失败: {}", e);
                }
            }
            return Err(e.clone());
        }
        if e.contains("TOKEN_INVALID") {
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            if let Some(account) = store.accounts.get_mut(&id) {
                account.is_token_invalid = true;
                account.is_banned = false;
                account.is_logged_out = false;
                if let Err(e) = store.save() {
                    eprintln!("[Store] 保存失败: {}", e);
                }
            }
            return Err(e.clone());
        }
        if e.contains("ACCOUNT_LOGGED_OUT") {
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            if let Some(account) = store.accounts.get_mut(&id) {
                account.is_logged_out = true;
                account.is_banned = false;
                account.is_token_invalid = false;
                if let Err(e) = store.save() {
                    eprintln!("[Store] 保存失败: {}", e);
                }
            }
            return Err(e.clone());
        }
    }

    let (display, new_tokens) = result?;

    // 如果产生了新 Token，保存
    if let Some(res) = new_tokens {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(account) = store.accounts.get_mut(&id) {
            AccountStore::apply_refreshed_tokens(
                account,
                res.access_token,
                res.refresh_token,
                res.id_token,
                res.expires_in,
            );
            if let Err(e) = store.save() {
                eprintln!("[Store] 保存失败: {}", e);
            }
        }
    }

    // 更新缓存
    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(account) = store.accounts.get_mut(&id) {
            account.cached_quota = Some(usage_to_cached(&display));
            // quota 拉到了 = token 没过期，清掉历史 stale 失效标记
            account.is_token_invalid = false;
            account.is_logged_out = false;
            if let Err(e) = store.save() {
                eprintln!("[Store] 保存失败: {}", e);
            }
        }
    }

    Ok(display)
}

fn usage_to_cached(u: &UsageDisplay) -> crate::account::CachedQuota {
    crate::account::CachedQuota {
        five_hour_left: u.five_hour_left as f64,
        five_hour_reset: u.five_hour_reset.clone(),
        five_hour_reset_at: u.five_hour_reset_at,
        five_hour_label: u.five_hour_label.clone(),
        weekly_left: u.weekly_left as f64,
        weekly_reset: u.weekly_reset.clone(),
        weekly_reset_at: u.weekly_reset_at,
        weekly_label: u.weekly_label.clone(),
        plan_type: u.plan_type.clone(),
        is_valid_for_cli: u.is_valid_for_cli,
        reset_credits: u.reset_credits,
        updated_at: Utc::now(),
    }
}

/// 发送 Codex 推荐邀请（POST /backend-api/wham/referrals/invite）。
/// 复用账号自身 token，走 quota 同一条出口（Pro 号同 IP 约束见 usage::send_referral_invite）。
#[tauri::command]
async fn send_codex_invite(
    state: tauri::State<'_, AppState>,
    id: String,
    emails: Vec<String>,
) -> Result<usage::InviteResult, String> {
    // 清洗邮箱：trim、去空、去重（大小写无关）
    let mut seen = std::collections::HashSet::new();
    let emails: Vec<String> = emails
        .into_iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .filter(|e| seen.insert(e.to_lowercase()))
        .collect();
    if emails.is_empty() {
        return Err("至少需要 1 个邀请邮箱".to_string());
    }
    if emails.len() > 50 {
        return Err(format!("邀请邮箱过多：{} 个，最多 50 个", emails.len()));
    }

    // 取该账号 token / account_id（逻辑与 get_quota_by_id 一致）
    let (access_token_opt, account_id, refresh_token, is_client_or_solo) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store
            .accounts
            .get(&id)
            .ok_or_else(|| format!("账号 {} 不存在", id))?;
        if account.is_relay() {
            return Err("RELAY_ACCOUNT:中转站账号不支持 Codex 邀请".to_string());
        }
        let at = AccountStore::extract_access_token(&account.auth_json);
        let aid = AccountStore::extract_account_id(&account.auth_json);
        let rt = account
            .refresh_token
            .clone()
            .or_else(|| AccountStore::extract_refresh_token(&account.auth_json));
        let solo_or_client = matches!(store.settings.remote_mode.as_str(), "client" | "solo");
        (at, aid, rt, solo_or_client)
    };

    let access_token = if let Some(at) = access_token_opt {
        at
    } else if is_client_or_solo {
        return Err(
            "TOKEN_INVALID:client/solo 模式禁止本机 rt 刷新；请确认账号已同步到 Server".to_string(),
        );
    } else if let Some(ref rt) = refresh_token {
        match crate::oauth::refresh_access_token_locked(&id, rt).await {
            Ok(token_res) => {
                let mut store = state.store.lock().map_err(|e| e.to_string())?;
                if let Some(account) = store.accounts.get_mut(&id) {
                    AccountStore::apply_refreshed_tokens(
                        account,
                        token_res.access_token.clone(),
                        token_res.refresh_token.clone(),
                        token_res.id_token,
                        token_res.expires_in,
                    );
                    let _ = store.save();
                }
                token_res.access_token
            }
            Err(e) => return Err(format!("TOKEN_INVALID:刷新 token 失败: {}", e)),
        }
    } else {
        return Err("TOKEN_INVALID:无 access_token 且无 refresh_token".to_string());
    };

    usage::send_referral_invite(&access_token, account_id.as_deref(), &emails, None).await
}

/// 唤醒/账户检测：用账号 token 模拟 codex CLI 发一句「你好」，
/// 200=活着且回话 / 429=配额耗尽 / 403=需验证 / 401=失效。
#[tauri::command]
async fn send_codex_wakeup(
    state: tauri::State<'_, AppState>,
    id: String,
    prompt: Option<String>,
) -> Result<usage::WakeupResult, String> {
    let prompt = prompt.unwrap_or_else(|| "你好".to_string());

    let (access_token_opt, account_id, refresh_token, is_client_or_solo) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store
            .accounts
            .get(&id)
            .ok_or_else(|| format!("账号 {} 不存在", id))?;
        if account.is_relay() {
            return Err("RELAY_ACCOUNT:中转站账号不支持 Codex 唤醒".to_string());
        }
        let at = AccountStore::extract_access_token(&account.auth_json);
        let aid = AccountStore::extract_account_id(&account.auth_json);
        let rt = account
            .refresh_token
            .clone()
            .or_else(|| AccountStore::extract_refresh_token(&account.auth_json));
        let solo_or_client = matches!(store.settings.remote_mode.as_str(), "client" | "solo");
        (at, aid, rt, solo_or_client)
    };

    let access_token = if let Some(at) = access_token_opt {
        at
    } else if is_client_or_solo {
        return Err(
            "TOKEN_INVALID:client/solo 模式禁止本机 rt 刷新；请确认账号已同步到 Server".to_string(),
        );
    } else if let Some(ref rt) = refresh_token {
        match crate::oauth::refresh_access_token_locked(&id, rt).await {
            Ok(token_res) => {
                let mut store = state.store.lock().map_err(|e| e.to_string())?;
                if let Some(account) = store.accounts.get_mut(&id) {
                    AccountStore::apply_refreshed_tokens(
                        account,
                        token_res.access_token.clone(),
                        token_res.refresh_token.clone(),
                        token_res.id_token,
                        token_res.expires_in,
                    );
                    let _ = store.save();
                }
                token_res.access_token
            }
            Err(e) => return Err(format!("TOKEN_INVALID:刷新 token 失败: {}", e)),
        }
    } else {
        return Err("TOKEN_INVALID:无 access_token 且无 refresh_token".to_string());
    };

    usage::send_wakeup(
        &access_token,
        account_id.as_deref(),
        &prompt,
        usage::DEFAULT_WAKEUP_MODEL,
    )
    .await
}

/// 用指定账号在「隔离 CODEX_HOME + 直连 OpenAI」下打开一个交互式 codex 终端。
///
/// 用途：referral 兑现需要"真 codex CLI 的 turn"——手搓 API 不算数。这里给该号
/// 单独建一个 CODEX_HOME（塞它自己的 auth.json + 默认 openai provider 直连，
/// **不走 codex-switcher proxy**，避免被改写成激活账号），再开 Terminal 跑 codex，
/// 你在里面发一句"你好"即可触发兑现（前提：该号已在网页完成 referral 接受）。
///
/// 注意：codex 启动会刷新并轮换 refresh_token，本号在 store/Server 里的副本会变旧。
/// 薅 referral 不依赖回写（奖励照样到账），但该 free 号之后可能因 token 失步被吊销——
/// 一次性号可不管。
#[tauri::command]
fn open_codex_terminal(state: State<AppState>, id: String) -> Result<String, String> {
    let (auth_json, name) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let acc = store
            .accounts
            .get(&id)
            .ok_or_else(|| format!("账号 {} 不存在", id))?;
        if acc.is_relay() {
            return Err("RELAY_ACCOUNT:中转站账号不支持 codex 终端".to_string());
        }
        (acc.auth_json.clone(), acc.name.clone())
    };

    // auth_json 已是 codex 格式（{last_refresh, tokens:{...}}），补上 OPENAI_API_KEY: null
    let mut auth = auth_json;
    if !auth.is_object() {
        return Err("账号 auth_json 结构异常".to_string());
    }
    if auth.get("tokens").and_then(|t| t.get("access_token")).is_none() {
        return Err("账号缺少 codex tokens，无法启动".to_string());
    }
    if let Some(obj) = auth.as_object_mut() {
        obj.entry("OPENAI_API_KEY".to_string())
            .or_insert(serde_json::Value::Null);
    }

    // 隔离 CODEX_HOME：~/.codex-switcher/codex-launch/<id>/
    let home = dirs::home_dir()
        .ok_or("无法获取用户目录")?
        .join(".codex-switcher")
        .join("codex-launch")
        .join(&id);
    std::fs::create_dir_all(&home).map_err(|e| format!("创建 CODEX_HOME 失败: {}", e))?;
    std::fs::write(
        home.join("auth.json"),
        serde_json::to_string(&auth).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写 auth.json 失败: {}", e))?;
    // 默认 openai provider（直连 chatgpt.com），不设 model_provider = 不走 switcher proxy
    let config = "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"low\"\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\n";
    std::fs::write(home.join("config.toml"), config)
        .map_err(|e| format!("写 config.toml 失败: {}", e))?;

    // 开 Terminal.app 跑 codex（登录 shell 有 PATH，能找到 codex）
    let home_str = home.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let inner = format!(
        "export CODEX_HOME=\\\"{}\\\"; clear; echo '账号: {} — 在下面直接发一句 你好 即可触发 referral 兑现'; codex",
        home_str,
        name.replace('\'', "")
    );
    let script = format!("tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell", inner);
    Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn()
        .map_err(|e| format!("打开 Terminal 失败: {}", e))?;

    Ok(format!("已为 {} 打开 codex 终端", name))
}

/// 将当前 Codex auth.json 强制同步到指定账号
#[tauri::command]
fn sync_current_auth_to_account(state: State<AppState>, id: String) -> Result<(), String> {
    let auth_json = AccountStore::read_codex_auth()?;
    let mut store = state.store.lock().map_err(|e| e.to_string())?;
    if store.sync_account_from_auth_json(&id, auth_json) {
        store.save()?;
        return Ok(());
    }
    Err("同步失败：账号不存在或 User ID 不匹配".to_string())
}

/// 检查 Codex 是否已登录
#[tauri::command]
fn check_codex_login() -> Result<bool, String> {
    Ok(AccountStore::codex_auth_path().exists())
}

/// 获取指定账号的用量信息（不切换账号）
#[tauri::command]
async fn get_quota_by_id(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    id: String,
) -> Result<UsageDisplay, String> {
    // Relay 账号：不走 OpenAI usage 路径
    {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(acc) = store.accounts.get(&id) {
            if acc.is_relay() {
                return Err(
                    "RELAY_ACCOUNT:中转站账号请用「中转站余额刷新」，不是 OpenAI usage".to_string(),
                );
            }
        }
    }

    // 当前激活账号：先按 ~/.codex/auth.json 做身份校验与同步，再继续走 API 查询配额
    let is_current = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store.current.as_deref() == Some(id.as_str())
    };

    if is_current {
        // 手机锚模式（v0.7+）：disk 故意锁在 anchor 上，跟 current 身份不匹配是 BY DESIGN，
        // 不该把这种状态当冲突。anchor != current 时整段 disk 校验/反向同步直接跳过 ——
        // proxy 会按 store.current 的 token 路由 quota 请求，store 内本来就是权威值。
        let anchor_owns_disk = {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            store
                .session_anchor_id()
                .map(|aid| aid != id)
                .unwrap_or(false)
        };

        if anchor_owns_disk {
            println!(
                "[Quota] 手机锚生效，disk 归 anchor，跳过 current({}) 的 disk 一致性校验",
                id
            );
        } else {
            let official_auth = AccountStore::read_codex_auth()?;
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            let local_auth = store
                .accounts
                .get(&id)
                .ok_or_else(|| format!("账号 {} 不存在", id))?
                .auth_json
                .clone();

            if !AccountStore::auth_identity_matches(&local_auth, &official_auth) {
                return Err(
                    "当前激活账号与 ~/.codex/auth.json 身份不匹配，已拒绝覆盖，请先在 Codex 中切回同一账号".to_string(),
                );
            }

            if local_auth != official_auth {
                println!(
                    "[Quota] 当前激活账号 {}：检测到官方 auth.json 变更，按权威源同步。",
                    id
                );
                if store.sync_account_from_auth_json(&id, official_auth) {
                    store.save()?;
                }
            } else {
                println!("[Quota] 当前激活账号 {}：已与官方 auth.json 保持一致。", id);
            }
        }
    }

    // 1. 从 Store 获取该账号的 Token
    let (access_token_opt, account_id, refresh_token, is_client_or_solo) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store
            .accounts
            .get(&id)
            .ok_or_else(|| format!("账号 {} 不存在", id))?;

        let at = AccountStore::extract_access_token(&account.auth_json);
        let aid = AccountStore::extract_account_id(&account.auth_json);
        let rt = account
            .refresh_token
            .clone()
            .or_else(|| AccountStore::extract_refresh_token(&account.auth_json));
        let solo_or_client =
            matches!(store.settings.remote_mode.as_str(), "client" | "solo");

        (at, aid, rt, solo_or_client)
    };

    // 如果没有 access_token，先用 refresh_token 换一个
    // client/solo 模式：禁止本机 rt 刷新（rt 是 Server 的权威，本机偷偷 rotate 会让
    // Server 那边的 rt 立刻被 OpenAI 标 reused）
    let access_token = if let Some(at) = access_token_opt {
        at
    } else if is_client_or_solo {
        return Err(
            "TOKEN_INVALID:client/solo 模式禁止本机 rt 刷新；请确认账号已同步到 Server".to_string(),
        );
    } else if let Some(ref rt) = refresh_token {
        match crate::oauth::refresh_access_token_locked(&id, rt).await {
            Ok(token_res) => {
                let mut store = state.store.lock().map_err(|e| e.to_string())?;
                if let Some(account) = store.accounts.get_mut(&id) {
                    AccountStore::apply_refreshed_tokens(
                        account,
                        token_res.access_token.clone(),
                        token_res.refresh_token.clone(),
                        token_res.id_token,
                        token_res.expires_in,
                    );
                    if let Err(e) = store.save() {
                        eprintln!("[Store] 保存失败: {}", e);
                    }
                }
                token_res.access_token
            }
            Err(e) => return Err(format!("TOKEN_INVALID:刷新 token 失败: {}", e)),
        }
    } else {
        return Err("TOKEN_INVALID:无 access_token 且无 refresh_token".to_string());
    };

    // 2. 使用 Token 获取用量（client/solo 模式同样禁止 usage.rs 内部本地 rt 刷新）
    let result = UsageFetcher::fetch_usage_direct(
        access_token,
        account_id,
        refresh_token,
        !is_client_or_solo, // client/solo 禁本地 refresh，其它模式 allow
    )
    .await;

    // 检测封号/失效：分开标记
    if let Err(ref e) = result {
        if e.contains("ACCOUNT_BANNED") {
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            if let Some(account) = store.accounts.get_mut(&id) {
                account.is_banned = true;
                account.is_token_invalid = false;
                account.is_logged_out = false;
                if let Err(e) = store.save() {
                    eprintln!("[Store] 保存失败: {}", e);
                }
            }
            return Err(e.clone());
        }
        if e.contains("TOKEN_INVALID") {
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            if let Some(account) = store.accounts.get_mut(&id) {
                account.is_token_invalid = true;
                account.is_banned = false;
                account.is_logged_out = false;
                if let Err(e) = store.save() {
                    eprintln!("[Store] 保存失败: {}", e);
                }
            }
            return Err(e.clone());
        }
        if e.contains("ACCOUNT_LOGGED_OUT") {
            let mut store = state.store.lock().map_err(|e| e.to_string())?;
            if let Some(account) = store.accounts.get_mut(&id) {
                account.is_logged_out = true;
                account.is_banned = false;
                account.is_token_invalid = false;
                if let Err(e) = store.save() {
                    eprintln!("[Store] 保存失败: {}", e);
                }
            }
            return Err(e.clone());
        }
    }

    let (usage, new_tokens) = result?;

    // 3. 如果有新 Token，更新该账号的数据
    if let Some(tokens) = new_tokens {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(account) = store.accounts.get_mut(&id) {
            // 更新 auth_json 中的 Token 信息
            if let Some(obj) = account.auth_json.as_object_mut() {
                if let Some(tokens_obj) = obj.get_mut("tokens").and_then(|v| v.as_object_mut()) {
                    tokens_obj.insert(
                        "access_token".to_string(),
                        serde_json::json!(tokens.access_token),
                    );

                    if let Some(rt) = &tokens.refresh_token {
                        tokens_obj.insert("refresh_token".to_string(), serde_json::json!(rt));
                    } else if let Some(rt) = account.refresh_token.as_deref() {
                        if tokens_obj.get("refresh_token").is_none() {
                            tokens_obj.insert("refresh_token".to_string(), serde_json::json!(rt));
                        }
                    }

                    if let Some(it) = &tokens.id_token {
                        tokens_obj.insert("id_token".to_string(), serde_json::json!(it));
                    }

                    if let Some(expires_in) = tokens.expires_in {
                        let expires_at = (chrono::Utc::now()
                            + chrono::Duration::seconds(expires_in as i64))
                        .to_rfc3339();
                        tokens_obj.insert("expires_at".to_string(), serde_json::json!(expires_at));
                    }
                }
            }

            // 更新 refresh_token 字段
            if let Some(rt) = tokens.refresh_token {
                account.refresh_token = Some(rt);
            }
            if let Some(obj) = account.auth_json.as_object_mut() {
                obj.insert(
                    "last_refresh".to_string(),
                    serde_json::json!(Utc::now().to_rfc3339()),
                );
            }

            // 更新配额缓存
            account.cached_quota = Some(account::CachedQuota {
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
                updated_at: Utc::now(),
            });
            // quota 拉到了 = token 没过期，清掉历史 stale 失效标记
            account.is_token_invalid = false;
            account.is_logged_out = false;
        }
        store.save()?;
    } else {
        // 即使没有新 Token，也更新配额缓存
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        if let Some(account) = store.accounts.get_mut(&id) {
            account.cached_quota = Some(account::CachedQuota {
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
                updated_at: Utc::now(),
            });
            // quota 拉到了 = token 没过期，清掉历史 stale 失效标记
            account.is_token_invalid = false;
            account.is_logged_out = false;
        }
        store.save()?;
    }

    Ok(usage)
}

/// 修复 Codex App 的隔离属性 (需要 sudo 权限)
#[tauri::command]
fn request_quarantine_fix_ticket(state: State<AppState>) -> Result<String, String> {
    state.issue_quarantine_fix_ticket()
}

/// 修复 Codex App 的隔离属性 (需要 sudo 权限)
#[tauri::command]
async fn fix_codex_quarantine(
    state: tauri::State<'_, AppState>,
    ticket: String,
) -> Result<(), String> {
    state.consume_quarantine_fix_ticket(&ticket)?;
    ide_control::remove_quarantine()
}

/// 重载 IDE 窗口
#[tauri::command]
async fn reload_ide_windows(use_window_reload: bool) -> Result<Vec<String>, String> {
    let ides = ide_control::detect_running_ides();
    let mut reloaded = Vec::new();

    for ide in &ides {
        if let Err(e) = ide_control::reload_ide(ide, use_window_reload) {
            println!("重载 {} 失败: {}", ide, e);
        } else {
            reloaded.push(ide.clone());
        }
    }

    Ok(reloaded)
}

/// 获取 Token 用量统计
#[tauri::command]
fn get_token_stats(state: State<AppState>) -> Result<token_tracker::UsageStats, String> {
    Ok(state.token_tracker.get_stats())
}

/// 重置 Token 用量统计
#[tauri::command]
fn reset_token_stats(state: State<AppState>) -> Result<(), String> {
    state.token_tracker.reset();
    Ok(())
}

/// 获取 Token 使用历史（趋势图数据）
#[tauri::command]
fn get_token_history(days: u32) -> Result<Vec<token_tracker::TokenHistoryEntry>, String> {
    Ok(token_tracker::TokenTracker::get_history(days))
}

/// 订阅号每个号的 5h / 周周期"实测累加 + 估算上限"三级下钻数据。
///
/// 设计：
/// 1. 顶层每个订阅号一条 AccountTokenHistory，含当前/最近完成 5h、当前/最近完成周
///    的 CycleSummary 摘要。
/// 2. `cycles_5h` / `cycles_week` 是完整周期序列（倒序），点开账号看历史，能直接
///    肉眼比"上周 Plus 实际配额 / 这周 Plus 实际配额"判断 codex 是否改额度。
/// 3. 每个周期再点开看 `sessions`：该周期内每个 session_key 消耗多少 tokens、几轮。
///
/// 估算上限：在窗口内找一个 quota snapshot（reset_at 匹配），
///   capacity ≈ tokens_used_up_to_snapshot_ts / (snapshot.used_pct / 100)
/// 选 used_pct 最大的那个 snapshot 算（量化误差最小）。
#[derive(Debug, Clone, serde::Serialize)]
struct SessionInCycle {
    session_key: String,
    total_tokens: i64,
    turn_count: u32,
    first_seen_at: i64,
    last_seen_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CycleDetail {
    window_start: i64,
    window_end: i64,
    is_current: bool,
    total_tokens: i64,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    turn_count: u32,
    sessions: Vec<SessionInCycle>,
    /// 实测累加 ÷ snapshot used_pct × 100，用来估算 Plan 真实配额
    estimated_capacity: Option<i64>,
    /// 估算所用快照的 used_pct（用于在前端打"低 used_pct 量化误差大"标记）
    estimate_used_pct: Option<i32>,
    /// 是否在窗口内触发过限额切号
    hit_limit: bool,
    last_switch_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AccountTokenHistory {
    account_id: String,
    email: String,
    plan_type: String,
    is_current: bool,
    is_banned: bool,
    is_token_invalid: bool,
    /// 当前进行中的 5h 周期
    current_5h: Option<CycleDetail>,
    /// 最近一个已完成的 5h 周期
    last_5h: Option<CycleDetail>,
    current_week: Option<CycleDetail>,
    last_week: Option<CycleDetail>,
    /// 5h 周期全量历史（已完成 + 当前），倒序
    cycles_5h: Vec<CycleDetail>,
    /// 周周期全量历史，倒序
    cycles_week: Vec<CycleDetail>,
}

#[tauri::command]
fn get_account_token_history(
    state: State<AppState>,
) -> Result<Vec<AccountTokenHistory>, String> {
    let history = token_tracker::TokenTracker::get_history(30);
    let snapshots = quota_snapshot::read_all();
    let switches = state.switch_logger.get_history(30);
    let store = state.store.lock().map_err(|e| e.to_string())?;
    let current_id = store.current.clone();

    let five_h: i64 = 5 * 3600;
    let week_s: i64 = 7 * 24 * 3600;

    let mut out: Vec<AccountTokenHistory> = Vec::new();

    for (id, acc) in store.accounts.iter() {
        if acc.is_relay() {
            continue;
        }
        let cq = match acc.cached_quota.as_ref() {
            Some(q) => q,
            None => continue,
        };
        let email = AccountStore::extract_email(&acc.auth_json)
            .unwrap_or_else(|| acc.name.clone());

        // 该号的所有 entry / snapshot / switch（按时间排）
        let mut my_entries: Vec<&token_tracker::TokenHistoryEntry> = history
            .iter()
            .filter(|e| e.account_id == *id)
            .collect();
        my_entries.sort_by_key(|e| e.timestamp.timestamp());

        let mut my_snaps: Vec<&quota_snapshot::QuotaSnapshot> = snapshots
            .iter()
            .filter(|s| s.account_id == *id)
            .collect();
        my_snaps.sort_by_key(|s| s.ts.timestamp());

        let cycles_5h = build_cycles(
            &my_entries,
            &my_snaps,
            &switches,
            &acc.name,
            cq.five_hour_reset_at,
            five_h,
            true, // is_5h
        );
        let cycles_week = build_cycles(
            &my_entries,
            &my_snaps,
            &switches,
            &acc.name,
            cq.weekly_reset_at,
            week_s,
            false,
        );

        // "当前 5h" 优先取 is_current 那条；账号最近没活动时（reset_at 还没真正
        // 进入活跃周期 / 上次使用在好几个周期前）降级到最近一个有数据的周期，免得
        // 整列全是 "—" 看不出哪些号有过用量。前端用 is_current 字段区分真"当前"
        // 还是"最近一次"。
        let current_5h = cycles_5h
            .iter()
            .find(|c| c.is_current)
            .cloned()
            .or_else(|| cycles_5h.first().cloned());
        let last_5h = {
            let cur_end = current_5h.as_ref().map(|c| c.window_end);
            cycles_5h
                .iter()
                .find(|c| Some(c.window_end) != cur_end)
                .cloned()
        };
        let current_week = cycles_week
            .iter()
            .find(|c| c.is_current)
            .cloned()
            .or_else(|| cycles_week.first().cloned());
        let last_week = {
            let cur_end = current_week.as_ref().map(|c| c.window_end);
            cycles_week
                .iter()
                .find(|c| Some(c.window_end) != cur_end)
                .cloned()
        };

        out.push(AccountTokenHistory {
            account_id: id.clone(),
            email,
            plan_type: cq.plan_type.clone(),
            is_current: current_id.as_ref() == Some(id),
            is_banned: acc.is_banned,
            is_token_invalid: acc.is_token_invalid,
            current_5h,
            last_5h,
            current_week,
            last_week,
            cycles_5h,
            cycles_week,
        });
    }

    // 按 plan 优先级排序：pro → plus → team → free → unknown；同 plan 内按 email
    fn plan_rank(plan: &str) -> u8 {
        match plan.to_lowercase().as_str() {
            "pro" => 0,
            "plus" => 1,
            "team" => 2,
            "free" => 3,
            _ => 4,
        }
    }
    out.sort_by(|a, b| {
        plan_rank(&a.plan_type)
            .cmp(&plan_rank(&b.plan_type))
            .then(a.email.cmp(&b.email))
    });
    Ok(out)
}

/// 把一个号在 30 天里的 entries 按 `reset_at` 锚点划成多个 (5h 或 周) 周期。
/// 返回倒序（最近一个在最前），含当前进行中的窗口 + 历史已完成窗口。
fn build_cycles(
    entries: &[&token_tracker::TokenHistoryEntry],
    snaps: &[&quota_snapshot::QuotaSnapshot],
    switches: &[switch_log::SwitchEvent],
    account_name: &str,
    reset_at_opt: Option<i64>,
    window_size: i64,
    is_5h: bool,
) -> Vec<CycleDetail> {
    let reset_at = match reset_at_opt {
        Some(r) => r,
        None => return Vec::new(),
    };

    use std::collections::HashMap;
    // key = window_end → CycleDetail accumulator
    let mut buckets: HashMap<i64, CycleDetail> = HashMap::new();
    // session_key → (tokens, turn_count, first_ts, last_ts) within each window
    let mut sessions_per_window: HashMap<i64, HashMap<String, SessionInCycle>> = HashMap::new();

    for e in entries {
        let ts = e.timestamp.timestamp();
        if ts >= reset_at {
            continue;
        }
        let n = (reset_at - ts - 1) / window_size;
        let win_end = reset_at - n * window_size;
        let win_start = win_end - window_size;
        let is_current = n == 0;

        let cycle = buckets.entry(win_end).or_insert_with(|| CycleDetail {
            window_start: win_start,
            window_end: win_end,
            is_current,
            total_tokens: 0,
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            turn_count: 0,
            sessions: Vec::new(),
            estimated_capacity: None,
            estimate_used_pct: None,
            hit_limit: false,
            last_switch_reason: None,
        });
        cycle.input_tokens += e.input_tokens;
        cycle.cached_input_tokens += e.cached_input_tokens;
        cycle.output_tokens += e.output_tokens;
        cycle.total_tokens += e.input_tokens + e.output_tokens;
        cycle.turn_count += 1;

        // session breakdown（旧记录 session_key 为空，统一聚到 "(legacy)" 一类）
        let sk = if e.session_key.is_empty() {
            "(legacy)".to_string()
        } else {
            e.session_key.clone()
        };
        let sessions_map = sessions_per_window.entry(win_end).or_default();
        let sess = sessions_map.entry(sk.clone()).or_insert(SessionInCycle {
            session_key: sk,
            total_tokens: 0,
            turn_count: 0,
            first_seen_at: ts,
            last_seen_at: ts,
        });
        sess.total_tokens += e.input_tokens + e.output_tokens;
        sess.turn_count += 1;
        if ts < sess.first_seen_at {
            sess.first_seen_at = ts;
        }
        if ts > sess.last_seen_at {
            sess.last_seen_at = ts;
        }
    }

    // 为每个 cycle 用 snapshot 估算 capacity。
    // 策略：找 reset_at 匹配该 cycle 的 snapshots，
    // 选 used_pct 最大那个（量化误差最小），
    // 然后用"该 snapshot 之前累计的 tokens / used_pct × 100"作为 capacity。
    for (win_end, cycle) in buckets.iter_mut() {
        let matching_snaps: Vec<&&quota_snapshot::QuotaSnapshot> = snaps
            .iter()
            .filter(|s| {
                let rs = if is_5h {
                    s.five_hour_reset_at
                } else {
                    s.weekly_reset_at
                };
                rs == Some(*win_end)
            })
            .collect();
        // 选 used_pct 最大的 snapshot
        let best = matching_snaps.iter().max_by_key(|s| {
            if is_5h {
                s.five_hour_used_pct
            } else {
                s.weekly_used_pct
            }
        });
        if let Some(s) = best {
            let used_pct = if is_5h {
                s.five_hour_used_pct
            } else {
                s.weekly_used_pct
            };
            if used_pct > 0 {
                let snap_ts = s.ts.timestamp();
                // 累加该 cycle 内、snapshot 之前的 tokens
                let mut tokens_up_to_snap: i64 = 0;
                for e in entries {
                    let ts = e.timestamp.timestamp();
                    if ts >= cycle.window_start && ts < cycle.window_end && ts <= snap_ts {
                        tokens_up_to_snap += e.input_tokens + e.output_tokens;
                    }
                }
                if tokens_up_to_snap > 0 {
                    let cap =
                        (tokens_up_to_snap as f64 / used_pct as f64 * 100.0).round() as i64;
                    cycle.estimated_capacity = Some(cap);
                    cycle.estimate_used_pct = Some(used_pct);
                }
            }
        }
    }

    // 用 switch_log 反查 limit_hit（沿用旧逻辑）
    for (win_end, cycle) in buckets.iter_mut() {
        let win_start = cycle.window_start;
        let win_end_slop = *win_end + 60;
        for sw in switches.iter() {
            let sw_ts = sw.timestamp.timestamp();
            if sw_ts < win_start || sw_ts >= win_end_slop {
                continue;
            }
            let from = match sw.from_account.as_deref() {
                Some(n) if !n.is_empty() => n,
                _ => continue,
            };
            if from != account_name {
                continue;
            }
            let is_limit = matches!(
                sw.reason,
                switch_log::SwitchReason::Http429
                    | switch_log::SwitchReason::InStreamRateLimit
                    | switch_log::SwitchReason::WebSocketRateLimit
                    | switch_log::SwitchReason::WebSocketPrecheck
            );
            if is_limit {
                cycle.hit_limit = true;
                cycle.last_switch_reason = Some(format!("{}", sw.reason));
            }
        }
    }

    // 把 sessions 装回 cycle，按 total_tokens 降序
    for (win_end, mut sessions_map) in sessions_per_window {
        if let Some(cycle) = buckets.get_mut(&win_end) {
            let mut list: Vec<SessionInCycle> = sessions_map.drain().map(|(_, v)| v).collect();
            list.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));
            cycle.sessions = list;
        }
    }

    let mut result: Vec<CycleDetail> = buckets.into_values().collect();
    result.sort_by(|a, b| b.window_end.cmp(&a.window_end));
    let label = if is_5h { "5h" } else { "wk" };
    let has_current = result.iter().any(|c| c.is_current);
    println!(
        "[AcctHist] {} {} entries={} reset_at={:?} cycles={} has_current={}",
        account_name,
        label,
        entries.len(),
        reset_at_opt,
        result.len(),
        has_current
    );
    result
}

/// 订阅号每个完整 5h / 周窗口的 token 总量。
///
/// 用途：用户横向对比 free / plus / pro / team 的实际限额 —— 当某账号在某个
/// 窗口跑到 usage_limit_reached 时，该窗口的 total_tokens 就是该 Plan 的窗口配额。
///
/// 窗口对齐：以 `cached_quota.{five_hour,weekly}_reset_at` 为锚点，往前每 5h
/// （或 1 周）划一个窗口，把 30 天历史里属于该账号的请求按时间归到对应窗口。
/// `is_current` 标识当前进行中的窗口（n=0），其他都是已完成的历史窗口。
#[derive(Debug, Clone, serde::Serialize)]
struct QuotaCycle {
    account_id: String,
    /// account.name —— 与 switch_log.from_account 字段匹配用
    name: String,
    email: String,
    plan_type: String,
    /// "5h" 或 "week"
    window_type: String,
    /// 窗口起点（unix sec）
    window_start: i64,
    /// 窗口终点（unix sec）= 该窗口对应的 reset 时间
    window_end: i64,
    /// 当前进行中的窗口（n=0），其他是已完成的
    is_current: bool,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    /// input + output（cached 已计入 input，不重复加）
    total_tokens: i64,
    request_count: u32,
    /// 窗口内发生过限额触发切号（429 / WS 限额 / 流内限额 / WS 预检发现耗尽）
    /// → 该窗口 total_tokens ≈ 该 Plan 实测窗口上限
    limit_hit: bool,
    /// 窗口内发生过封号触发切号 —— total_tokens 不能当限额参考
    banned_in_window: bool,
    /// 窗口内最后一次"限额 / 封号"切号的 reason 文本
    last_switch_reason: Option<String>,
    /// 该切号事件的 unix sec 时间戳
    last_switch_at: Option<i64>,
}

#[tauri::command]
fn get_quota_cycles(state: State<AppState>) -> Result<Vec<QuotaCycle>, String> {
    let history = token_tracker::TokenTracker::get_history(30);
    let store = state.store.lock().map_err(|e| e.to_string())?;

    let five_h: i64 = 5 * 3600;
    let week: i64 = 7 * 24 * 3600;

    use std::collections::HashMap;
    // key = (account_id, window_type, window_end_unix_sec)
    let mut buckets: HashMap<(String, String, i64), QuotaCycle> = HashMap::new();

    for entry in history.iter() {
        let acc = match store.accounts.get(&entry.account_id) {
            Some(a) => a,
            None => continue, // 已删除账号的旧记录
        };
        if acc.is_relay() {
            continue;
        }
        let cq = match acc.cached_quota.as_ref() {
            Some(q) => q,
            None => continue, // 没拉过 quota 没法对齐窗口锚点
        };

        let email = AccountStore::extract_email(&acc.auth_json)
            .unwrap_or_else(|| acc.name.clone());
        let ts = entry.timestamp.timestamp();

        for (kind, window_size, reset_at_opt) in [
            ("5h", five_h, cq.five_hour_reset_at),
            ("week", week, cq.weekly_reset_at),
        ] {
            let reset_at = match reset_at_opt {
                Some(r) => r,
                None => continue,
            };
            if ts >= reset_at {
                // entry 在 cached reset_at 之后 → quota 已过期、cache 没刷
                // 简化处理：跳过这条记录的此窗口归类
                continue;
            }
            // 半开区间 [reset_at - (n+1)W, reset_at - nW)：
            // ts = reset_at - 1     → n = 0（当前窗口）
            // ts = reset_at - W     → n = 0（属于当前窗口的起点）
            // ts = reset_at - W - 1 → n = 1（上一个窗口的末尾）
            let n = (reset_at - ts - 1) / window_size;
            let win_end = reset_at - n * window_size;
            let win_start = win_end - window_size;
            let is_current = n == 0;

            let key = (entry.account_id.clone(), kind.to_string(), win_end);
            let cycle = buckets.entry(key).or_insert_with(|| QuotaCycle {
                account_id: entry.account_id.clone(),
                name: acc.name.clone(),
                email: email.clone(),
                plan_type: cq.plan_type.clone(),
                window_type: kind.to_string(),
                window_start: win_start,
                window_end: win_end,
                is_current,
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                request_count: 0,
                limit_hit: false,
                banned_in_window: false,
                last_switch_reason: None,
                last_switch_at: None,
            });
            cycle.input_tokens += entry.input_tokens;
            cycle.cached_input_tokens += entry.cached_input_tokens;
            cycle.output_tokens += entry.output_tokens;
            cycle.total_tokens += entry.input_tokens + entry.output_tokens;
            cycle.request_count += 1;
        }
    }

    // 用 switch_log 反查每个窗口内的切号事件，标记是否真正触发了限额或封号。
    // 限额命中（→ total_tokens ≈ 该号该窗口实测上限）：
    //   - Http429 / InStreamRateLimit / WebSocketRateLimit：流内/握手时上游真发了限额响应
    //   - WebSocketPrecheck：发起 WS 前发现 cached_quota.left <= 0，说明此号在该窗口
    //     已经达到上限（虽然这次切号事件本身是"事后"，但能反推窗口被打满了）
    // QuotaThreshold（阈值预防）不算 limit_hit，那是 cached_left 跌到阈值的提前切，
    // total_tokens 会低于真实上限。
    let switches = state.switch_logger.get_history(30);
    for cycle in buckets.values_mut() {
        let win_start = cycle.window_start;
        // 限额响应通常在窗口最后一条 entry 之后立刻发，给 60s slop 容纳网络抖动
        let win_end_slop = cycle.window_end + 60;
        for sw in switches.iter() {
            let sw_ts = sw.timestamp.timestamp();
            if sw_ts < win_start || sw_ts >= win_end_slop {
                continue;
            }
            let from = match sw.from_account.as_deref() {
                Some(n) if !n.is_empty() => n,
                _ => continue,
            };
            if from != cycle.name {
                continue;
            }
            let is_limit = matches!(
                sw.reason,
                switch_log::SwitchReason::Http429
                    | switch_log::SwitchReason::InStreamRateLimit
                    | switch_log::SwitchReason::WebSocketRateLimit
                    | switch_log::SwitchReason::WebSocketPrecheck
            );
            let is_ban = matches!(
                sw.reason,
                switch_log::SwitchReason::InStreamBanned
                    | switch_log::SwitchReason::BannedDetected
            );
            if is_limit {
                cycle.limit_hit = true;
            }
            if is_ban {
                cycle.banned_in_window = true;
            }
            if is_limit || is_ban {
                if cycle.last_switch_at.map(|t| t < sw_ts).unwrap_or(true) {
                    cycle.last_switch_at = Some(sw_ts);
                    cycle.last_switch_reason = Some(format!("{}", sw.reason));
                }
            }
        }
    }

    let mut result: Vec<QuotaCycle> = buckets.into_values().collect();
    // plan ASC, window_type ('5h' < 'week'), window_end DESC, email ASC
    result.sort_by(|a, b| {
        a.plan_type
            .cmp(&b.plan_type)
            .then(a.window_type.cmp(&b.window_type))
            .then(b.window_end.cmp(&a.window_end))
            .then(a.email.cmp(&b.email))
    });
    Ok(result)
}

/// Plan 配额上限估算 —— 用相邻两次 quota 快照之间的 Δused_pct + 期间代理捕获到的
/// Δtokens 反推该 Plan 的窗口配额，**不需要账号被打到 usage_limit_reached**。
///
/// 公式：capacity ≈ Δtokens / Δused_pct × 100
///
/// 桶按 (account_id, window_type, reset_at) 划分 —— 同 reset_at 意味着同一个窗口，
/// used_pct 在桶内单调递增。Δpct 太小（<3%）的样本被丢弃，避免 used_pct 整数量化
/// 误差放大估算。
#[derive(Debug, Clone, serde::Serialize)]
struct PlanCapacityEstimate {
    plan_type: String,
    /// "5h" 或 "week"
    window_type: String,
    sample_count: u32,
    avg_capacity: f64,
    median_capacity: f64,
    min_capacity: f64,
    max_capacity: f64,
}

#[tauri::command]
fn get_plan_capacity_estimates() -> Result<Vec<PlanCapacityEstimate>, String> {
    let snapshots = quota_snapshot::read_all();
    let history = token_tracker::TokenTracker::get_history(30);

    use std::collections::HashMap;
    // key: (account_id, window_type, reset_at, plan_type) → [(ts, used_pct), ...]
    let mut by_window: HashMap<(String, String, i64, String), Vec<(i64, i32)>> =
        HashMap::new();
    for s in &snapshots {
        let ts = s.ts.timestamp();
        if let Some(reset) = s.five_hour_reset_at {
            by_window
                .entry((
                    s.account_id.clone(),
                    "5h".to_string(),
                    reset,
                    s.plan_type.clone(),
                ))
                .or_default()
                .push((ts, s.five_hour_used_pct));
        }
        if let Some(reset) = s.weekly_reset_at {
            by_window
                .entry((
                    s.account_id.clone(),
                    "week".to_string(),
                    reset,
                    s.plan_type.clone(),
                ))
                .or_default()
                .push((ts, s.weekly_used_pct));
        }
    }

    // 收集到 by_plan: (plan, window_type) → [estimate, ...]
    let mut by_plan: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for ((account_id, window_type, _reset, plan_type), pairs) in &by_window {
        let mut p = pairs.clone();
        p.sort_by_key(|(ts, _)| *ts);
        for i in 1..p.len() {
            let (t1, pct1) = p[i - 1];
            let (t2, pct2) = p[i];
            if pct2 <= pct1 {
                // 跨越 reset 边界（理论上 reset_at 已经分桶不会出现）或同时刻
                continue;
            }
            let delta_pct = (pct2 - pct1) as f64;
            // used_pct 是 0-100 整数；Δpct<3 时量化误差 >33%，丢弃
            if delta_pct < 3.0 {
                continue;
            }
            // Δtokens：t1 < entry.ts <= t2 的 token-history 累加
            let delta_tokens: i64 = history
                .iter()
                .filter(|e| {
                    e.account_id == *account_id
                        && e.timestamp.timestamp() > t1
                        && e.timestamp.timestamp() <= t2
                })
                .map(|e| e.input_tokens + e.output_tokens)
                .sum();
            if delta_tokens <= 0 {
                continue;
            }
            let estimate = delta_tokens as f64 / delta_pct * 100.0;
            by_plan
                .entry((plan_type.clone(), window_type.clone()))
                .or_default()
                .push(estimate);
        }
    }

    let mut result: Vec<PlanCapacityEstimate> = by_plan
        .into_iter()
        .map(|((plan, wt), mut estimates)| {
            estimates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = estimates.len();
            let avg = estimates.iter().sum::<f64>() / n as f64;
            let median = if n % 2 == 1 {
                estimates[n / 2]
            } else {
                (estimates[n / 2 - 1] + estimates[n / 2]) / 2.0
            };
            let min = *estimates.first().unwrap();
            let max = *estimates.last().unwrap();
            PlanCapacityEstimate {
                plan_type: plan,
                window_type: wt,
                sample_count: n as u32,
                avg_capacity: avg,
                median_capacity: median,
                min_capacity: min,
                max_capacity: max,
            }
        })
        .collect();
    result.sort_by(|a, b| {
        a.plan_type
            .cmp(&b.plan_type)
            .then(a.window_type.cmp(&b.window_type))
    });
    Ok(result)
}

/// 手动触发一次 client 模式快速 auth.json 同步（拉 Server current → 写盘）。
/// 用于"我看到 store/disk 不一致"或"想立即把磁盘对齐到 Server"的场景。
#[tauri::command]
async fn force_auth_resync(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(do_one_fast_auth_sync(&state.store).await)
}

/// 当前 SessionAffinity 表里所有活跃绑定（session_key → account 映射）
#[tauri::command]
fn get_session_bindings(
    state: State<AppState>,
) -> Result<Vec<session_affinity::SessionBindingSnapshot>, String> {
    Ok(state.session_affinity.snapshot())
}

// ── Session Routes（用户级硬路由）命令 ──

/// 列出所有 session 硬路由（按 created_at desc）
#[tauri::command]
async fn list_session_routes(
    state: State<'_, AppState>,
) -> Result<Vec<session_routes::SessionRoute>, String> {
    let store = state
        .session_routes
        .lock()
        .map_err(|e| format!("session_routes lock: {}", e))?;
    Ok(store.list())
}

/// 新增 / upsert 一条 session 硬路由（按 session_id 去重，hit_count 在 upsert 时保留）
///
/// 写完会广播 `ws_disconnect`，让所有正在 ChatGPT 上挂着的 WS 客户端被踢，codex
/// 自动重连后会经过 hard route 检查、命中新路由。这样用户在 UI 点"绑定"后
/// 下一条消息就立刻走到新上游，不需要重启 codex 或 codex-switcher。
#[tauri::command]
async fn add_session_route(
    state: State<'_, AppState>,
    session_id: String,
    account_id: String,
    label: Option<String>,
) -> Result<session_routes::SessionRoute, String> {
    let route = {
        let mut store = state
            .session_routes
            .lock()
            .map_err(|e| format!("session_routes lock: {}", e))?;
        let route = store.add(session_id, account_id, label);
        store.save()?;
        route
    };
    state.ws_disconnect.notify_waiters();
    Ok(route)
}

/// 删除一条 session 硬路由
#[tauri::command]
async fn delete_session_route(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    {
        let mut store = state
            .session_routes
            .lock()
            .map_err(|e| format!("session_routes lock: {}", e))?;
        if !store.delete(&id) {
            return Err(format!("session route 不存在: {}", id));
        }
        store.save()?;
    }
    state.ws_disconnect.notify_waiters();
    Ok(())
}

/// 启用 / 禁用一条 session 硬路由（不删，可继续保留 hit_count）
#[tauri::command]
async fn toggle_session_route(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut store = state
            .session_routes
            .lock()
            .map_err(|e| format!("session_routes lock: {}", e))?;
        if !store.toggle(&id, enabled) {
            return Err(format!("session route 不存在: {}", id));
        }
        store.save()?;
    }
    state.ws_disconnect.notify_waiters();
    Ok(())
}

/// 修改一条 session 硬路由的 label
#[tauri::command]
async fn update_session_route_label(
    state: State<'_, AppState>,
    id: String,
    label: Option<String>,
) -> Result<(), String> {
    let mut store = state
        .session_routes
        .lock()
        .map_err(|e| format!("session_routes lock: {}", e))?;
    if !store.update_label(&id, label) {
        return Err(format!("session route 不存在: {}", id));
    }
    store.save()?;
    // label 改名不影响路由匹配，不需要踢 WS
    Ok(())
}

/// 扫描 ~/.codex/sessions 下的本地 session 列表（用于前端"挑一条会话来绑定"）
#[tauri::command]
async fn list_codex_sessions(
    limit: Option<usize>,
    project_filter: Option<String>,
    days_back: Option<u32>,
) -> Result<Vec<codex_sessions::CodexSession>, String> {
    codex_sessions::list_codex_sessions(limit, project_filter, days_back)
}

/// 检测"当前活跃的 codex 会话"：返回 mtime 最新且在 `window_secs` 秒内被写过的
/// rollout 对应的 session 元信息；没有则 None（用户没在跟 codex 聊或停了很久）。
/// 前端 AddRouteModal 的"绑定当前活跃会话"按钮调这个。
#[tauri::command]
async fn detect_active_codex_session(
    window_secs: Option<u64>,
) -> Result<Option<codex_sessions::CodexSession>, String> {
    Ok(codex_sessions::detect_active_session(
        window_secs.unwrap_or(300),
    ))
}

// ── Skills 管理命令 ──

#[tauri::command]
fn get_installed_skills() -> Result<Vec<skills::InstalledSkill>, String> {
    let data = skills::SkillStore::load();
    Ok(data.skills)
}

#[tauri::command]
fn get_skill_repos() -> Result<Vec<skills::SkillRepo>, String> {
    let data = skills::SkillStore::load();
    Ok(data.repos)
}

#[tauri::command]
fn add_skill_repo(owner: String, name: String, branch: String) -> Result<(), String> {
    let mut data = skills::SkillStore::load();
    if data
        .repos
        .iter()
        .any(|r| r.owner == owner && r.name == name)
    {
        return Err("仓库已存在".into());
    }
    data.repos.push(skills::SkillRepo {
        owner,
        name,
        branch,
        enabled: true,
    });
    skills::SkillStore::save(&data)
}

#[tauri::command]
fn remove_skill_repo(owner: String, name: String) -> Result<(), String> {
    let mut data = skills::SkillStore::load();
    data.repos.retain(|r| !(r.owner == owner && r.name == name));
    skills::SkillStore::save(&data)
}

#[tauri::command]
async fn discover_skills() -> Result<Vec<skills::DiscoverableSkill>, String> {
    let data = skills::SkillStore::load();
    let mut discovered = skills::SkillStore::discover_skills(&data.repos).await;
    // 标记已安装的
    let installed_dirs: std::collections::HashSet<String> =
        data.skills.iter().map(|s| s.directory.clone()).collect();
    for s in &mut discovered {
        s.installed = installed_dirs.contains(&s.directory);
    }
    Ok(discovered)
}

#[tauri::command]
async fn install_skill(skill_json: String) -> Result<(), String> {
    let skill: skills::DiscoverableSkill =
        serde_json::from_str(&skill_json).map_err(|e| e.to_string())?;
    let mut data = skills::SkillStore::load();
    skills::SkillStore::install_skill(&mut data, &skill).await?;
    skills::SkillStore::save(&data)
}

#[tauri::command]
fn uninstall_skill(skill_id: String) -> Result<(), String> {
    let mut data = skills::SkillStore::load();
    skills::SkillStore::uninstall_skill(&mut data, &skill_id)?;
    skills::SkillStore::save(&data)
}

#[tauri::command]
fn toggle_skill_app_link(app: String, enabled: bool) -> Result<(), String> {
    skills::SkillStore::toggle_app_link(&app, enabled)
}

#[tauri::command]
fn get_skill_app_status() -> Result<std::collections::HashMap<String, bool>, String> {
    Ok(skills::SkillStore::get_app_link_status())
}

#[tauri::command]
fn get_skill_content(directory: String) -> Result<String, String> {
    let ssot = dirs::home_dir()
        .unwrap()
        .join(".codex-switcher")
        .join("skills")
        .join(&directory);
    let md_path = ssot.join("SKILL.md");
    std::fs::read_to_string(&md_path).map_err(|e| format!("读取失败: {}", e))
}

#[tauri::command]
fn scan_and_import_skills() -> Result<usize, String> {
    let mut data = skills::SkillStore::load();
    let count = skills::SkillStore::scan_existing(&mut data);
    if count > 0 {
        skills::SkillStore::save(&data)?;
    }
    Ok(count)
}

#[tauri::command]
fn sync_all_skills() -> Result<(), String> {
    skills::SkillStore::sync_all();
    Ok(())
}

#[tauri::command]
fn get_switch_history(
    state: State<AppState>,
    days: u32,
) -> Result<Vec<switch_log::SwitchEvent>, String> {
    Ok(state.switch_logger.get_history(days))
}

/// 获取切号统计
#[tauri::command]
fn get_switch_stats(state: State<AppState>) -> Result<switch_log::SwitchStats, String> {
    Ok(state.switch_logger.get_stats())
}

/// 显示主窗口（供 tray popup 调用）
#[tauri::command]
fn show_main_window_cmd(app: tauri::AppHandle) {
    crate::tray::show_main_window_from_cmd(&app);
}

/// 杀死所有 codex 相关进程（排除 Codex Switcher 自身）
#[tauri::command]
fn kill_codex_processes() -> Result<String, String> {
    let script = r#"
        killed=0
        for pid in $(pgrep -f codex 2>/dev/null); do
            cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
            case "$cmd" in
                *codex-switcher*|*Codex\ Switcher*|*codex_switcher*) continue ;;
            esac
            kill -9 "$pid" 2>/dev/null && killed=$((killed+1))
        done
        echo "$killed"
    "#;

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|e| format!("执行失败: {}", e))?;

    let count = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let n: i32 = count.parse().unwrap_or(0);

    if n > 0 {
        Ok(format!("已终止 {} 个 codex 进程", n))
    } else {
        Ok("未找到运行中的 codex 进程".to_string())
    }
}

/// 设置 OPENAI_BASE_URL 环境变量（终端 + GUI 应用全覆盖）
#[tauri::command]
fn set_proxy_env(port: u16, enable: bool) -> Result<String, String> {
    let home = dirs::home_dir().ok_or("无法获取用户目录")?;
    let env_value = format!("http://localhost:{}/v1", port);
    let env_line = format!("export OPENAI_BASE_URL={}", env_value);
    let marker = "# codex-switcher-proxy";
    let mut results = Vec::new();

    // ── 1. 终端：写入 .zshrc / .bashrc ──
    for rc_name in &[".zshrc", ".bashrc"] {
        let rc_path = home.join(rc_name);
        if !rc_path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&rc_path)
            .map_err(|e| format!("读取 {} 失败: {}", rc_name, e))?;

        let cleaned: Vec<&str> = content
            .lines()
            .filter(|line| !line.contains(marker))
            .collect();
        let mut new_content = cleaned.join("\n");

        if enable {
            if !new_content.ends_with('\n') {
                new_content.push('\n');
            }
            new_content.push_str(&format!("{} {}\n", env_line, marker));
        }

        std::fs::write(&rc_path, &new_content)
            .map_err(|e| format!("写入 {} 失败: {}", rc_name, e))?;
        results.push(rc_name.to_string());
    }

    // ── 2. GUI 应用：launchctl setenv（Codex App 重启后生效）──
    #[cfg(target_os = "macos")]
    {
        if enable {
            let _ = std::process::Command::new("launchctl")
                .args(["setenv", "OPENAI_BASE_URL", &env_value])
                .output();
            results.push("launchctl".to_string());
        } else {
            let _ = std::process::Command::new("launchctl")
                .args(["unsetenv", "OPENAI_BASE_URL"])
                .output();
            results.push("launchctl".to_string());
        }
    }

    // ── 3. Codex App config.toml：写入 openai_base_url ──
    match set_codex_config_base_url(if enable { Some(&env_value) } else { None }) {
        Ok(_) => results.push("config.toml".to_string()),
        Err(e) => results.push(format!("config.toml(失败: {})", e)),
    }

    let status = if enable { "已设置" } else { "已移除" };
    Ok(format!(
        "{} OPENAI_BASE_URL ({})。\n终端：新窗口生效\nCodex App：重启后生效",
        status,
        results.join(", ")
    ))
}

/// 读写 ~/.codex/config.toml 的 openai_base_url 字段
fn set_codex_config_base_url(url: Option<&str>) -> Result<(), String> {
    let config_path = dirs::home_dir()
        .ok_or("无法获取用户目录")?
        .join(".codex")
        .join("config.toml");

    if !config_path.exists() {
        if url.is_some() {
            // 文件不存在，创建并写入
            let content = format!("openai_base_url = \"{}\"\n", url.unwrap());
            std::fs::write(&config_path, content)
                .map_err(|e| format!("创建 config.toml 失败: {}", e))?;
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("读取 config.toml 失败: {}", e))?;

    let mut new_lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut in_section = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // 检测 [section] 开头，用于判断是否在顶层
        if trimmed.starts_with('[') {
            in_section = true;
        }

        // 匹配顶层的 openai_base_url = "xxx"
        if !in_section && trimmed.starts_with("openai_base_url") && trimmed.contains('=') {
            found = true;
            if let Some(u) = url {
                new_lines.push(format!("openai_base_url = \"{}\"", u));
            }
            // url 为 None 时跳过这行（移除）
            continue;
        }
        new_lines.push(line.to_string());
    }

    // 如果要设置但没找到已有行，在第一个 [section] 之前插入
    if url.is_some() && !found {
        let u = url.unwrap();
        let insert_line = format!("openai_base_url = \"{}\"", u);
        // 找到第一个 [section] 的位置
        let pos = new_lines.iter().position(|l| l.trim().starts_with('['));
        match pos {
            Some(idx) => new_lines.insert(idx, insert_line),
            None => new_lines.push(insert_line),
        }
    }

    std::fs::write(&config_path, new_lines.join("\n") + "\n")
        .map_err(|e| format!("写入 config.toml 失败: {}", e))?;

    Ok(())
}

/// 切换 Codex fast 模式（修改 config.toml 的 profile 字段）
#[tauri::command]
fn set_codex_fast_mode(enable: bool) -> Result<String, String> {
    let config_path = dirs::home_dir()
        .ok_or("无法获取用户目录")?
        .join(".codex")
        .join("config.toml");

    if !config_path.exists() {
        return Err("~/.codex/config.toml 不存在".to_string());
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("读取 config.toml 失败: {}", e))?;

    let mut new_lines: Vec<String> = Vec::new();
    let mut found_profile = false;

    for line in content.lines() {
        let trimmed = line.trim();
        // 匹配 profile = "xxx" 行（顶层，不在 [section] 下面的缩进行）
        if trimmed.starts_with("profile") && trimmed.contains('=') && !trimmed.starts_with('[') {
            found_profile = true;
            if enable {
                new_lines.push("profile = \"fast\"".to_string());
            }
            // 不 enable 时跳过这行（移除 profile）
            continue;
        }
        new_lines.push(line.to_string());
    }

    // 如果 enable 但没找到 profile 行，在文件开头插入
    if enable && !found_profile {
        new_lines.insert(0, "profile = \"fast\"".to_string());
    }

    std::fs::write(&config_path, new_lines.join("\n") + "\n")
        .map_err(|e| format!("写入 config.toml 失败: {}", e))?;

    if enable {
        Ok("Fast 模式已开启（2x 额度消耗，更快推理）。重启 Codex 生效。".to_string())
    } else {
        Ok("Fast 模式已关闭。重启 Codex 生效。".to_string())
    }
}

/// 切换 ~/.codex/config.toml 里的 [features] goals 开关
#[tauri::command]
fn set_codex_features_goals(enable: bool) -> Result<String, String> {
    let config_path = dirs::home_dir()
        .ok_or("无法获取用户目录")?
        .join(".codex")
        .join("config.toml");

    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取 config.toml 失败: {}", e))?
    } else {
        String::new()
    };

    let mut new_lines: Vec<String> = Vec::new();
    let mut in_features = false;
    let mut features_header_idx: Option<usize> = None;
    let mut goals_seen = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_features = trimmed == "[features]";
            if in_features {
                features_header_idx = Some(new_lines.len());
            }
        }
        // 在 [features] section 里匹配 goals = ... 行
        if in_features
            && trimmed.starts_with("goals")
            && trimmed.contains('=')
            && !trimmed.starts_with('[')
        {
            goals_seen = true;
            if enable {
                new_lines.push("goals = true".to_string());
            }
            // disable 时跳过这行（移除）
            continue;
        }
        new_lines.push(line.to_string());
    }

    if enable {
        if features_header_idx.is_none() {
            // 没有 [features] section → 在文件末尾追加
            if !new_lines.is_empty() && !new_lines.last().map(|l| l.is_empty()).unwrap_or(true) {
                new_lines.push(String::new());
            }
            new_lines.push("[features]".to_string());
            new_lines.push("goals = true".to_string());
        } else if !goals_seen {
            // 有 [features] section 但没 goals 行 → 紧跟在 header 后面插
            let idx = features_header_idx.unwrap();
            new_lines.insert(idx + 1, "goals = true".to_string());
        }
    }

    std::fs::write(&config_path, new_lines.join("\n") + "\n")
        .map_err(|e| format!("写入 config.toml 失败: {}", e))?;

    Ok(if enable {
        "[features] goals = true 已写入。重启 Codex 生效。".to_string()
    } else {
        "[features] goals 已关闭。重启 Codex 生效。".to_string()
    })
}

/// 读 ~/.codex/config.toml 里的 [features] goals 开关
#[tauri::command]
fn get_codex_features_goals() -> Result<bool, String> {
    let config_path = dirs::home_dir()
        .ok_or("无法获取用户目录")?
        .join(".codex")
        .join("config.toml");

    if !config_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&config_path).map_err(|e| format!("读取失败: {}", e))?;
    let mut in_features = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_features = trimmed == "[features]";
            continue;
        }
        if in_features && trimmed.starts_with("goals") && trimmed.contains('=') {
            return Ok(trimmed.contains("true"));
        }
    }

    Ok(false)
}

/// 获取当前 fast 模式状态
#[tauri::command]
fn get_codex_fast_mode() -> Result<bool, String> {
    let config_path = dirs::home_dir()
        .ok_or("无法获取用户目录")?
        .join(".codex")
        .join("config.toml");

    if !config_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&config_path).map_err(|e| format!("读取失败: {}", e))?;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("profile") && trimmed.contains('=') {
            return Ok(trimmed.contains("\"fast\""));
        }
    }

    Ok(false)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncStatus {
    pub is_synced: bool,
    pub disk_email: Option<String>,
    pub matching_id: Option<String>,
    pub current_id: Option<String>,
}

/// 检查 IDE 磁盘状态与内存状态的同步情况
#[tauri::command]
fn get_sync_status(state: State<AppState>) -> Result<SyncStatus, String> {
    let disk_auth = match AccountStore::read_codex_auth() {
        Ok(a) => a,
        Err(_) => {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            return Ok(SyncStatus {
                is_synced: true,
                disk_email: None,
                matching_id: None,
                current_id: store.current.clone(),
            });
        }
    };

    let store = state.store.lock().map_err(|e| e.to_string())?;
    let disk_email = AccountStore::extract_email(&disk_auth);

    // Relay 短路：current 是中转账号 + 磁盘 auth.json 是 ApiKey schema
    // (`{"OPENAI_API_KEY": "..."}`，无 tokens 块、无 email) 是这次 v0.5.1
    // 改造后的"对路"状态——如果两边 api_key 串相等就是已同步，不要按 OAuth
    // email 比对路径走（那条会报"未知账号身份不匹配"误报）。
    if let Some(curr_id) = store.current.as_ref() {
        if let Some(curr_acc) = store.accounts.get(curr_id) {
            if curr_acc.is_relay() {
                let curr_api_key = curr_acc
                    .auth_json
                    .pointer("/tokens/access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let disk_api_key = disk_auth
                    .get("OPENAI_API_KEY")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !curr_api_key.is_empty() && curr_api_key == disk_api_key {
                    return Ok(SyncStatus {
                        is_synced: true,
                        disk_email: None,
                        matching_id: store.current.clone(),
                        current_id: store.current.clone(),
                    });
                }
            }
        }
    }

    // 快速路径：先检查磁盘 auth 与当前激活账号是否身份一致
    // 这解决了 JWT 过期/损坏导致 email 提取失败的误报问题
    let current_matches_disk = store
        .current
        .as_ref()
        .and_then(|curr_id| {
            store.accounts.get(curr_id).map(|a| {
                AccountStore::auth_identity_matches(&a.auth_json, &disk_auth)
                    || disk_email
                        .as_deref()
                        .map(|e| a.name.to_lowercase() == e.to_lowercase())
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if current_matches_disk {
        return Ok(SyncStatus {
            is_synced: true,
            disk_email,
            matching_id: store.current.clone(),
            current_id: store.current.clone(),
        });
    }

    // 慢路径：遍历所有账号匹配
    let matching_id = disk_email
        .as_deref()
        .and_then(|email| {
            let email_lower = email.to_lowercase();
            store
                .accounts
                .values()
                .find(|a| {
                    AccountStore::extract_email(&a.auth_json)
                        .map(|e| e.to_lowercase() == email_lower)
                        .unwrap_or(false)
                        || a.name.to_lowercase() == email_lower
                })
                .map(|a| a.id.clone())
        })
        .or_else(|| {
            store
                .accounts
                .values()
                .find(|a| AccountStore::auth_identity_matches(&a.auth_json, &disk_auth))
                .map(|a| a.id.clone())
        });

    let is_synced = match (&store.current, &matching_id) {
        (Some(curr), Some(match_id)) => curr == match_id,
        (None, None) => true,
        _ => false,
    };

    Ok(SyncStatus {
        is_synced,
        disk_email,
        matching_id,
        current_id: store.current.clone(),
    })
}

/// 强制将 Switcher 的激活指针对齐到磁盘账号
/// 安全策略：只修改激活指针，绝不覆盖已有账号的 Token 数据
#[tauri::command]
fn sync_active_with_disk(state: State<AppState>, app: tauri::AppHandle) -> Result<(), String> {
    let disk_auth = AccountStore::read_codex_auth()?;
    let disk_email = AccountStore::extract_email(&disk_auth);
    let mut store = state.store.lock().map_err(|e| e.to_string())?;

    // 手机锚模式（v0.7+）：disk 故意锁在 anchor 上，"按 disk 对齐 current" 等于
    // 把 current 强拉回 anchor —— 破坏整个 anchor 设计的目的。直接拒绝。
    // 用户想换 anchor 应该走 set_session_anchor，想离开 anchor 模式应该先取消 anchor。
    if let Some(anchor_id) = store.session_anchor_id() {
        if store.current.as_deref() != Some(anchor_id.as_str()) {
            return Err(
                "手机锚生效中：disk 是 anchor 的镜像，不能用它对齐 current。\
                 想离开 anchor 模式请先在 anchor 账号上点 📱 按钮取消"
                    .to_string(),
            );
        }
    }

    // 优先用 JWT Email 匹配（最可靠），其次才用 account_id
    let matching_id = disk_email
        .as_deref()
        .and_then(|email| {
            let email_lower = email.to_lowercase();
            store
                .accounts
                .values()
                .find(|a| {
                    AccountStore::extract_email(&a.auth_json)
                        .map(|e| e.to_lowercase() == email_lower)
                        .unwrap_or(false)
                        || a.name.to_lowercase() == email_lower
                })
                .map(|a| a.id.clone())
        })
        .or_else(|| {
            // fallback: account_id 匹配
            store
                .accounts
                .values()
                .find(|a| AccountStore::auth_identity_matches(&a.auth_json, &disk_auth))
                .map(|a| a.id.clone())
        })
        .ok_or_else(|| "磁盘账号不在管理列表中，请先导入".to_string())?;

    // 安全：只改指针，不覆盖 Token。避免封号 Token 污染好号。
    store.current = Some(matching_id);
    store.save()?;

    crate::tray::update_tray_menu(&app);
    Ok(())
}

// ==================== Remote Mode Tauri Commands ====================

/// 生成 32 字节随机 shared secret（UI 启用 server 模式时调用）
#[tauri::command]
fn remote_generate_secret() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    // 用 base64 URL safe 编码，避免特殊字符
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(buf)
}

/// 读取 client 配置快照：返回 (primary_url, fallback_url, secret)
fn client_settings_snapshot_raw(
    state: &State<AppState>,
) -> Result<(String, String, String), String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    if store.settings.remote_server_url.is_empty()
        && store.settings.remote_server_url_fallback.is_empty()
    {
        return Err("未配置 Server 地址".to_string());
    }
    if store.settings.remote_shared_secret.is_empty() {
        return Err("未配置共享密钥".to_string());
    }
    Ok((
        store.settings.remote_server_url.clone(),
        store.settings.remote_server_url_fallback.clone(),
        store.settings.remote_shared_secret.clone(),
    ))
}

/// 解析出当前可用 URL（primary → fallback），返回 (url, secret)
async fn client_settings_snapshot(state: &State<'_, AppState>) -> Result<(String, String), String> {
    let (primary, fallback, secret) = client_settings_snapshot_raw(state)?;
    let url = remote_client::resolve_base_url(&primary, &fallback).await?;
    Ok((url, secret))
}

#[tauri::command]
async fn remote_health(base_url: String) -> Result<remote_client::RemoteHealth, String> {
    remote_client::health(&base_url).await
}

#[tauri::command]
async fn remote_test_auth(
    base_url: String,
    secret: String,
) -> Result<remote_client::RemoteHealth, String> {
    remote_client::test_auth(&base_url, &secret).await
}

/// 用当前 settings 的 primary + fallback 双探测，返回 (url_in_use, health)
#[tauri::command]
async fn remote_probe(
    state: State<'_, AppState>,
) -> Result<(String, remote_client::RemoteHealth), String> {
    remote_client::invalidate_cached_url();
    let (primary, fallback, secret) = client_settings_snapshot_raw(&state)?;
    let url = remote_client::resolve_base_url(&primary, &fallback).await?;
    let h = remote_client::test_auth(&url, &secret).await?;
    Ok((url, h))
}

#[tauri::command]
async fn remote_push_account(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    id: String,
) -> Result<remote_client::UpsertOutcome, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    let account = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store
            .list_accounts()
            .into_iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| format!("本地未找到账号 {}", id))?
    };
    let outcome = remote_client::upsert_account(&url, &secret, &account).await?;
    // 若 Server 按邮箱+身份合并到了旧 id，本机也把这个账号的 id 改过去，避免下次推又走 merged 分支
    if outcome.upserted == "merged" && outcome.id != id {
        let new_id = outcome.id.clone();
        if let Ok(mut store) = state.store.lock() {
            if let Some(mut acc) = store.accounts.remove(&id) {
                acc.id = new_id.clone();
                store.accounts.insert(new_id.clone(), acc);
                if store.current.as_deref() == Some(id.as_str()) {
                    store.current = Some(new_id);
                }
                let _ = store.save();
            }
        }
        let _ = app.emit("accounts-updated", ());
    }
    Ok(outcome)
}

#[tauri::command]
async fn remote_push_all(state: State<'_, AppState>) -> Result<usize, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    let accounts: Vec<Account> = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store.list_accounts().into_iter().cloned().collect()
    };
    let mut ok = 0usize;
    for a in accounts.iter() {
        remote_client::upsert_account(&url, &secret, a).await?;
        ok += 1;
    }
    Ok(ok)
}

#[tauri::command]
async fn remote_pull_all(state: State<'_, AppState>) -> Result<usize, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    let remote_accounts = remote_client::list_accounts(&url, &secret).await?;
    let mut merged = 0usize;
    let mut store = state.store.lock().map_err(|e| e.to_string())?;
    for ra in remote_accounts {
        store.accounts.insert(ra.id.clone(), ra);
        merged += 1;
    }
    store.save()?;
    Ok(merged)
}

#[derive(serde::Serialize)]
struct RemoteTokenSyncReport {
    pulled: usize,
    refreshed: usize,
    current: Option<String>,
    current_name: Option<String>,
    wrote_auth_json: bool,
    errors: Vec<(String, String)>,
}

/// 从 Server 逐个拉取每个账号的最新 token（/accounts/:id/token），合并到本机 store。
/// 若 Server 的 current 在本机存在，同时写 ~/.codex/auth.json 并更新本机 current。
#[tauri::command]
async fn remote_pull_all_tokens(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<RemoteTokenSyncReport, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    // 先整体 list 一遍，确保本机有所有账号元数据
    let remote_accounts = remote_client::list_accounts(&url, &secret).await?;
    let pulled = remote_accounts.len();
    {
        let mut store = state.store.lock().map_err(|e| e.to_string())?;
        for ra in remote_accounts.iter() {
            store.accounts.insert(ra.id.clone(), ra.clone());
        }
        store.save()?;
    }
    // 逐个拉 token（/accounts/:id/token 返回 Server 上最新的 auth_json）
    let mut refreshed = 0usize;
    let mut errors: Vec<(String, String)> = Vec::new();
    let ids: Vec<String> = remote_accounts.iter().map(|a| a.id.clone()).collect();
    for id in ids.iter() {
        match remote_client::fetch_token(&url, &secret, id).await {
            Ok(t) => {
                if let Ok(mut store) = state.store.lock() {
                    store.sync_account_from_auth_json(id, t.auth_json);
                    let _ = store.save();
                }
                refreshed += 1;
            }
            Err(e) => errors.push((id.clone(), e)),
        }
    }
    // 处理 Server 的 current：若本机有该账号，则写 auth.json + 更新 current
    let cur = remote_client::get_current(&url, &secret).await.ok();
    let mut wrote_auth_json = false;
    let (cur_id, cur_name) = if let Some(c) = cur.as_ref() {
        (c.current.clone(), c.name.clone())
    } else {
        (None, None)
    };
    if let Some(cid) = cur_id.as_ref() {
        let (auth_opt, allow_disk) = {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            (
                store.accounts.get(cid).map(|a| a.auth_json.clone()),
                store.should_write_disk_for(cid),
            )
        };
        if let Some(auth) = auth_opt {
            if !allow_disk {
                // 手机锚生效：不覆盖 anchor 的磁盘镜像，只对齐 current
                if let Ok(mut store) = state.store.lock() {
                    store.current = Some(cid.clone());
                    let _ = store.save();
                }
                println!("[RemotePull] 手机锚生效，跳过写 ~/.codex/auth.json（current={} != anchor）", cid);
            } else if let Err(e) = account::AccountStore::write_codex_auth(&auth) {
                errors.push((cid.clone(), format!("写 auth.json 失败: {}", e)));
            } else {
                wrote_auth_json = true;
                if let Ok(mut store) = state.store.lock() {
                    store.current = Some(cid.clone());
                    let _ = store.save();
                }
            }
        }
    }
    let _ = app.emit("accounts-updated", ());
    Ok(RemoteTokenSyncReport {
        pulled,
        refreshed,
        current: cur_id,
        current_name: cur_name,
        wrote_auth_json,
        errors,
    })
}

#[tauri::command]
async fn remote_delete_account_cmd(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    remote_client::delete_account(&url, &secret, &id).await
}

#[tauri::command]
async fn remote_fetch_token(
    state: State<'_, AppState>,
    id: String,
) -> Result<remote_client::RemoteToken, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    remote_client::fetch_token(&url, &secret, &id).await
}

/// client 模式下由 Server 完成一次 token 刷新 + usage 拉取，并把结果同步到本机 cached_quota。
/// 如果 Server 那边没有这个账号（404 / not_found），fallback 到本地直查（用本机 store 里的 token）。
#[tauri::command]
async fn remote_refresh_account_quota(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    id: String,
) -> Result<UsageDisplay, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    match remote_client::refresh_account_quota(&url, &secret, &id).await {
        Ok(usage) => {
            if let Ok(mut store) = state.store.lock() {
                if let Some(acc) = store.accounts.get_mut(&id) {
                    acc.cached_quota = Some(usage_to_cached(&usage));
                    acc.is_banned = false;
                    acc.is_token_invalid = false;
                    acc.is_logged_out = false;
                    let _ = store.save();
                }
            }
            let _ = app.emit("accounts-updated", ());
            Ok(usage)
        }
        Err(e) => {
            // Server 那边可能根本没有这个账号（典型场景：刚批量导入到本机的账号还没推到 Server）
            // → fallback 到本地直查，用本机 store 里的 token / refresh_token 跑一次 fetch_usage_direct
            let lower = e.to_lowercase();
            let is_missing = lower.contains("not_found")
                || lower.contains("not found")
                || lower.contains("404")
                || lower.contains("account") && lower.contains("not");
            if !is_missing {
                return Err(e);
            }
            println!(
                "[Quota] Server 没有账号 {}，fallback 到本地直查（可能是刚导入未推 Server）",
                id
            );
            get_quota_by_id(state, app, id).await
        }
    }
}

#[derive(serde::Serialize)]
struct SkillSyncReport {
    pushed: Vec<String>,
    skipped: Vec<String>,
    errors: Vec<(String, String)>,
}

/// 本机 → Server 单向同步所有 skills（按黑名单跳过）
#[tauri::command]
async fn remote_sync_skills(state: State<'_, AppState>) -> Result<SkillSyncReport, String> {
    let (url, secret) = client_settings_snapshot(&state).await?;
    let blacklist: std::collections::HashSet<String> = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store
            .settings
            .skills_sync_blacklist
            .iter()
            .cloned()
            .collect()
    };
    let names = skills::list_local_skill_dirs();
    let mut pushed = Vec::new();
    let mut skipped = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    for name in names {
        if blacklist.contains(&name) {
            skipped.push(name);
            continue;
        }
        let zip_result = {
            let name = name.clone();
            tokio::task::spawn_blocking(move || skills::zip_skill_dir(&name))
                .await
                .map_err(|e| format!("zip task 崩溃: {}", e))?
        };
        let bytes = match zip_result {
            Ok(b) => b,
            Err(e) => {
                errors.push((name, e));
                continue;
            }
        };
        match remote_client::upload_skill(&url, &secret, &name, bytes).await {
            Ok(_) => pushed.push(name),
            Err(e) => errors.push((name, e)),
        }
    }
    Ok(SkillSyncReport {
        pushed,
        skipped,
        errors,
    })
}

/// 按当前 settings 启动/重启 server 端 HTTP API（便于 UI 切换模式后不用重启 App）
#[tauri::command]
fn remote_restart_server(state: State<AppState>, app: tauri::AppHandle) -> Result<String, String> {
    let (mode, port, bind, secret) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        (
            store.settings.remote_mode.clone(),
            store.settings.remote_server_port,
            store.settings.remote_server_bind.clone(),
            store.settings.remote_shared_secret.clone(),
        )
    };
    // 停掉旧的
    {
        let mut slot = state
            .remote_server_handle
            .lock()
            .map_err(|e| e.to_string())?;
        if let Some(h) = slot.take() {
            h.abort();
        }
    }
    if mode != "server" {
        return Ok(format!("已停止（当前模式 {}）", mode));
    }
    if secret.is_empty() {
        return Err("共享密钥为空，请先生成".to_string());
    }
    let handle = remote_server::spawn_remote_server(
        state.store.clone(),
        bind.clone(),
        port,
        secret,
        env!("CARGO_PKG_VERSION").to_string(),
        app,
    );
    let mut slot = state
        .remote_server_handle
        .lock()
        .map_err(|e| e.to_string())?;
    *slot = Some(handle);
    Ok(format!("已启动 http://{}:{}", bind, port))
}

// ==================== end Remote Mode commands ====================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// 退出兜底：把 anchor 的磁盘 expires_at 从撒谎的 +24h 恢复成真实值。
/// 详见 `AccountStore::restore_disk_real_expiry_for_anchor` 注释。
fn restore_anchor_disk_on_exit(reason: &str) {
    let Some(store) = GLOBAL_STORE_FOR_EXIT.get() else {
        return;
    };
    let Ok(guard) = store.lock() else {
        return;
    };
    match guard.restore_disk_real_expiry_for_anchor() {
        Ok(true) => eprintln!(
            "[{}] 已恢复 ~/.codex/auth.json 真实 expires_at（anchor 释放给 Codex.app）",
            reason
        ),
        Ok(false) => {}
        Err(e) => eprintln!("[{}] anchor expires_at 恢复失败: {}", reason, e),
    }
}

pub fn run() {
    // 把 stdout/stderr 重定向到 ~/.codex-switcher/proxy.log
    // 兼容 GUI 启动（Mac App double-click / Tauri build），让所有 println! / eprintln! 落盘
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".codex-switcher");
        let _ = std::fs::create_dir_all(&dir);
        let log_path = dir.join("proxy.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            redirect_stdout_stderr_to_file(file);
            eprintln!(
                "\n=== codex-switcher started {} pid={} ===",
                chrono::Utc::now().to_rfc3339(),
                std::process::id()
            );
        }
    }

    // panic 兜底：跟 RunEvent::Exit 走同一条恢复路径
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_anchor_disk_on_exit("Panic");
        prev_hook(info);
    }));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(AppState::new())
        .setup(|app| {
            // ── Deep link 监听：codexswitch:// + ccswitch:// ──
            // 收到 URL 后解析，把结果 emit 到前端"deep-link://import-pending"事件，
            // 由前端弹确认框，用户点"导入"才会调 add_relay_account 落库。
            use tauri_plugin_deep_link::DeepLinkExt;
            let dl_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let url_str = url.to_string();
                    match deep_link::parse(&url_str) {
                        Ok(payload) => {
                            println!("[DeepLink] 解析成功: {} → {}", payload.source, payload.name);
                            if let Some(window) = dl_handle.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                            if let Err(e) = dl_handle.emit("deep-link://import-pending", &payload) {
                                eprintln!("[DeepLink] emit 失败: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("[DeepLink] 解析失败 ({}): {}", url_str, e);
                        }
                    }
                }
            });
            // 初始化系统托盘
            if let Err(e) = tray::init(app.handle()) {
                eprintln!("初始化托盘失败: {:?}", e);
            }

            // 启动后台调度器（仅在设置开启时）
            let state = app.state::<AppState>();
            let should_start = state
                .store
                .lock()
                .map(|store| store.settings.background_refresh)
                .unwrap_or(false);
            if should_start {
                let handle = scheduler::start(state.store.clone(), app.handle().clone());
                let mut scheduler_handle = state.scheduler.lock().unwrap();
                *scheduler_handle = Some(handle);
            } else {
                println!("[Scheduler] 后台刷新未开启，跳过启动");
            }

            // 启动本地代理（仅在设置开启时）
            let (proxy_enabled, proxy_port, proxy_allow_lan) = state
                .store
                .lock()
                .map(|s| {
                    (
                        s.settings.proxy_enabled,
                        s.settings.proxy_port,
                        s.settings.proxy_allow_lan,
                    )
                })
                .unwrap_or((false, 18080, false));
            if proxy_enabled {
                let handle = proxy::start(
                    state.store.clone(),
                    proxy_port,
                    proxy_allow_lan,
                    app.handle().clone(),
                    state.proxy_stats.clone(),
                    state.token_tracker.clone(),
                    state.ws_disconnect.clone(),
                    state.switch_logger.clone(),
                    state.session_affinity.clone(),
                    state.session_routes.clone(),
                );
                let mut proxy_handle = state.proxy_handle.lock().unwrap();
                *proxy_handle = Some(handle);
                println!("[Proxy] 代理已随应用启动 (端口 {})", proxy_port);
            } else {
                println!("[Proxy] 本地代理未开启，跳过启动");
            }

            // 启动 Remote Mode HTTP API（仅在 mode=server 时）
            let (remote_mode, remote_port, remote_bind, remote_secret) = state
                .store
                .lock()
                .map(|s| {
                    (
                        s.settings.remote_mode.clone(),
                        s.settings.remote_server_port,
                        s.settings.remote_server_bind.clone(),
                        s.settings.remote_shared_secret.clone(),
                    )
                })
                .unwrap_or((
                    "off".to_string(),
                    18081,
                    "0.0.0.0".to_string(),
                    String::new(),
                ));
            if remote_mode == "server" {
                if remote_secret.is_empty() {
                    eprintln!("[RemoteServer] shared_secret 为空，拒绝启动（请在 UI 配置）");
                } else {
                    let handle = remote_server::spawn_remote_server(
                        state.store.clone(),
                        remote_bind,
                        remote_port,
                        remote_secret,
                        env!("CARGO_PKG_VERSION").to_string(),
                        app.handle().clone(),
                    );
                    let mut slot = state.remote_server_handle.lock().unwrap();
                    *slot = Some(handle);
                }
            } else {
                println!("[RemoteServer] Remote Mode 未启用（mode={}）", remote_mode);
            }

            // 启动定时额度刷新（client 模式无条件启动，它承担 Server 状态同步）
            let should_run_quota_loop = state
                .store
                .lock()
                .map(|s| s.settings.quota_refresh_enabled || s.settings.remote_mode == "client")
                .unwrap_or(false);
            if should_run_quota_loop {
                let handle = start_quota_refresh(state.store.clone(), app.handle().clone());
                let mut qr = state.quota_refresh_handle.lock().unwrap();
                *qr = Some(handle);
                println!("[QuotaRefresh] 启动中（setup 阶段）");
            }

            // 启动时立刻跑一次同步，把 store/disk 不一致 + 落后的 RT 立即对齐
            let store_for_init = state.store.clone();
            tauri::async_runtime::spawn(async move {
                if do_one_fast_auth_sync(&store_for_init).await {
                    println!("[FastAuthSync] 启动时同步完成");
                }
            });

            // 启动时把本地已有 Relay 账号 upsert 到 Server（升级路径迁移）
            // 旧版 add_relay_account 没 push 到 Server，新版 prune 不再特殊跳过 Relay，
            // 不预先 push 一次会被下一轮 quota_refresh 当残留删掉。
            let store_for_relay = state.store.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                let (mode, primary, fallback, secret) = {
                    let s = match store_for_relay.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    (
                        s.settings.remote_mode.clone(),
                        s.settings.remote_server_url.clone(),
                        s.settings.remote_server_url_fallback.clone(),
                        s.settings.remote_shared_secret.clone(),
                    )
                };
                if !account::pushes_to_server(&mode) || secret.is_empty() {
                    return;
                }
                let url = match remote_client::resolve_base_url(&primary, &fallback).await {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("[RelayPushOnStart] Server 不可达，跳过: {}", e);
                        return;
                    }
                };
                let relays: Vec<Account> = match store_for_relay.lock() {
                    Ok(s) => s
                        .accounts
                        .values()
                        .filter(|a| a.is_relay())
                        .cloned()
                        .collect(),
                    Err(_) => return,
                };
                if relays.is_empty() {
                    return;
                }
                println!(
                    "[RelayPushOnStart] 把 {} 个本地 Relay 账号 upsert 到 Server",
                    relays.len()
                );
                for acc in relays {
                    match remote_client::upsert_account(&url, &secret, &acc).await {
                        Ok(o) => println!(
                            "[RelayPushOnStart] {} → {} ({})",
                            acc.name, o.id, o.upserted
                        ),
                        Err(e) => {
                            eprintln!("[RelayPushOnStart] {} 失败: {}", acc.name, e)
                        }
                    }
                }
            });
            // 快速 auth.json 同步循环（client 模式专用，但循环内自检模式，可以无脑启动）
            let _fast_auth_handle = start_fast_auth_sync(state.store.clone());

            // 手机锚保活循环（无 anchor 时空转，不影响无该功能的用户）
            let _anchor_handle = scheduler::start_anchor_refresh(
                state.store.clone(),
                app.handle().clone(),
            );

            // client 模式下 server_url 空 → 用户配置错位，明确警告
            if let Ok(s) = state.store.lock() {
                if s.settings.remote_mode == "client"
                    && s.settings.remote_server_url.trim().is_empty()
                    && s.settings.remote_server_url_fallback.trim().is_empty()
                {
                    eprintln!(
                        "[Config] ⚠️ client 模式但 remote_server_url 为空 —— Server 不可达，本机将退回直连本地账号。\n\
                         去 设置 → 远程模式 填上 Server 地址（比如 http://192.168.2.14:18081）。"
                    );
                }
            }

            // solo 模式心跳循环（向 Server 声明"本机接管保活"）
            if remote_mode == "solo" {
                let handle =
                    start_solo_heartbeat(state.store.clone(), app.handle().clone());
                let mut slot = state.solo_heartbeat_handle.lock().unwrap();
                *slot = Some(handle);
                println!("[Solo] 心跳循环启动");
            }

            // 初始化 Skills SSOT + 自动导入
            if let Err(e) = skills::init_ssot() {
                eprintln!("[Skills] SSOT 初始化失败: {}", e);
            }
            {
                let mut data = skills::SkillStore::load();
                let count = skills::SkillStore::scan_existing(&mut data);
                if count > 0 {
                    let _ = skills::SkillStore::save(&data);
                    println!("[Skills] 自动导入 {} 个已有 skill", count);
                }
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 拦截关闭事件，改为隐藏窗口并从 Dock 隐藏
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                // macOS: 隐藏 Dock 图标，变成纯后台托盘应用
                #[cfg(target_os = "macos")]
                {
                    let app = window.app_handle();
                    app.set_activation_policy(tauri::ActivationPolicy::Accessory)
                        .unwrap_or(());
                }
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_accounts,
            get_current_account_id,
            import_current_account,
            switch_account,
            sync_current_auth_to_account,
            delete_account,
            update_account,
            update_relay_usage_cookie,
            set_account_inactive_refresh_enabled,
            set_session_anchor,
            export_accounts,
            import_accounts,
            add_relay_account,
            update_relay_model_map,
            refresh_relay_usage,
            bulk_import_accounts,
            check_codex_login,
            get_quota_by_id,
            send_codex_invite,
            send_codex_wakeup,
            open_codex_terminal,
            oauth_server::start_oauth_login,
            oauth_server::submit_oauth_callback,
            oauth_server::copy_to_clipboard,
            session_import::import_chatgpt_session,
            solo_sync_current,
            finalize_oauth_login,
            force_overwrite_disk_with_current,
            start_otp_login_batch,
            reload_ide_windows,
            get_settings,
            update_settings,
            get_proxy_status,
            kill_codex_processes,
            set_proxy_env,
            get_token_stats,
            reset_token_stats,
            show_main_window_cmd,
            set_codex_fast_mode,
            get_codex_fast_mode,
            set_codex_features_goals,
            get_codex_features_goals,
            get_token_history,
            get_quota_cycles,
            get_plan_capacity_estimates,
            get_account_token_history,
            get_session_bindings,
            list_session_routes,
            add_session_route,
            delete_session_route,
            toggle_session_route,
            update_session_route_label,
            list_codex_sessions,
            detect_active_codex_session,
            force_auth_resync,
            get_switch_history,
            get_switch_stats,
            get_installed_skills,
            get_skill_repos,
            add_skill_repo,
            remove_skill_repo,
            discover_skills,
            install_skill,
            uninstall_skill,
            toggle_skill_app_link,
            get_skill_app_status,
            get_skill_content,
            scan_and_import_skills,
            sync_all_skills,
            check_sync_conflict,
            request_quarantine_fix_ticket,
            fix_codex_quarantine,
            get_sync_status,
            sync_active_with_disk,
            remote_generate_secret,
            remote_health,
            remote_test_auth,
            remote_probe,
            remote_push_account,
            remote_push_all,
            remote_pull_all,
            remote_pull_all_tokens,
            remote_delete_account_cmd,
            remote_fetch_token,
            remote_refresh_account_quota,
            remote_sync_skills,
            remote_restart_server,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            // 退出兜底：把 anchor 的 expires_at 在磁盘上恢复成真实值，
            // 让 Codex.app 在 codex-switcher 死掉之后能自己 refresh（而不是
            // 拿着撒谎的 +24h expires_at 继续用导致手机 bridge 静默 401）。
            if matches!(event, tauri::RunEvent::Exit) {
                restore_anchor_disk_on_exit("Exit");
            }
        });
}

#[cfg(unix)]
fn redirect_stdout_stderr_to_file(file: std::fs::File) {
    use std::os::unix::io::IntoRawFd;

    let fd = file.into_raw_fd();
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
        libc::close(fd);
    }
}

#[cfg(not(unix))]
fn redirect_stdout_stderr_to_file(_file: std::fs::File) {}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_auth(account_id: &str, refresh_token: &str) -> serde_json::Value {
        serde_json::json!({
            "tokens": {
                "account_id": account_id,
                "refresh_token": refresh_token
            }
        })
    }

    fn test_account(name: &str, account_id: &str, refresh_token: &str) -> Account {
        let auth_json = test_auth(account_id, refresh_token);
        Account {
            id: "acc-1".to_string(),
            name: name.to_string(),
            auth_json: auth_json.clone(),
            refresh_token: AccountStore::extract_refresh_token(&auth_json),
            created_at: Utc::now(),
            last_used: None,
            notes: None,
            cached_quota: None,
            keepalive: account::KeepaliveState::default(),
            is_banned: false,
            is_token_invalid: false,
            is_logged_out: false,
            kind: account::AccountKind::Legacy,
            relay_base_url: None,
            relay_homepage: None,
            relay_usage_preset: None,
            relay_usage_cookie: None,
            relay_usage_cache: None,
            relay_model_map: None,
            relay_model_fallback: None,
            relay_protocol: None,
            relay_category: None,
            is_session_anchor: false,
        }
    }

    #[test]
    fn quota_refresh_never_allows_local_token_refresh() {
        assert!(!allow_local_refresh_for_quota(true));
        assert!(!allow_local_refresh_for_quota(false));
    }

    #[test]
    fn sync_conflict_is_ignored_when_identity_mismatch() {
        let current = test_account("current", "acct-local", "rt-local");
        let disk_auth = test_auth("acct-disk", "rt-new");

        assert_eq!(detect_sync_conflict_for_current(&current, &disk_auth), None);
    }

    #[test]
    fn sync_conflict_is_reported_when_identity_matches_and_refresh_token_changed() {
        let current = test_account("current", "acct-1", "rt-local");
        let disk_auth = test_auth("acct-1", "rt-new");

        assert_eq!(
            detect_sync_conflict_for_current(&current, &disk_auth),
            Some("current".to_string())
        );
    }

    #[test]
    fn quarantine_fix_ticket_can_only_be_used_once() {
        let state = AppState::new();
        let ticket = state.issue_quarantine_fix_ticket().unwrap();

        assert!(state.consume_quarantine_fix_ticket(&ticket).is_ok());
        assert!(state.consume_quarantine_fix_ticket(&ticket).is_err());
    }

    #[test]
    fn quarantine_fix_ticket_rejects_mismatch() {
        let state = AppState::new();
        let _ticket = state.issue_quarantine_fix_ticket().unwrap();

        assert!(state.consume_quarantine_fix_ticket("wrong-ticket").is_err());
    }

    #[test]
    fn quarantine_fix_ticket_rejects_expired_ticket() {
        let state = AppState::new();
        {
            let mut slot = state.quarantine_fix_ticket.lock().unwrap();
            *slot = Some(QuarantineFixTicket {
                value: "expired".to_string(),
                expires_at: Utc::now() - chrono::Duration::seconds(1),
            });
        }

        let err = state
            .consume_quarantine_fix_ticket("expired")
            .expect_err("expired ticket should be rejected");
        assert!(err.contains("过期"));
    }
}
