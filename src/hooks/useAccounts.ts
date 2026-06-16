import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';

export interface CachedQuota {
    five_hour_left: number;
    five_hour_reset: string;
    five_hour_reset_at?: number;
    five_hour_label?: string;
    weekly_left: number;
    weekly_reset: string;
    weekly_reset_at?: number;
    weekly_label?: string;
    plan_type: string;
    is_valid_for_cli?: boolean;
    reset_credits?: number | null;
    updated_at: string;
}

export interface AppSettings {
    auto_reload_ide: boolean;
    primary_ide: string;
    use_pkill_restart: boolean;
    background_refresh: boolean;
    refresh_interval_minutes: number;
    inactive_refresh_days: number;
    theme_palette: string;
    remote_mode?: string;
    relay_auto_switch_out?: boolean;
    relay_auto_switch_in?: boolean;
    client_direct_upstream?: boolean;
}

export interface KeepaliveState {
    inactive_refresh_enabled: boolean;
    last_attempt_at: string | null;
    last_success_at: string | null;
    last_error: string | null;
}

export interface SyncStatus {
    is_synced: boolean;
    disk_email: string | null;
    matching_id: string | null;
    current_id: string | null;
}

export type AccountKind = 'legacy' | 'chatgpt_oauth' | 'openai_key' | 'relay';

export interface RelayUsageCache {
    remaining: number;
    unit: string;
    is_active: boolean;
    next_reset_at?: number | null;
    updated_at: string;
}

export interface Account {
    id: string;
    name: string;
    auth_json: unknown;
    created_at: string;
    last_used: string | null;
    notes: string | null;
    cached_quota: CachedQuota | null;
    keepalive: KeepaliveState;
    is_banned: boolean;
    is_token_invalid: boolean;
    is_logged_out: boolean;
    kind?: AccountKind;
    relay_base_url?: string | null;
    relay_homepage?: string | null;
    relay_usage_preset?: string | null;
    relay_usage_cookie?: string | null;
    relay_usage_cache?: RelayUsageCache | null;
    relay_model_map?: Record<string, string> | null;
    relay_model_fallback?: string | null;
    relay_protocol?: string | null;
    /** 业务分类：aggregator (中转) / coding_plan / third_party (API) */
    relay_category?: 'aggregator' | 'coding_plan' | 'third_party' | null;
    /** 手机锚（Codex.app 手机远程连接绑定）。整个 store 强约束最多一个 true。 */
    is_session_anchor?: boolean;
}

/** 解析有效 kind：与 Rust 端 `Account::effective_kind()` 行为一致
 *  (Legacy 时按 auth_json 里的 access_token 前缀派生) */
export function effectiveKind(account: Account): Exclude<AccountKind, 'legacy'> {
    if (account.kind && account.kind !== 'legacy') return account.kind;
    const auth = account.auth_json as { tokens?: { access_token?: string }; access_token?: string } | null;
    const tok = auth?.tokens?.access_token ?? auth?.access_token;
    if (typeof tok === 'string' && tok.startsWith('eyJ')) return 'chatgpt_oauth';
    return 'openai_key';
}

