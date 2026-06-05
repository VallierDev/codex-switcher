//! 后台调度器 - 账号状态同步
//!
//! 策略：
//! - 当前账号：仅按官方 auth.json 回流同步，不主动 refresh
//! - 非活跃账号：由 Switcher 独占执行保活 refresh，并原子回写账号库
//! - 手机锚账号 (v0.7+)：独立 4 min tick，无论 current 是谁都强保活，且把
//!   刷新出来的 token 落盘到 `~/.codex/auth.json`（用 +24h 撒谎 expires_at 让
//!   Codex.app 永远不会自己 refresh，rt 单写者就是本程序）

use crate::account::AccountStore;
use crate::oauth;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::Emitter;
use tokio::time::Duration;

/// anchor 刷新间隔：4 分钟。
///
/// OpenAI access_token 真实寿命大约 10 min，4 min 留 2.5 倍安全余量。
/// 比这个再短意义不大（rt 旋转有限），更长则不安全。
const ANCHOR_REFRESH_INTERVAL_SECS: u64 = 4 * 60;

#[derive(Debug, Clone)]
struct RefreshTarget {
    id: String,
    name: String,
    refresh_token: String,
}

#[derive(Serialize, Clone)]
struct RefreshFailedPayload {
    account_name: String,
    reason: String,
}

fn is_reused_or_revoked_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("refresh_token_reused")
        || lower.contains("refresh_token_invalidated")
        || lower.contains("refresh_token_expired")
        || lower.contains("deactivated")
        || lower.contains("unauthorized")
        || lower.contains("invalid_grant")
}

fn is_logged_out_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("logged out") || lower.contains("signed in to another account")
}

