import { useState, useEffect, useMemo } from 'react';
import { Zap, RefreshCw, ArrowLeftRight, Trash2, Clock, UploadCloud, Plus, Gauge } from 'lucide-react';
import { Account, AppSettings, RelayUsageCache, effectiveKind } from '../hooks/useAccounts';
import { invoke } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';

const KIND_BADGE: Record<ReturnType<typeof effectiveKind>, { label: string; className: string }> = {
    chatgpt_oauth: { label: '订阅', className: 'badge kind-chatgpt' },
    openai_key: { label: 'API', className: 'badge kind-openai' },
    relay: { label: '中转', className: 'badge kind-relay' },
};

/** Relay 类账号在 row 上展示哪个标签。新字段 `relay_category` 是权威来源，
 * 缺失时回退到通用"中转"。 */
function relayCategoryBadge(account: Account): { label: string; className: string } {
    switch (account.relay_category) {
        case 'coding_plan':
            return { label: 'Plan', className: 'badge kind-codingplan' };
        case 'third_party':
            return { label: '三方', className: 'badge kind-thirdparty' };
        case 'aggregator':
        default:
            return { label: '中转', className: 'badge kind-relay' };
    }
}
import { useShortCountdown } from '../hooks/useCountdown';
import './AccountList.css';
import { ConfirmModal } from './ConfirmModal';

interface UsageData {
    five_hour_left: number;
    five_hour_reset: string;
    five_hour_reset_at?: number;
    five_hour_label: string;
    weekly_left: number;
    weekly_reset: string;
    weekly_reset_at?: number;
    weekly_label: string;
    plan_type: string;
    is_valid_for_cli: boolean;
}

type FilterType = 'all' | 'sub' | 'plus' | 'pro' | 'team' | 'free' | 'relay' | 'coding_plan' | 'third_party';

interface AccountListProps {
    accounts: Account[];
    currentId: string | null;
    settings: AppSettings;
    onSwitch: (id: string) => void | Promise<void>;
    onDelete: (id: string) => void;
    onUpdateSettings: (settings: AppSettings) => void;
    onRefreshComplete?: () => void;
    onAddAccount?: () => void;
    onAddRelay?: () => void;
    onRefreshUsage?: () => void;
    usageLoading?: boolean;
}