export function useAccounts() {
    const [accounts, setAccounts] = useState<Account[]>([]);
    const [currentId, setCurrentId] = useState<string | null>(null);
    const [settings, setSettings] = useState<AppSettings>({
        auto_reload_ide: false,
        primary_ide: 'Windsurf',
        use_pkill_restart: false,
        background_refresh: false,
        refresh_interval_minutes: 30,
        inactive_refresh_days: 7,
        theme_palette: 'midnight',
    });
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);

    // 加载账号和设置
    const loadData = useCallback(async () => {
        try {
            setError(null);

            const [accountList, current, appSettings] = await Promise.all([
                invoke<Account[]>('get_accounts'),
                invoke<string | null>('get_current_account_id'),
                invoke<AppSettings>('get_settings'),
            ]);

            setAccounts(accountList);
            setCurrentId(current);
            setSettings(appSettings);
        } catch (err) {
            setError(String(err));
        } finally {
            setLoading(false);
        }
    }, []);

    // 初始加载
    useEffect(() => {
        loadData();
    }, [loadData]);

    const setInactiveRefreshEnabled = useCallback(async (id: string, enabled: boolean) => {
        try {
            setError(null);
            await invoke('set_account_inactive_refresh_enabled', { id, enabled });
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 设置 / 取消 手机锚（互斥：整个 store 最多一个 anchor）
    const setSessionAnchor = useCallback(async (id: string, enabled: boolean) => {
        try {
            setError(null);
            await invoke('set_session_anchor', { id, enabled });
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 更新设置
    const updateSettings = useCallback(async (newSettings: AppSettings) => {
        try {
            setError(null);
            await invoke('update_settings', { settings: newSettings });
            setSettings(newSettings);
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, []);

    // ... 其他方法保持不变，但使用 loadData 替换 loadAccounts ...

    // 导入当前账号
    const importCurrent = useCallback(async (name: string, notes?: string) => {
        try {
            setError(null);
            await invoke('import_current_account', { name, notes });
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 切换账号
    const switchTo = useCallback(async (id: string) => {
        try {
            setError(null);
            await invoke('switch_account', { id });
            setCurrentId(id);
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 删除账号
    // 注意：不在这里做 confirm —— 上层 UI (AccountList ConfirmModal / AccountCard 二次点击)
    // 已经承担确认职责；这里再弹 window.confirm 会变成双弹框。
    const deleteAccount = useCallback(async (id: string) => {
        try {
            setError(null);
            if (currentId === id) {
                setCurrentId(null);
            }
            await invoke('delete_account', { id });
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData, currentId]);

    // 更新账号
    const updateAccount = useCallback(async (id: string, name?: string, notes?: string) => {
        try {
            setError(null);
            await invoke('update_account', { id, name, notes });
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 导出
    const exportAccounts = useCallback(async () => {
        try {
            return await invoke<string>('export_accounts');
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, []);

    // 导入
    const importAccounts = useCallback(async (json: string) => {
        try {
            setError(null);
            await invoke('import_accounts', { json });
            await loadData();
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 检查 Codex 登录状态
    const checkCodexLogin = useCallback(async () => {
        try {
            return await invoke<boolean>('check_codex_login');
        } catch {
            return false;
        }
    }, []);

    // 开始 OAuth 登录；openBrowser=false 时只准备 URL + 启动监听，不打开默认浏览器
    const startOAuthLogin = useCallback(async (openBrowser: boolean = true) => {
        try {
            setError(null);
            return await invoke<string>('start_oauth_login', { openBrowser });
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, []);

    // 完成 OAuth 登录
    const finalizeOAuthLogin = useCallback(async (code: string) => {
        try {
            setError(null);
            const account = await invoke<Account>('finalize_oauth_login', { code });
            await loadData();
            return account;
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, [loadData]);

    // 重载 IDE 窗口
    const reloadIdeWindows = useCallback(async (useWindowReload: boolean = false) => {
        try {
            setError(null);
            return await invoke<string[]>('reload_ide_windows', { useWindowReload });
        } catch (err) {
            setError(String(err));
            throw err;
        }
    }, []);

    return {
        accounts,
        currentId,
        settings,
        loading,
        error,
        refresh: loadData,
        importCurrent,
        switchTo,
        deleteAccount,
        updateAccount,
        exportAccounts,
        importAccounts,
        checkCodexLogin,
        startOAuthLogin,
        finalizeOAuthLogin,
        reloadIdeWindows,
        updateSettings,
        setInactiveRefreshEnabled,
        setSessionAnchor,
        checkSyncConflict: useCallback(async () => {
            return invoke<string | null>('check_sync_conflict');
        }, []),
        getSyncStatus: useCallback(async () => {
            return invoke<SyncStatus>('get_sync_status');
        }, []),
        syncActiveWithDisk: useCallback(async () => {
            await invoke('sync_active_with_disk');
            await loadData();
        }, [loadData]),
    };
}