/// 启动后台状态同步调度器
pub fn start(
    store: Arc<Mutex<AccountStore>>,
    app_handle: tauri::AppHandle,
) -> tauri::async_runtime::JoinHandle<()> {
    // 使用 Tauri 的 async runtime 而不是直接 tokio::spawn
    // 因为在 setup() 中调用时 Tokio runtime 可能尚未完全初始化
    tauri::async_runtime::spawn(async move {
        println!("✅ 后台调度器已启动");

        loop {
            let (enabled, interval_minutes, inactive_refresh_days, remote_mode) = {
                let store = store.lock().unwrap();
                (
                    store.settings.background_refresh,
                    store.settings.refresh_interval_minutes,
                    store.settings.inactive_refresh_days,
                    store.settings.remote_mode.clone(),
                )
            };

            if !enabled {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }

            // client / solo 模式：rt 旋转都交给 Server（solo 是临时模式，最终 rt 仍是
            // Server 唯一 owner）。本机不抢 rt，避免两侧 rotate 冲突让 OpenAI 把 rt 标 reused。
            if matches!(remote_mode.as_str(), "client" | "solo") {
                println!(
                    "[Scheduler] {} 模式：跳过本机保活，rt 由 Server 唯一刷新",
                    remote_mode
                );
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }

            println!("[Scheduler] 开始后台同步检查...");

            let interval_minutes = if interval_minutes == 0 {
                30
            } else {
                interval_minutes
            };

            let mut store_changed = false;
            let mut has_failure_event = false;

            // 1) 同步当前账号（权威源：~/.codex/auth.json）
            if let Ok(official_auth) = AccountStore::read_codex_auth() {
                let mut store = store.lock().unwrap();
                if let Some(current_id) = store.current.clone() {
                    let local_auth = store.accounts.get(&current_id).map(|a| a.auth_json.clone());

                    if let Some(local_auth) = local_auth {
                        if AccountStore::auth_identity_matches(&local_auth, &official_auth) {
                            if local_auth != official_auth {
                                println!(
                                    "[Scheduler] 当前账号 {} 检测到官方 auth.json 变动，按权威源同步。",
                                    current_id
                                );
                                if store.sync_account_from_auth_json(&current_id, official_auth) {
                                    let _ = store.save();
                                    store_changed = true;
                                    println!("[Scheduler] ✅ 当前账号反向同步成功");
                                }
                            } else {
                                println!(
                                    "[Scheduler] 当前账号 {} 与官方 auth.json 一致。",
                                    current_id
                                );
                            }
                        } else {
                            println!(
                                "[Scheduler] 当前账号 {} 与官方 auth.json 身份不匹配，跳过同步。",
                                current_id
                            );
                        }
                    }
                }
            }

            // 2) 收集应由 Switcher 独占保活的非活跃账号
            let targets: Vec<RefreshTarget> = {
                let store = store.lock().unwrap();
                let current = store.current.as_deref();
                store
                    .accounts
                    .values()
                    .filter(|account| current != Some(account.id.as_str()))
                    .filter(|account| {
                        AccountStore::should_refresh_inactive_account(
                            account,
                            inactive_refresh_days,
                        )
                    })
                    .filter_map(|account| {
                        let rt = account
                            .refresh_token
                            .clone()
                            .or_else(|| AccountStore::extract_refresh_token(&account.auth_json))?;
                        Some(RefreshTarget {
                            id: account.id.clone(),
                            name: account.name.clone(),
                            refresh_token: rt,
                        })
                    })
                    .collect()
            };

            // 3) 对非活跃账号执行独占保活刷新
            for target in targets {
                println!("[Scheduler] 非活跃账号 {} 尝试保活刷新", target.name);

                match oauth::refresh_access_token_locked(&target.id, &target.refresh_token).await {
                    Ok(tokens) => {
                        let mut store = store.lock().unwrap();
                        if store.current.as_deref() == Some(target.id.as_str()) {
                            // 账号已变为当前，交给官方路径维护
                            continue;
                        }
                        if let Some(account) = store.accounts.get_mut(&target.id) {
                            if !account.keepalive.inactive_refresh_enabled {
                                continue;
                            }
                            AccountStore::apply_refreshed_tokens(
                                account,
                                tokens.access_token,
                                tokens.refresh_token,
                                tokens.id_token,
                                tokens.expires_in,
                            );
                        }
                        store.mark_keepalive_attempt_success(&target.id);
                        let _ = store.save();
                        store_changed = true;
                        println!("[Scheduler] ✅ 非活跃账号 {} 保活刷新成功", target.name);

                        // 记录后台保活系统日志
                        use tauri::Manager;
                        if let Some(logger) =
                            app_handle.try_state::<Arc<crate::switch_log::SwitchLogger>>()
                        {
                            logger.inner().log_switch(
                                None,
                                target.name.clone(),
                                crate::switch_log::SwitchReason::BackgroundKeepalive,
                                None,
                                None,
                            );
                        }
                    }
                    Err(err) => {
                        let reason = err;
                        let mut store = store.lock().unwrap();
                        store.mark_keepalive_attempt_failed(&target.id, reason.clone());
                        if is_reused_or_revoked_error(&reason) || is_logged_out_error(&reason) {
                            // 风险保护：检测到 reused/revoked 后，自动停用该账号的非活跃保活，避免重复消耗。
                            let _ = store.set_inactive_refresh_enabled(&target.id, false);
                            if let Some(account) = store.accounts.get_mut(&target.id) {
                                if is_logged_out_error(&reason) {
                                    account.is_logged_out = true;
                                } else {
                                    account.is_token_invalid = true;
                                }
                            }
                        }
                        let _ = store.save();
                        has_failure_event = true;
                        println!(
                            "[Scheduler] ❌ 非活跃账号 {} 保活刷新失败: {}",
                            target.name, reason
                        );

                        let _ = app_handle.emit(
                            "token-refresh-failed",
                            RefreshFailedPayload {
                                account_name: target.name,
                                reason,
                            },
                        );
                    }
                }
            }

            if store_changed || has_failure_event {
                let _ = app_handle.emit("accounts-updated", ());
            }

            tokio::time::sleep(Duration::from_secs(u64::from(interval_minutes) * 60)).await;
        }
    })
}

