//! Codex Switcher - Tauri 主入口
//!
//! 暴露所有 Tauri 命令供前端调用

mod account;
mod bulk_import;
mod deep_link;
mod ide_control;
pub mod mailbox;
pub mod oauth;
mod oauth_server;
pub mod otp_login;
pub mod output_compress;
pub mod sentinel;
mod proxy;
mod refresh_lock;
mod remote_client;
mod remote_server;
mod scheduler;
mod session_affinity;
mod skills;
mod switch_log;
mod token_tracker;
mod tray;
mod usage;

use account::{Account, AccountStore};
use chrono::Utc;
use refresh_lock::RefreshLockManager;
use std::process::Command;
use tauri::{Emitter, Manager, State};
use usage::{UsageDisplay, UsageFetcher};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

const QUARANTINE_FIX_TICKET_TTL_SECS: i64 = 120;

// ─── 全局：shell tool 输出压缩统计 ─────────────────────────────────
// 跨请求累计，UI 可通过 get_compression_stats / reset_compression_stats 拉取/清零
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressStats {
    pub frames_seen: u64,
    pub frames_compressed: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

static COMPRESS_STATS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<CompressStats>>> =
    std::sync::OnceLock::new();

pub fn compress_stats() -> std::sync::Arc<std::sync::Mutex<CompressStats>> {
    COMPRESS_STATS
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(CompressStats::default())))
        .clone()
}

/// 给 proxy 调用：每次看到一个 shell tool 输出帧（无论是否真截断）就更新统计。
pub fn record_compress_frame(was_compressed: bool, bytes_in: u64, bytes_out: u64) {
    let stats = compress_stats();
    let mut guard = match stats.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    guard.frames_seen += 1;
    if was_compressed {
        guard.frames_compressed += 1;
    }
    guard.bytes_in = guard.bytes_in.saturating_add(bytes_in);
    guard.bytes_out = guard.bytes_out.saturating_add(bytes_out);
}

#[tauri::command]
fn get_compression_stats() -> CompressStats {
    let stats = compress_stats();
    let guard = match stats.lock() {
        Ok(g) => g,
        Err(_) => return CompressStats::default(),
    };
    guard.clone()
}

#[tauri::command]
fn reset_compression_stats() {
    let stats = compress_stats();
    let mut guard = match stats.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    *guard = CompressStats::default();
}

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
    pub quota_refresh_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub refresh_locks: RefreshLockManager,
    pub remote_server_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub solo_heartbeat_handle: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    quarantine_fix_ticket: std::sync::Mutex<Option<QuarantineFixTicket>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            store: std::sync::Arc::new(std::sync::Mutex::new(AccountStore::load())),
            scheduler: std::sync::Mutex::new(None),
            proxy_handle: std::sync::Mutex::new(None),
            proxy_stats: std::sync::Arc::new(proxy::ProxyStats::default()),
            token_tracker: token_tracker::TokenTracker::new(),
            ws_disconnect: std::sync::Arc::new(tokio::sync::Notify::new()),
            switch_logger: switch_log::SwitchLogger::new(),
            session_affinity: std::sync::Arc::new(session_affinity::SessionAffinity::new()),
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
        let mut slot = state.solo_heartbeat_handle.lock().map_err(|e| e.to_string())?;
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
        let existing_emails: std::collections::HashSet<String> = store
            .accounts
            .values()
            .map(|a| a.name.clone())
            .collect();
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
    if account::pushes_to_server(&remote_mode) && !secret.is_empty() && !newly_added_ids.is_empty() {
        let store_arc = state.store.clone();
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            let base = match remote_client::resolve_base_url(&server_url, &server_url_fallback).await {
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
async fn add_relay_account(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    name: String,
    base_url: String,
    api_key: String,
    homepage: Option<String>,
    usage_preset: Option<String>,
    notes: Option<String>,
    model_map: Option<std::collections::HashMap<String, String>>,
    model_fallback: Option<String>,
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
            homepage.map(|h| h.trim().to_string()).filter(|h| !h.is_empty()),
            usage_preset.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()),
            notes,
            model_map,
            model_fallback.map(|f| f.trim().to_string()).filter(|f| !f.is_empty()),
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
                        Err(e) => eprintln!(
                            "[Relay] 推送 Server 失败（账号已本地保存）: {}",
                            e
                        ),
                    }
                }
            }
            Err(e) => eprintln!("[Relay] 读取 client 配置失败，未推送 Server: {}", e),
        }
    }

    crate::tray::update_tray_menu(&app);
    Ok(account)
}

