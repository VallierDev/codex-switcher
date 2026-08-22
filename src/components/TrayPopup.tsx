import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
// Rust 端 on_window_event(Focused(false)) 负责隐藏弹窗
import './TrayPopup.css';

interface QuotaInfo {
    five_hour_left: number;
    five_hour_reset_at: number | null;
    weekly_left: number;
    weekly_reset_at: number | null;
    plan_type: string;
}

interface AccountInfo {
    name: string;
    is_banned: boolean;
    is_token_invalid: boolean;
    is_logged_out: boolean;
    cached_quota: QuotaInfo | null;
}

interface ProxyStatus {
    enabled: boolean;
    port: number;
    is_running: boolean;
    total_requests: number;
    auto_switches: number;
}

interface TokenStats {
    total_input_tokens: number;
    total_output_tokens: number;
    total_tokens: number;
    total_cost_usd: number;
    total_requests: number;
    last_month_cost: number | null;
    last_month_tokens: number | null;
}

interface TrayData {
    account: AccountInfo | null;
    proxy: ProxyStatus;
    tokens: TokenStats;
    next_account: { name: string; score: number } | null;
    /** 当前 anchor 账号名（若有），current != anchor 时切号 disk 不动 */
    anchor: { name: string; is_current: boolean } | null;
}