/// 启动手机锚专用刷新循环（v0.7+）。
///
/// **职责**：把 anchor 账号的 access_token / refresh_token 保活，并把刷新结果
/// 原子写盘到 `~/.codex/auth.json`（带 +24h 撒谎 expires_at 字段，让
/// Codex.app 和 codex CLI 都不会自己主动 refresh，rt 旋转的单写者就是这里）。
///
/// **触发条件**：store 里存在 `is_session_anchor = true` 的账号。无 anchor 时此 tick 空转。
///
/// **与 main scheduler 关系**：互不阻塞、互不重复。
/// - main scheduler 用 `should_refresh_inactive_account`（天级粒度），不适合 anchor
/// - anchor 这里强制 4 min 跑一次；如果 anchor == current 且 main scheduler 也想刷，
///   `oauth::refresh_access_token` 是幂等可重入的（每次都拿新 rt），这里抢到先就给它写
/// - remote_mode == "client" | "solo" 时跳过：client 由 `start_fast_auth_sync`
///   从 Server 拉；solo 本机虽然跑 codex 但 rt 仍委托 Server（solo 是临时模式）
pub fn start_anchor_refresh(
    store: Arc<Mutex<AccountStore>>,
    app_handle: tauri::AppHandle,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        println!("✅ 手机锚保活循环已启动（4 min 间隔，无 anchor 时空转）");
        loop {
            tokio::time::sleep(Duration::from_secs(ANCHOR_REFRESH_INTERVAL_SECS)).await;

            // 1) 取 anchor 信息 + 模式
            let (anchor_id, anchor_name, anchor_rt, remote_mode) = {
                let store = match store.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let mode = store.settings.remote_mode.clone();
                match store.session_anchor() {
                    Some(acc) => {
                        let rt = acc
                            .refresh_token
                            .clone()
                            .or_else(|| AccountStore::extract_refresh_token(&acc.auth_json));
                        (Some(acc.id.clone()), acc.name.clone(), rt, mode)
                    }
                    None => (None, String::new(), None, mode),
                }
            };

            let Some(anchor_id) = anchor_id else {
                // 没设 anchor，空转
                continue;
            };

            if matches!(remote_mode.as_str(), "client" | "solo") {
                // client / solo 模式：rt 都委托 Server，本机不抢
                // - client：disk 由 Server 通过 fast_auth_sync 拉
                // - solo：本机自己跑 codex，但 rt 仍走 Server（solo 是临时模式）
                continue;
            }

            let Some(rt) = anchor_rt else {
                eprintln!(
                    "[AnchorRefresh] anchor 账号 {} 缺 refresh_token，无法保活（需要重新登录）",
                    anchor_name
                );
                continue;
            };

            // 2) 刷新 token
            match oauth::refresh_access_token_locked(&anchor_id, &rt).await {
                Ok(tokens) => {
                    // 3a) 写回 store
                    let auth_value = {
                        let mut store = match store.lock() {
                            Ok(g) => g,
                            Err(_) => continue,
                        };
                        let Some(account) = store.accounts.get_mut(&anchor_id) else {
                            continue;
                        };
                        // 校验 anchor 标记还在（用户可能在 tick 期间取消了 anchor）
                        if !account.is_session_anchor {
                            println!(
                                "[AnchorRefresh] {} 已不是 anchor（用户中途取消），跳过写盘",
                                anchor_name
                            );
                            continue;
                        }
                        AccountStore::apply_refreshed_tokens(
                            account,
                            tokens.access_token,
                            tokens.refresh_token,
                            tokens.id_token,
                            tokens.expires_in,
                        );
                        let v = account.to_codex_auth_value();
                        let _ = store.save();
                        v
                    };

                    // 3b) 写盘（extended_expiry 防 Codex.app 自刷）
                    if let Err(e) =
                        AccountStore::write_codex_auth_extended_expiry(&auth_value)
                    {
                        eprintln!("[AnchorRefresh] 写 ~/.codex/auth.json 失败: {}", e);
                    } else {
                        println!(
                            "[AnchorRefresh] ✅ anchor {} 保活成功 + 已落盘",
                            anchor_name
                        );
                    }
                    crate::proxy::invalidate_remote_token_cache();
                }
                Err(reason) => {
                    eprintln!(
                        "[AnchorRefresh] ❌ anchor {} 保活失败: {}",
                        anchor_name, reason
                    );
                    // rt 失效是致命情况：手机 bridge 会跟着断。标记账号 token_invalid，
                    // 让 UI 弹出"重新登录 anchor"提示
                    if is_reused_or_revoked_error(&reason) || is_logged_out_error(&reason)
                    {
                        if let Ok(mut store) = store.lock() {
                            if let Some(account) = store.accounts.get_mut(&anchor_id) {
                                if is_logged_out_error(&reason) {
                                    account.is_logged_out = true;
                                } else {
                                    account.is_token_invalid = true;
                                }
                                let _ = store.save();
                            }
                        }
                        let _ = app_handle.emit(
                            "token-refresh-failed",
                            RefreshFailedPayload {
                                account_name: anchor_name.clone(),
                                reason,
                            },
                        );
                    }
                }
            }
        }
    })
}