/// 更新 Relay 账号的模型映射 / 兜底（编辑功能用）。
#[tauri::command]
fn update_relay_model_map(
    state: State<AppState>,
    id: String,
    model_map: Option<std::collections::HashMap<String, String>>,
    model_fallback: Option<String>,
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
    let (base_url, api_key, preset) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let acc = store.accounts.get(&id).ok_or("账号不存在")?;
        if !acc.is_relay() {
            return Err("不是中转站账号".into());
        }
        let base = acc.relay_base_url.clone().ok_or("中转站账号缺 base_url")?;
        let key = AccountStore::extract_access_token(&acc.auth_json)
            .ok_or("中转站账号缺 api_key")?;
        let preset = acc.relay_usage_preset.clone();
        (base, key, preset)
    };

    let cache = match preset.as_deref() {
        Some("openai_compat") | None => {
            UsageFetcher::fetch_relay_usage_openai_compat(&base_url, &api_key).await?
        }
        Some("glm_zhipu") => {
            UsageFetcher::fetch_relay_usage_glm_zhipu(&base_url, &api_key).await?
        }
        Some(other) => return Err(format!("未支持的 usage_preset: {}", other)),
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
    save_token_as_account(&state, &app, token_res, Some("OpenAI OAuth 登录".to_string())).await
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

        if needs_refresh && refresh_token.is_some() {
            if let Some(ref rt) = refresh_token {
                match oauth::refresh_access_token(rt).await {
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
    match usage::UsageFetcher::fetch_usage_direct(access_token, account_id, refresh_token, false)
        .await
    {
        Ok((usage, _)) => {
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
pub async fn do_one_fast_auth_sync(
    store: &std::sync::Arc<std::sync::Mutex<AccountStore>>,
) -> bool {
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
    let Some(cid) = target_cid else { return false; };

    // 2) 拉 cid 的最新 token 写盘
    match remote_client::fetch_token(&base, &secret, &cid).await {
        Ok(t) => {
            if let Ok(mut s) = store.lock() {
                s.sync_account_from_auth_json(&cid, t.auth_json.clone());
                // 把本机 current 也对齐上（如果之前不一致）
                s.current = Some(cid.clone());
                let _ = s.save();
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
            let (enabled, interval_minutes, batch_size, remote_mode, primary, fallback, secret) = {
                let s = store.lock().unwrap();
                (
                    s.settings.quota_refresh_enabled,
                    s.settings.quota_refresh_interval.max(1),
                    s.settings.quota_refresh_batch.max(1),
                    s.settings.remote_mode.clone(),
                    s.settings.remote_server_url.clone(),
                    s.settings.remote_server_url_fallback.clone(),
                    s.settings.remote_shared_secret.clone(),
                )
            };

            // client 模式：强制从 Server 拉 /quotas，忽略 enabled 开关（开关对 client 无意义）
            // server/off 模式：遵循 enabled 开关
            if remote_mode == "client" {
                if secret.is_empty() {
                    println!("[QuotaRefresh] client 模式但未配置 secret，跳过本轮");
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
                                let remote_ids: std::collections::HashSet<String> =
                                    entries.iter().map(|e| e.id.clone()).collect();
                                let (updated, pruned) = {
                                    let mut updated = 0usize;
                                    let mut pruned = 0usize;
                                    if let Ok(mut s) = store.lock() {
                                        // 1) 同步 quota/封禁/失效状态
                                        for e in &entries {
                                            if let Some(acc) = s.accounts.get_mut(&e.id) {
                                                if let Some(q) = e.cached_quota.clone() {
                                                    acc.cached_quota = Some(q);
                                                    updated += 1;
                                                }
                                                acc.is_banned = e.is_banned;
                                                acc.is_token_invalid = e.is_token_invalid;
                                                acc.is_logged_out = e.is_logged_out;
                                            }
                                        }
                                        // 2) 删除 Server 上已不存在的账号（多端删号同步）
                                        // Relay 账号也参与 prune：add_relay_account 现在会 upsert 到 Server，
                                        // Server 上有这账号就不会被 prune。
                                        let local_ids: Vec<String> =
                                            s.accounts.keys().cloned().collect();
                                        for id in local_ids {
                                            if !remote_ids.contains(&id) {
                                                s.accounts.remove(&id);
                                                pruned += 1;
                                                if s.current.as_deref() == Some(id.as_str()) {
                                                    s.current = None;
                                                }
                                            }
                                        }
                                        let _ = s.save();
                                    }
                                    (updated, pruned)
                                };
                                // 3) 同步 Server 的 current 到本机（UI 统一 + 本机 Codex CLI 用同一个号）
                                //    - 拉 Server 最新 token
                                //    - 写本机 store（accounts.json） + 官方 ~/.codex/auth.json
                                //    - 更新本机 current 指针
                                //    规则：若 Server 正常，本机始终跟随 Server 的 current。
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
                                                    if let Ok(mut s) = store.lock() {
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
                                                    }
                                                    // client 模式：用 extended_expiry 防 codex 自刷
                                                    if let Err(e) =
                                                        account::AccountStore::write_codex_auth_extended_expiry(
                                                            &t.auth_json,
                                                        )
                                                    {
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
                                println!(
                                    "[QuotaRefresh] client 从 Server 同步 {} 个额度，删除本地残留 {} 个",
                                    updated, pruned
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

            // 按 cached_quota.updated_at 排序，最旧的优先
            // Relay 账号的 quota 通过专属 fetcher 拉取，这里跳过避免无谓打 OpenAI usage API
            let targets: Vec<(String, String)> = {
                let s = store.lock().unwrap();
                let mut accounts: Vec<_> = s
                    .accounts
                    .values()
                    .filter(|a| {
                        !a.is_banned && !a.is_token_invalid && !a.is_logged_out && !a.is_relay()
                    })
                    .map(|a| {
                        let updated = a
                            .cached_quota
                            .as_ref()
                            .map(|q| q.updated_at)
                            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
                        (a.id.clone(), a.name.clone(), updated)
                    })
                    .collect();
                // 最旧的排前面
                accounts.sort_by_key(|(_, _, t)| *t);
                accounts
                    .into_iter()
                    .take(batch_size as usize)
                    .map(|(id, name, _)| (id, name))
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
                            match crate::oauth::refresh_access_token(rt_val).await {
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
                                    updated_at: chrono::Utc::now(),
                                });
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

                // 每个号之间间隔 interval_minutes 分钟
                tokio::time::sleep(tokio::time::Duration::from_secs(
                    u64::from(interval_minutes) * 60,
                ))
                .await;
            }

            // 如果没有目标，等一轮
            if targets.is_empty() {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            }
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
            println!("[SmartSwitch] Relay 类型，跳过 quota 检查直接切换: {}", target_name);
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
                    "RELAY_ACCOUNT:中转站账号不支持 OpenAI usage 查询，请用「中转站余额刷新」".to_string()
                );
            }
        }
    }
    let (access_token, account_id, refresh_token) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        let account = store.accounts.get(&id).ok_or("账号不存在")?;
        let at = AccountStore::extract_access_token(&account.auth_json);
        let aid = AccountStore::extract_account_id(&account.auth_json);
        let rt = account.refresh_token.clone();
        (at, aid, rt)
    };

    // 如果没有 access_token，先用 refresh_token 换一个
    let access_token = if let Some(at) = access_token {
        at
    } else if let Some(ref rt) = refresh_token {
        match crate::oauth::refresh_access_token(rt).await {
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

    let result =
        UsageFetcher::fetch_usage_direct(access_token, account_id, refresh_token, true).await;

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
        updated_at: Utc::now(),
    }
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
                    "RELAY_ACCOUNT:中转站账号请用「中转站余额刷新」，不是 OpenAI usage".to_string()
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

    // 1. 从 Store 获取该账号的 Token
    let (access_token_opt, account_id, refresh_token) = {
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

        (at, aid, rt)
    };

    // 如果没有 access_token，先用 refresh_token 换一个
    let access_token = if let Some(at) = access_token_opt {
        at
    } else if let Some(ref rt) = refresh_token {
        match crate::oauth::refresh_access_token(rt).await {
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

    // 2. 使用 Token 获取用量（允许自动刷新）
    let result = UsageFetcher::fetch_usage_direct(
        access_token,
        account_id,
        refresh_token,
        true, // 允许 refresh，解决 token 过期问题
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
                updated_at: Utc::now(),
            });
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
                updated_at: Utc::now(),
            });
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
    if data.repos.iter().any(|r| r.owner == owner && r.name == name) {
        return Err("仓库已存在".into());
    }
    data.repos.push(skills::SkillRepo { owner, name, branch, enabled: true });
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
    let installed_dirs: std::collections::HashSet<String> = data.skills.iter().map(|s| s.directory.clone()).collect();
    for s in &mut discovered {
        s.installed = installed_dirs.contains(&s.directory);
    }
    Ok(discovered)
}

#[tauri::command]
async fn install_skill(skill_json: String) -> Result<(), String> {
    let skill: skills::DiscoverableSkill = serde_json::from_str(&skill_json).map_err(|e| e.to_string())?;
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
    let ssot = dirs::home_dir().unwrap().join(".codex-switcher").join("skills").join(&directory);
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
        std::fs::read_to_string(&config_path).map_err(|e| format!("读取 config.toml 失败: {}", e))?
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
        if in_features && trimmed.starts_with("goals") && trimmed.contains('=') && !trimmed.starts_with('[') {
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
    if store.settings.remote_server_url.is_empty() && store.settings.remote_server_url_fallback.is_empty() {
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
async fn client_settings_snapshot(
    state: &State<'_, AppState>,
) -> Result<(String, String), String> {
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
        let auth_opt = {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            store.accounts.get(cid).map(|a| a.auth_json.clone())
        };
        if let Some(auth) = auth_opt {
            if let Err(e) = account::AccountStore::write_codex_auth(&auth) {
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
async fn remote_delete_account_cmd(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
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
            use std::os::unix::io::IntoRawFd;
            let fd = file.into_raw_fd();
            unsafe {
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
                libc::close(fd);
            }
            eprintln!(
                "\n=== codex-switcher started {} pid={} ===",
                chrono::Utc::now().to_rfc3339(),
                std::process::id()
            );
        }
    }

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
            set_account_inactive_refresh_enabled,
            export_accounts,
            import_accounts,
            add_relay_account,
            update_relay_model_map,
            refresh_relay_usage,
            bulk_import_accounts,
            check_codex_login,
            get_quota_by_id,
            oauth_server::start_oauth_login,
            oauth_server::submit_oauth_callback,
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
            get_session_bindings,
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
            get_compression_stats,
            reset_compression_stats,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

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
            relay_usage_cache: None,
            relay_model_map: None,
            relay_model_fallback: None,
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