function formatCountdown(resetAt: number | null): string {
    if (!resetAt || resetAt <= 0) return '未知';
    const diff = resetAt - Math.floor(Date.now() / 1000);
    if (diff <= 0) return '已重置';
    const h = Math.floor(diff / 3600);
    const m = Math.floor((diff % 3600) / 60);
    return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

function formatTokens(n: number): string {
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
    if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K';
    return n.toString();
}

function statusClass(pct: number): string {
    if (pct > 50) return 'healthy';
    if (pct > 10) return 'warning';
    return 'critical';
}

function statusLabel(pct: number): string {
    if (pct > 50) return 'HEALTHY';
    if (pct > 10) return 'WARNING';
    return 'CRITICAL';
}

export function TrayPopup() {
    const [data, setData] = useState<TrayData | null>(null);
    const [switching, setSwitching] = useState(false);

    const fetchData = async () => {
        try {
            const [proxy, tokens] = await Promise.all([
                invoke<ProxyStatus>('get_proxy_status'),
                invoke<TokenStats>('get_token_stats'),
            ]);

            // Get current account info from accounts list
            const accounts = await invoke<any[]>('get_accounts');
            const currentId = await invoke<string | null>('get_current_account_id');
            const account = currentId ? accounts.find((a: any) => a.id === currentId) : null;
            const anchorAcc = accounts.find((a: any) => a.is_session_anchor);

            setData({
                account: account ? {
                    name: account.name,
                    is_banned: account.is_banned,
                    is_token_invalid: account.is_token_invalid,
                    is_logged_out: account.is_logged_out,
                    cached_quota: account.cached_quota,
                } : null,
                proxy,
                tokens,
                next_account: null, // Will be populated later
                anchor: anchorAcc
                    ? { name: anchorAcc.name, is_current: anchorAcc.id === currentId }
                    : null,
            });
        } catch (e) {
            console.error('Failed to fetch tray data:', e);
        }
    };

    useEffect(() => {
        document.documentElement.classList.add('is-tray-popup');
        document.body.classList.add('is-tray-popup');
        fetchData();
        const interval = setInterval(fetchData, 5000);
        const unsub = listen('accounts-updated', fetchData);

        // 焦点丢失由 Rust 端 on_window_event 处理

        return () => {
            clearInterval(interval);
            unsub.then(fn => fn());
            document.documentElement.classList.remove('is-tray-popup');
            document.body.classList.remove('is-tray-popup');
        };
    }, []);

    const handleSwitch = async () => {
        setSwitching(true);
        try {
            await invoke('switch_to_next_account_internal_cmd');
        } catch {
            // fallback: try tray's method
        }
        await fetchData();
        setSwitching(false);
    };

    const handleRefresh = async () => {
        try {
            const currentId = await invoke<string | null>('get_current_account_id');
            if (!currentId) return;
            // Relay 账号走 refresh_relay_usage（GLM 等），订阅号走 OpenAI usage 路径。
            const accounts = await invoke<Array<{ id: string; kind?: string }>>('get_accounts');
            const acc = accounts.find(a => a.id === currentId);
            const isRelay = (acc?.kind ?? '').toLowerCase() === 'relay';
            if (isRelay) {
                await invoke('refresh_relay_usage', { id: currentId });
            } else {
                await invoke('get_quota_by_id', { id: currentId });
            }
            await fetchData();
        } catch (e) {
            console.error('Refresh failed:', e);
        }
    };

    const handleOpenDashboard = async () => {
        await invoke('show_main_window_cmd');
        getCurrentWebviewWindow().hide();
    };

    const q = data?.account?.cached_quota;
    const fiveH = q?.five_hour_left ?? 0;
    const weekly = q?.weekly_left ?? 0;

    return (
        <div className="tray-popup">
            {/* Header */}
            <div className="tp-header">
                <div className="tp-title">
                    <div className="tp-logo">⚡</div>
                    <div>
                        <div className="tp-name">Codex Switcher</div>
                        <div className="tp-subtitle">Usage Monitor</div>
                    </div>
                </div>
                {data?.proxy.is_running && (
                    <div className="tp-badge running">● Proxy ON</div>
                )}
            </div>

            {/* Account */}
            {data?.account && (
                <div className="tp-account">
                    {data.account.name}
                    <span className="tp-plan">{q?.plan_type || '-'}</span>
                    {data.account.is_banned && <span className="tp-banned">封号</span>}
                    {data.account.is_logged_out && !data.account.is_banned && <span className="tp-logged-out">需重登</span>}
                    {data.account.is_token_invalid && !data.account.is_banned && !data.account.is_logged_out && <span className="tp-invalid">失效</span>}
                </div>
            )}

            {/* Anchor 状态条：anchor != current 时显示"手机在 X，代理出口在 current"，帮用户秒懂当前 disk 锁在哪号 */}
            {data?.anchor && !data.anchor.is_current && (
                <div className="tp-anchor">
                    <span className="tp-anchor-icon">📱</span>
                    <span className="tp-anchor-text">
                        手机锚 <b>{data.anchor.name}</b> · 代理出口 <b>{data.account?.name}</b>
                    </span>
                </div>
            )}
            {data?.anchor && data.anchor.is_current && (
                <div className="tp-anchor matched">
                    <span className="tp-anchor-icon">📱</span>
                    <span className="tp-anchor-text">手机锚 = 当前号</span>
                </div>
            )}

            {/* Quota Cards */}
            <div className="tp-cards">
                <div className={`tp-card ${statusClass(fiveH)}`}>
                    <div className="tp-card-header">
                        <span className="tp-card-icon">⚡</span>
                        <span>SESSION</span>
                        <span className={`tp-status ${statusClass(fiveH)}`}>{statusLabel(fiveH)}</span>
                    </div>
                    <div className="tp-card-value">
                        {q ? Math.round(fiveH) : '-'}<span className="tp-unit">%</span>
                        <span className="tp-remaining">Remaining</span>
                    </div>
                    <div className="tp-progress">
                        <div className={`tp-progress-bar ${statusClass(fiveH)}`} style={{ width: `${fiveH}%` }} />
                    </div>
                    <div className="tp-reset">
                        Resets in {q ? formatCountdown(q.five_hour_reset_at) : '-'}
                    </div>
                </div>

                <div className={`tp-card ${statusClass(weekly)}`}>
                    <div className="tp-card-header">
                        <span className="tp-card-icon">📅</span>
                        <span>WEEKLY</span>
                        <span className={`tp-status ${statusClass(weekly)}`}>{statusLabel(weekly)}</span>
                    </div>
                    <div className="tp-card-value">
                        {q ? Math.round(weekly) : '-'}<span className="tp-unit">%</span>
                        <span className="tp-remaining">Remaining</span>
                    </div>
                    <div className="tp-progress">
                        <div className={`tp-progress-bar ${statusClass(weekly)}`} style={{ width: `${weekly}%` }} />
                    </div>
                    <div className="tp-reset">
                        Resets in {q ? formatCountdown(q.weekly_reset_at) : '-'}
                    </div>
                </div>
            </div>

            {/* Cost & Token Cards */}
            <div className="tp-cards">
                <div className="tp-card cost">
                    <div className="tp-card-header">
                        <span className="tp-card-icon">💰</span>
                        <span>COST USAGE</span>
                    </div>
                    <div className="tp-card-value cost-value">
                        ${(data?.tokens.total_cost_usd ?? 0).toFixed(2)}
                        <span className="tp-remaining">Spent</span>
                    </div>
                    {data?.tokens.last_month_cost !== null && data?.tokens.last_month_cost !== undefined && (
                        <div className="tp-compare">
                            Vs 上月 ${data.tokens.last_month_cost.toFixed(2)}
                        </div>
                    )}
                </div>

                <div className="tp-card tokens">
                    <div className="tp-card-header">
                        <span className="tp-card-icon">#</span>
                        <span>TOKEN USAGE</span>
                    </div>
                    <div className="tp-card-value token-value">
                        {formatTokens(data?.tokens.total_tokens ?? 0)}
                        <span className="tp-remaining">Tokens</span>
                    </div>
                    <div className="tp-token-detail">
                        In {formatTokens(data?.tokens.total_input_tokens ?? 0)} / Out {formatTokens(data?.tokens.total_output_tokens ?? 0)}
                    </div>
                </div>
            </div>

            {/* Actions */}
            <div className="tp-actions">
                <button className="tp-btn primary" onClick={handleOpenDashboard}>
                    📋 Dashboard
                </button>
                <button className="tp-btn" onClick={handleRefresh}>
                    ↻ Refresh
                </button>
                <button
                    className="tp-btn accent"
                    onClick={handleSwitch}
                    disabled={switching}
                >
                    {switching ? '...' : '→ Switch'}
                </button>
            </div>
        </div>
    );
}