export function AccountList({
    accounts,
    currentId,
    settings,
    onSwitch,
    onAddAccount,
    onAddRelay,
    onRefreshUsage,
    usageLoading,
    onDelete,
    onUpdateSettings,
    onRefreshComplete,
}: AccountListProps) {
    const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
    const [refreshingIds, setRefreshingIds] = useState<Set<string>>(new Set());
    const [copiedId, setCopiedId] = useState<string | null>(null);
    const [switchingIds] = useState<Set<string>>(new Set());
    const [usageMap, setUsageMap] = useState<Record<string, UsageData>>({});
    const [isRefreshingAll, setIsRefreshingAll] = useState(false);
    const [searchQuery, setSearchQuery] = useState('');
    const [filter, setFilter] = useState<FilterType>('all');
    const [invalidIds, setInvalidIds] = useState<Set<string>>(new Set());
    const [bannedIds, setBannedIds] = useState<Set<string>>(new Set());
    const [accountToDelete, setAccountToDelete] = useState<{ id: string, name: string } | null>(null);
    const [pushingIds, setPushingIds] = useState<Set<string>>(new Set());
    const [pushToast, setPushToast] = useState<{ type: 'success' | 'error'; text: string } | null>(null);
    // Relay 类型账号的余额缓存（与 ChatGPT usage 独立）
    const [relayUsageMap, setRelayUsageMap] = useState<Record<string, RelayUsageCache>>({});
    const [cookieEditor, setCookieEditor] = useState<{ id: string; name: string; value: string } | null>(null);
    const [savingCookie, setSavingCookie] = useState(false);

    const autoReload = settings.auto_reload_ide;
    const setAutoReload = (val: boolean) => onUpdateSettings({ ...settings, auto_reload_ide: val });

    const handleCopy = (id: string, text: string) => {
        navigator.clipboard.writeText(text).then(() => {
            setCopiedId(id);
            setTimeout(() => setCopiedId(null), 2000);
        });
    };

    // 初始化数据
    useEffect(() => {
        const initialUsage: Record<string, UsageData> = {};
        const initialInvalids = new Set<string>();
        const initialBanned = new Set<string>();
        const initialRelayUsage: Record<string, RelayUsageCache> = {};

        accounts.forEach(acc => {
            if (acc.is_banned) {
                initialBanned.add(acc.id);
                initialInvalids.add(acc.id);
            } else if (acc.is_token_invalid || acc.is_logged_out) {
                initialInvalids.add(acc.id);
            }
            if (acc.relay_usage_cache) {
                initialRelayUsage[acc.id] = acc.relay_usage_cache;
            }
            if (acc.cached_quota) {
                const isValid = acc.cached_quota.is_valid_for_cli !== false;
                initialUsage[acc.id] = {
                    five_hour_left: acc.cached_quota.five_hour_left,
                    five_hour_reset: acc.cached_quota.five_hour_reset,
                    five_hour_reset_at: acc.cached_quota.five_hour_reset_at,
                    five_hour_label: acc.cached_quota.five_hour_label || '5H 限额',
                    weekly_left: acc.cached_quota.weekly_left,
                    weekly_reset: acc.cached_quota.weekly_reset,
                    weekly_reset_at: acc.cached_quota.weekly_reset_at,
                    weekly_label: acc.cached_quota.weekly_label || '周限额',
                    plan_type: acc.cached_quota.plan_type,
                    is_valid_for_cli: isValid,
                };
                if (!isValid) initialInvalids.add(acc.id);
            }
        });
        setUsageMap(prev => ({ ...prev, ...initialUsage }));
        setRelayUsageMap(prev => ({ ...prev, ...initialRelayUsage }));
        setInvalidIds(initialInvalids);
        setBannedIds(initialBanned);
    }, [accounts]);

    // 搜索与过滤逻辑
    const filteredAccounts = useMemo(() => {
        let result = searchQuery
            ? accounts.filter(a => a.name.toLowerCase().includes(searchQuery.toLowerCase()))
            : accounts;

        if (filter !== 'all') {
            result = result.filter(a => {
                // Relay 类账号现在按 relay_category 分流
                const isRelay = effectiveKind(a) === 'relay';
                if (filter === 'relay') return isRelay && (a.relay_category ?? 'aggregator') === 'aggregator';
                if (filter === 'coding_plan') return isRelay && a.relay_category === 'coding_plan';
                if (filter === 'third_party') return isRelay && a.relay_category === 'third_party';
                if (isRelay) return false; // 其它 plan 过滤胶囊只看订阅类
                // Sub = 所有 ChatGPT 订阅号（不含 Relay / OpenAI Key）
                if (filter === 'sub') return effectiveKind(a) === 'chatgpt_oauth';
                const type = usageMap[a.id]?.plan_type?.toLowerCase() || '';
                if (filter === 'pro') return type.includes('pro');
                if (filter === 'plus') return type.includes('plus');
                if (filter === 'team') return type.includes('team');
                if (filter === 'free') return type && !type.includes('pro') && !type.includes('plus') && !type.includes('team');
                return true;
            });
        }
        return result;
    }, [accounts, searchQuery, filter, usageMap]);

    const filterCounts = useMemo(() => {
        const counts = { all: accounts.length, sub: 0, pro: 0, plus: 0, team: 0, free: 0, relay: 0, coding_plan: 0, third_party: 0 };
        accounts.forEach(a => {
            const kind = effectiveKind(a);
            if (kind === 'relay') {
                const cat = a.relay_category ?? 'aggregator';
                if (cat === 'coding_plan') counts.coding_plan++;
                else if (cat === 'third_party') counts.third_party++;
                else counts.relay++;
                return;
            }
            // Sub = ChatGPT 订阅类（所有 plan tier 合在一起）
            if (kind === 'chatgpt_oauth') counts.sub++;
            const type = usageMap[a.id]?.plan_type?.toLowerCase() || '';
            if (type.includes('pro')) counts.pro++;
            else if (type.includes('plus')) counts.plus++;
            else if (type.includes('team')) counts.team++;
            else if (type) counts.free++;
        });
        return counts;
    }, [accounts, usageMap]);

    // 辅助工具函数
    const formatDate = (val?: string | Date | null) => {
        if (!val) return '-';
        const d = typeof val === 'string' ? new Date(val) : val;
        return isNaN(d.getTime()) ? '-' : d.toLocaleDateString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' });
    };

    const parseDuration = (str?: string) => {
        if (!str || str === '未知' || str === 'N/A') return { text: 'N/A', hours: 999 };
        if (str === '即将重置') return { text: '重置中', hours: 0 };
        const matches = { d: str.match(/(\d+)天/), h: str.match(/(\d+)小时/), m: str.match(/(\d+)分钟/) };
        const d = parseInt(matches.d?.[1] || '0'), h = parseInt(matches.h?.[1] || '0'), m = parseInt(matches.m?.[1] || '0');
        const totalH = d * 24 + h + m / 60;
        const compact = d > 0 ? `${d}天 ${h}时` : h > 0 ? `${h}时 ${m}分` : `${m}分`;
        return { text: compact || 'N/A', hours: totalH };
    };

    const getStatusInfo = (account: Account) => {
        const isCurrent = account.id === currentId;
        const err = account.keepalive?.last_error;
        const isPermanent = err?.toLowerCase().match(/reused|invalidated|expired/);

        if (isPermanent) return { text: '过期', warn: true };
        if (isCurrent) return { text: '当前账号', warn: false };
        return { text: err ? '重试中' : '正常', warn: !!err };
    };

    const handlePushToServer = async (id: string, name: string) => {
        setPushingIds(prev => new Set(prev).add(id));
        try {
            const r = await invoke<{ ok: boolean; id: string; upserted: string; quota_refreshed?: boolean }>(
                'remote_push_account',
                { id }
            );
            const actionText =
                r.upserted === 'created' ? '新增'
                : r.upserted === 'merged' ? '合并到同邮箱旧账号'
                : '更新';
            const quotaText = r.quota_refreshed ? '，已刷新额度' : '';
            setPushToast({ type: 'success', text: `${name} 推送 Server 成功（${actionText}${quotaText}）` });
        } catch (e) {
            setPushToast({ type: 'error', text: `${name} 推送失败: ${e}` });
        } finally {
            setPushingIds(prev => { const n = new Set(prev); n.delete(id); return n; });
            setTimeout(() => setPushToast(null), 4000);
        }
    };

    // 交互处理
    const handleRefreshOne = async (id: string) => {
        setRefreshingIds(prev => new Set(prev).add(id));
        try {
            // Relay 账号走专属 fetcher（不查 OpenAI usage）
            const acc = accounts.find(a => a.id === id);
            if (acc && effectiveKind(acc) === 'relay') {
                const cache = await invoke<RelayUsageCache>('refresh_relay_usage', { id });
                setRelayUsageMap(prev => ({ ...prev, [id]: cache }));
                onRefreshComplete?.();
                return;
            }
            const cmd = settings.remote_mode === 'client'
                ? 'remote_refresh_account_quota'
                : 'get_quota_by_id';
            const usage = await invoke<UsageData>(cmd, { id });
            setUsageMap(prev => ({ ...prev, [id]: usage }));
            setInvalidIds(prev => {
                const next = new Set(prev);
                usage.is_valid_for_cli ? next.delete(id) : next.add(id);
                return next;
            });
            onRefreshComplete?.();
        } catch (err) {
            const errMsg = String(err);
            if (errMsg.includes('ACCOUNT_BANNED')) {
                setBannedIds(prev => new Set(prev).add(id));
                setInvalidIds(prev => new Set(prev).add(id));
            } else if (errMsg.includes('TOKEN_INVALID')) {
                setInvalidIds(prev => new Set(prev).add(id));
            }
        } finally {
            setRefreshingIds(prev => { const n = new Set(prev); n.delete(id); return n; });
        }
    };

    const handleSaveUsageCookie = async () => {
        if (!cookieEditor) return;
        setSavingCookie(true);
        try {
            await invoke('update_relay_usage_cookie', {
                id: cookieEditor.id,
                usageCookie: cookieEditor.value.trim() || null,
            });
            setRelayUsageMap(prev => {
                const next = { ...prev };
                delete next[cookieEditor.id];
                return next;
            });
            const id = cookieEditor.id;
            setCookieEditor(null);
            await handleRefreshOne(id);
        } catch (e) {
            setPushToast({ type: 'error', text: `保存 MiMo Cookie 失败: ${e}` });
            setTimeout(() => setPushToast(null), 4000);
        } finally {
            setSavingCookie(false);
        }
    };

    /// Relay 余额展示：
    /// - unit 是 `%` → 进度条 mini-card（GLM 这种百分比模型）
    /// - 其它（USD/CNY 等金额） → 纯文本 mini-card（unity2 等返回金额的）
    const RelayQuotaItem = ({ account, cache }: { account: Account; cache: RelayUsageCache | undefined }) => {
        const isMiMoRelay = [
            account.relay_usage_preset,
            account.relay_base_url,
            account.relay_homepage,
            account.name,
        ].some(v => (v ?? '').toLowerCase().includes('mimo') || (v ?? '').toLowerCase().includes('xiaomimimo'));
        const canEditCookie = isMiMoRelay;
        const openCookieEditor = () => {
            if (!canEditCookie) return;
            setCookieEditor({
                id: account.id,
                name: account.name,
                value: account.relay_usage_cookie ?? '',
            });
        };
        const editableProps = canEditCookie
            ? {
                role: 'button',
                tabIndex: 0,
                title: '点击修改 MiMo 配额 Cookie',
                onClick: openCookieEditor,
                onKeyDown: (e: React.KeyboardEvent<HTMLDivElement>) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        openCookieEditor();
                    }
                },
            }
            : {};
        if (!cache) {
            return (
                <div className="quota-grid" {...editableProps}>
                    <QuotaItem label="Token 配额" percentage={undefined} reset={undefined} />
                </div>
            );
        }
        const unit = cache.unit ?? '';
        const isPercent = unit === '%' || unit.includes('%');
        if (isPercent) {
            return (
                <div className="quota-grid" {...editableProps}>
                    <QuotaItem
                        label="Token 配额"
                        percentage={cache.remaining}
                        reset={cache.next_reset_at ? '' : undefined}
                        resetAt={cache.next_reset_at ?? undefined}
                    />
                </div>
            );
        }
        // 金额型：mini-card 风格但中间是数字+单位
        const tone = cache.is_active ? 'green' : 'red';
        return (
            <div className="quota-grid" {...editableProps}>
                <div className="quota-mini-card">
                    <div className={`quota-mini-bg ${tone}`} style={{ width: '100%' }} />
                    <div className="quota-mini-content">
                        <span className="quota-label">余额</span>
                        <span className={`quota-percent ${tone}`}>
                            {cache.remaining.toFixed(2)} {unit}
                        </span>
                    </div>
                </div>
            </div>
        );
    };

    const QuotaItem = ({ label, percentage, reset, resetAt }: { label: string, percentage: number | undefined, reset: string | undefined, resetAt?: number }) => {
        const countdown = useShortCountdown(resetAt);
        if (percentage === undefined) return (
            <div className="quota-mini-card empty">
                <span className="quota-label">{label}</span>
                <span className="quota-empty">-</span>
            </div>
        );
        const { text, hours } = parseDuration(reset);
        const displayTime = countdown || text;
        const color = percentage > 50 ? 'green' : percentage > 20 ? 'orange' : 'red';
        const timeColor = hours < 1 ? 'success' : hours < 6 ? 'warning' : 'neutral';

        return (
            <div className="quota-mini-card">
                <div className={`quota-mini-bg ${color}`} style={{ width: `${percentage}%` }} />
                <div className="quota-mini-content">
                    <span className="quota-label">{label}</span>
                    <div className={`quota-time ${timeColor}`}>
                        <Clock className="icon-tiny" />
                        <span>{displayTime}</span>
                    </div>
                    <span className={`quota-percent ${color}`}>{Math.round(percentage)}%</span>
                </div>
            </div>
        );
    };

    return (
        <div className="account-list-container">
            <div className="account-list-toolbar">
                <div className="search-box">
                    <span className="search-icon">🔍</span>
                    <input type="text" placeholder="搜索邮箱..." value={searchQuery} onChange={e => setSearchQuery(e.target.value)} />
                </div>
                <div className="filter-group">
                    {(['all', 'sub', 'pro', 'plus', 'team', 'free', 'relay', 'coding_plan', 'third_party'] as const).map(t => {
                        const isRelayLike = t === 'relay' || t === 'coding_plan' || t === 'third_party';
                        const isSubGroup = t === 'sub';
                        const label = t === 'all' ? 'ALL'
                            : t === 'sub' ? 'Sub'
                            : t === 'coding_plan' ? 'Plan'
                            : t === 'third_party' ? '三方'
                            : t === 'relay' ? '中转'
                            : t.toUpperCase();
                        return (
                            <button
                                key={t}
                                className={`filter-btn filter-btn-compact ${isRelayLike ? 'filter-btn--relay' : ''} ${isSubGroup ? 'filter-btn--sub' : ''} ${filter === t ? 'active' : ''}`}
                                onClick={() => setFilter(t)}
                            >
                                {label}<span className="filter-count">{filterCounts[t]}</span>
                            </button>
                        );
                    })}
                </div>
                <div className="toolbar-spacer" />
                <button
                    className={`toolbar-icon-btn ${autoReload ? 'active-reload' : ''}`}
                    onClick={() => setAutoReload(!autoReload)}
                    title={autoReload ? '关闭自动重载 IDE' : '开启自动重载 IDE'}
                >
                    <Zap size={16} fill={autoReload ? "currentColor" : "none"} />
                </button>
                {onAddAccount && (
                    <button
                        className="toolbar-icon-btn toolbar-icon-btn-primary"
                        onClick={onAddAccount}
                        title="登录账号 (OpenAI / OTP / 导入)"
                    >
                        <Plus size={16} />
                    </button>
                )}
                {onAddRelay && (
                    <button
                        className="toolbar-icon-btn toolbar-icon-btn-relay"
                        onClick={onAddRelay}
                        title="添加中转 (Coding Plan / 通用 Responses 中转)"
                    >
                        <Plus size={16} />
                    </button>
                )}
                {onRefreshUsage && (
                    <button
                        className="toolbar-icon-btn toolbar-icon-btn-accent"
                        onClick={onRefreshUsage}
                        disabled={usageLoading}
                        title="刷新所有账号配额"
                    >
                        <Gauge className={usageLoading ? 'spinning' : ''} size={16} />
                    </button>
                )}
                <button className="btn-refresh" onClick={() => { setIsRefreshingAll(true); Promise.all(filteredAccounts.map(a => handleRefreshOne(a.id))).finally(() => setIsRefreshingAll(false)); }}>
                    <RefreshCw className={isRefreshingAll ? 'spinning' : ''} size={16} />
                </button>
            </div>

            <div className="account-table-scroll">
                <div className="account-table-header">
                    <div className="col-checkbox">
                        <input type="checkbox" className="custom-checkbox" checked={filteredAccounts.length > 0 && filteredAccounts.every(a => selectedIds.has(a.id))} onChange={() => { const s = new Set(selectedIds); filteredAccounts.every(a => s.has(a.id)) ? filteredAccounts.forEach(a => s.delete(a.id)) : filteredAccounts.forEach(a => s.add(a.id)); setSelectedIds(s); }} />
                    </div>
                    <div className="col-drag"></div>
                    <div className="col-email">账号信息</div>
                    <div className="col-quota-merged">配额状态</div>
                    <div className="col-time">同步/保活</div>
                    <div className="col-actions">操作</div>
                </div>

                <div className="account-table-body">
                    {filteredAccounts.map(acc => {
                        const usage = usageMap[acc.id];
                        const status = getStatusInfo(acc);
                        const err = acc.keepalive?.last_error;
                        const isPermanentError = err?.toLowerCase().match(/reused|invalidated|expired/);
                        const isInvalid = invalidIds.has(acc.id) || !!isPermanentError || acc.is_token_invalid || acc.is_logged_out;
                        const isBanned = bannedIds.has(acc.id);
                        const isLoggedOut = acc.is_logged_out;
                        const isCurrent = acc.id === currentId;
                        const isRefreshing = refreshingIds.has(acc.id);

                        return (
                            <div key={acc.id} className={`account-row ${isCurrent ? 'current' : ''} ${selectedIds.has(acc.id) ? 'selected' : ''} ${isBanned ? 'banned' : isLoggedOut ? 'logged-out' : isInvalid ? 'expired' : ''}`}>
                                <div className="col-checkbox">
                                    <input type="checkbox" className="custom-checkbox" checked={selectedIds.has(acc.id)} onChange={() => { const s = new Set(selectedIds); s.has(acc.id) ? s.delete(acc.id) : s.add(acc.id); setSelectedIds(s); }} />
                                </div>
                                <div className="col-drag"><span className="drag-handle">⋮⋮</span></div>
                                <div className="col-email" title="点击复制账号">
                                    {(() => {
                                        const isRelay = effectiveKind(acc) === 'relay';
                                        const isMiMoRelay = [
                                            acc.relay_usage_preset,
                                            acc.relay_base_url,
                                            acc.relay_homepage,
                                            acc.name,
                                        ].some(v => (v ?? '').toLowerCase().includes('mimo') || (v ?? '').toLowerCase().includes('xiaomimimo'));
                                        const link = isRelay
                                            ? (isMiMoRelay
                                                ? 'https://platform.xiaomimimo.com/console/plan-manage'
                                                : (acc.relay_homepage || acc.relay_base_url || ''))
                                            : '';
                                        const onNameClick = (e: React.MouseEvent) => {
                                            // Relay：点击账号名打开主页/base_url；其它：复制
                                            if (isRelay && link) {
                                                e.stopPropagation();
                                                openUrl(link).catch((err) => {
                                                    console.error('openUrl failed:', err);
                                                });
                                            } else {
                                                handleCopy(acc.id, acc.name);
                                            }
                                        };
                                        return (
                                            <span
                                                className={isRelay ? 'email-text relay-name-link' : 'email-text'}
                                                onClick={onNameClick}
                                                title={isRelay && link ? `点击打开 ${link}` : undefined}
                                            >
                                                {acc.name}
                                            </span>
                                        );
                                    })()}
                                    <div className="badges" style={{ display: 'flex', gap: '4px', marginLeft: '8px', flexWrap: 'wrap' }}>
                                        {(() => {
                                            const k = effectiveKind(acc);
                                            const meta = k === 'relay' ? relayCategoryBadge(acc) : KIND_BADGE[k];
                                            return <span className={meta.className}>{meta.label}</span>;
                                        })()}
                                        {copiedId === acc.id && <span className="badge copy-success">已复制</span>}
                                        {isCurrent && <span className="badge current">当前</span>}
                                        {isBanned ? <span className="badge banned" title="该账号已被 OpenAI 封禁">封号</span> : isLoggedOut ? <span className="badge logged-out" title="您已登出或登录了其他账号，请重新登录">已登出</span> : isInvalid && <span className="badge expired" title="该账号 Token 已过期或失效">过期</span>}
                                        {usage?.plan_type && <span className="badge plan">{usage.plan_type.toUpperCase()}</span>}
                                    </div>
                                </div>
                                <div className="col-quota-merged">
                                    {effectiveKind(acc) === 'relay' ? (
                                        <RelayQuotaItem account={acc} cache={relayUsageMap[acc.id]} />
                                    ) : usage ? (
                                        <div className="quota-grid">
                                            <QuotaItem label={usage.five_hour_label} percentage={usage.five_hour_left} reset={usage.five_hour_reset} resetAt={usage.five_hour_reset_at} />
                                            <QuotaItem label={usage.weekly_label} percentage={usage.weekly_left} reset={usage.weekly_reset} resetAt={usage.weekly_reset_at} />
                                        </div>
                                    ) : <span className="quota-empty">未获取数据</span>}
                                </div>
                                <div className="col-time">
                                    <div className="time-item">
                                        <span className="time-label">保活:</span>
                                        <span className={`time-val ${status.warn ? 'warn' : ''}`}>{status.text}</span>
                                    </div>
                                    <div className="time-item refresh">
                                        <span className="time-label">刷新:</span>
                                        <span className="time-val">{formatDate(acc.cached_quota?.updated_at)}</span>
                                    </div>
                                </div>
                                <div className="col-actions">
                                    <button className="action-btn refresh" onClick={() => handleRefreshOne(acc.id)} disabled={isRefreshing} title="刷新"><RefreshCw size={14} className={isRefreshing ? 'spinning' : ''} /></button>
                                    {settings.remote_mode === 'client' && (
                                        <button
                                            className="action-btn push"
                                            onClick={() => handlePushToServer(acc.id, acc.name)}
                                            disabled={pushingIds.has(acc.id)}
                                            title="推送到 Server"
                                        >
                                            <UploadCloud size={14} className={pushingIds.has(acc.id) ? 'spinning' : ''} />
                                        </button>
                                    )}
                                    {!isCurrent && (
                                        <button className="action-btn switch" onClick={() => onSwitch(acc.id)} disabled={switchingIds.has(acc.id)} title="切换"><ArrowLeftRight size={14} /></button>
                                    )}
                                    <button className="action-btn delete" onClick={() => setAccountToDelete({ id: acc.id, name: acc.name })} title="删除"><Trash2 size={14} /></button>
                                </div>
                            </div>
                        );
                    })}
                </div>
            </div>

            <div className="account-list-footer">
                <span>共 {filteredAccounts.length} 个账号</span>
                {selectedIds.size > 0 && <span className="selected-info">已选 {selectedIds.size} 个</span>}
                {pushToast && (
                    <span className={`push-toast ${pushToast.type}`} style={{ marginLeft: 'auto' }}>
                        {pushToast.text}
                    </span>
                )}
            </div>

            <ConfirmModal
                isOpen={!!accountToDelete}
                title="确认删除账号"
                message={<p>确定要永久删除账号 <strong>{accountToDelete?.name}</strong> 吗？<br /><br />此操作不可恢复，删除后有关该账号的本地授权信息将被清除。</p>}
                confirmText="彻底删除"
                onConfirm={() => {
                    if (accountToDelete) {
                        onDelete(accountToDelete.id);
                        setAccountToDelete(null);
                    }
                }}
                onCancel={() => setAccountToDelete(null)}
            />
            {cookieEditor && (
                <div className="modal-overlay" onClick={() => !savingCookie && setCookieEditor(null)}>
                    <div className="modal-content" onClick={e => e.stopPropagation()}>
                        <div className="modal-header">
                            <div className="header-top">
                                <h2>修改 MiMo 配额 Cookie</h2>
                                <button className="close-btn" onClick={() => setCookieEditor(null)} disabled={savingCookie}>
                                    ×
                                </button>
                            </div>
                        </div>
                        <div className="modal-body">
                            <p className="modal-tip" style={{ marginBottom: 12 }}>
                                账号：{cookieEditor.name}。登录 <code>platform.xiaomimimo.com</code> 后，从 Network 请求里复制 <code>Cookie:</code> header。
                            </p>
                            <textarea
                                value={cookieEditor.value}
                                onChange={e => setCookieEditor(prev => prev ? { ...prev, value: e.target.value } : prev)}
                                rows={5}
                                placeholder="Cookie: api-platform_serviceToken=...; userId=...; api-platform_ph=..."
                                style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12, width: '100%' }}
                                disabled={savingCookie}
                            />
                        </div>
                        <div className="modal-footer">
                            <button type="button" className="btn btn-ghost" onClick={() => setCookieEditor(null)} disabled={savingCookie}>
                                取消
                            </button>
                            <button type="button" className="btn btn-primary" onClick={handleSaveUsageCookie} disabled={savingCookie}>
                                {savingCookie ? '保存中…' : '保存并刷新'}
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}
