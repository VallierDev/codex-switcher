import { useState, useEffect, useMemo, useRef } from 'react';
import { Zap, RefreshCw, ArrowLeftRight, Trash2, Clock, UploadCloud, Plus, Gauge, UserPlus } from 'lucide-react';
import { Account, AppSettings, RelayUsageCache, SparkWindows, effectiveKind } from '../hooks/useAccounts';
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

/** 把上游英文邀请提示翻成人话 */
function friendlyInviteMessage(raw: string | null | undefined): string {
    const m = (raw || '').trim();
    if (!m) return '邀请未成功';
    if (/already has a referral sent/i.test(m)) return '已经给该邮箱发过邀请了（同一邮箱不能重复邀请）';
    if (/cannot be referred/i.test(m)) return '该邮箱无法被邀请（通常是已注册过 ChatGPT 的老用户，推荐奖励只对新邮箱有效）';
    if (/not available for your plan/i.test(m)) return '当前套餐不支持发邀请（free 号没有邀请权限）';
    return m;
}

/** 解出 accept-referral 链接里的 referral_context（base64 JSON），拿到奖励类型/被邀邮箱 */
function decodeReferralBenefit(url: string): string | null {
    try {
        const ctx = new URL(url).searchParams.get('referral_context');
        if (!ctx) return null;
        const json = JSON.parse(atob(ctx.replace(/-/g, '+').replace(/_/g, '/')));
        if (/rate_limit_reset/i.test(json.referral_type || '')) return '🔄 主动重置次数 +1';
        return json.invite_page_benefit_text || json.referral_type || null;
    } catch {
        return null;
    }
}

interface InviteLink {
    email: string;
    referral_id: string;
    invite_url: string;
}
interface InviteResult {
    ok: boolean;
    status_code: number;
    emails: string[];
    invites: InviteLink[];
    failed_emails: string[];
    message: string | null;
    upstream_raw: string;
}

interface ResetCreditResult {
    ok: boolean;
    status_code: number;
    code: string;
    windows_reset: number;
    message: string;
    upstream_raw: string;
}

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
    reset_credits?: number | null;
    spark?: SparkWindows | null;
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
    // Codex 邀请弹窗
    const [inviteModal, setInviteModal] = useState<{ id: string; name: string } | null>(null);
    const [inviteEmails, setInviteEmails] = useState('');
    const [inviteSending, setInviteSending] = useState(false);
    const [inviteResult, setInviteResult] = useState<InviteResult | null>(null);
    const [inviteError, setInviteError] = useState<string | null>(null);
    // Codex 启动：用该账号在隔离 CODEX_HOME 直连下开一个真 codex 终端
    const [launchingIds, setLaunchingIds] = useState<Set<string>>(new Set());
    // 主动重置：消耗一次 reset_credit 立即重置限额窗口
    const [resetModal, setResetModal] = useState<{ id: string; name: string; credits: number } | null>(null);
    const [resetting, setResetting] = useState(false);

    const autoReload = settings.auto_reload_ide;
    const setAutoReload = (val: boolean) => onUpdateSettings({ ...settings, auto_reload_ide: val });

    const handleCopy = (id: string, text: string) => {
        navigator.clipboard.writeText(text).then(() => {
            setCopiedId(id);
            setTimeout(() => setCopiedId(null), 2000);
        });
    };

    const handleLaunchCodex = async (id: string, name: string) => {
        if (launchingIds.has(id)) return;
        setLaunchingIds(prev => new Set(prev).add(id));
        try {
            const msg = await invoke<string>('open_codex_terminal', { id });
            setPushToast({ type: 'success', text: msg || `${name} 已打开 codex 终端` });
        } catch (e) {
            setPushToast({ type: 'error', text: `${name} 启动失败：${String(e)}` });
        } finally {
            setLaunchingIds(prev => { const n = new Set(prev); n.delete(id); return n; });
            setTimeout(() => setPushToast(null), 4000);
        }
    };

    const handleConsumeReset = async () => {
        if (!resetModal || resetting) return;
        const { id, name } = resetModal;
        setResetting(true);
        try {
            const res = await invoke<ResetCreditResult>('consume_reset_credit', { id });
            setResetModal(null);
            setPushToast({
                type: res.ok ? 'success' : 'error',
                text: `${name}：${res.message}`,
            });
            // 重置成功(或 nothing_to_reset)后重拉一次 quota，刷新限额条 + 剩余次数
            if (res.ok || res.code === 'nothing_to_reset') {
                await handleRefreshOne(id);
            }
        } catch (e) {
            setResetModal(null);
            setPushToast({ type: 'error', text: `${name} 重置失败：${humanizeRefreshError(String(e))}` });
        } finally {
            setResetting(false);
            setTimeout(() => setPushToast(null), 5000);
        }
    };

    const openInvite = (id: string, name: string) => {
        setInviteModal({ id, name });
        setInviteEmails('');
        setInviteResult(null);
        setInviteError(null);
    };

    const handleSendInvite = async () => {
        if (!inviteModal) return;
        const emails = inviteEmails
            .split(/[\s,;]+/)
            .map(e => e.trim())
            .filter(Boolean);
        if (emails.length === 0) {
            setInviteError('请至少填写 1 个邀请邮箱');
            return;
        }
        setInviteSending(true);
        setInviteError(null);
        setInviteResult(null);
        try {
            const res = await invoke<InviteResult>('send_codex_invite', { id: inviteModal.id, emails });
            setInviteResult(res);
            if (!res.ok) {
                // 优先用上游结构化 message（如重复邀请 "already has a referral sent"、
                // free 号 "Referral invites are not available for your plan"）
                let msg = res.message || `上游返回 HTTP ${res.status_code}`;
                if (!res.message) {
                    try {
                        const j = JSON.parse(res.upstream_raw);
                        const detail = j?.detail ?? j?.error?.message ?? j?.error;
                        if (detail) msg = typeof detail === 'string' ? detail : JSON.stringify(detail);
                    } catch { /* upstream_raw 非 JSON，保留默认文案 */ }
                }
                setInviteError(msg);
            }
        } catch (e) {
            setInviteError(String(e));
        } finally {
            setInviteSending(false);
        }
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
                    reset_credits: acc.cached_quota.reset_credits,
                    spark: acc.cached_quota.spark,
                };
                if (!isValid) initialInvalids.add(acc.id);
            }
        });
        setUsageMap(prev => ({ ...prev, ...initialUsage }));
        setRelayUsageMap(prev => ({ ...prev, ...initialRelayUsage }));
        setInvalidIds(initialInvalids);
        setBannedIds(initialBanned);
    }, [accounts]);

    // 自动 reset 后重拉：cached 数据老于 reset_at 时窗口已经重置但缓存还是旧的 0%，
    // 触发一次 refresh。
    // - 冷却 90s：足够让上一轮 invoke 完成且 accounts prop 拿到新 cached_quota；
    //   失败的话 90s 后自动重试，最多 90s/次的开销可以接受
    // - 跳过 refreshingIds 里在飞的，避免叠加
    // - is_token_invalid/banned/logged_out 由 backend 持久化，前端尊重
    const handleRefreshOneRef = useRef<(id: string) => Promise<void>>(async () => {});
    const autoRefreshTsRef = useRef<Map<string, number>>(new Map());
    const refreshingIdsRef = useRef<Set<string>>(new Set());
    refreshingIdsRef.current = refreshingIds;
    useEffect(() => {
        const COOLDOWN_MS = 90 * 1000;
        const AUTO_CONCURRENCY = 4;

        const scan = () => {
            const nowMs = Date.now();
            const stale: string[] = [];
            const reasons: Record<string, string> = {};
            for (const acc of accounts) {
                if (effectiveKind(acc) !== 'chatgpt_oauth') continue;
                if (acc.is_banned || acc.is_token_invalid || acc.is_logged_out) continue;
                const cq = acc.cached_quota;
                if (!cq) continue;
                const updatedAtMs = cq.updated_at ? new Date(cq.updated_at).getTime() : 0;
                const fiveResetMs = (cq.five_hour_reset_at ?? 0) * 1000;
                const weeklyResetMs = (cq.weekly_reset_at ?? 0) * 1000;
                const needs5h = fiveResetMs > 0 && fiveResetMs <= nowMs && updatedAtMs < fiveResetMs;
                const needsWk = weeklyResetMs > 0 && weeklyResetMs <= nowMs && updatedAtMs < weeklyResetMs;
                if (!needs5h && !needsWk) continue;
                if (refreshingIdsRef.current.has(acc.id)) continue;
                const last = autoRefreshTsRef.current.get(acc.id) ?? 0;
                if (nowMs - last < COOLDOWN_MS) continue;
                autoRefreshTsRef.current.set(acc.id, nowMs);
                stale.push(acc.id);
                reasons[acc.id] = needs5h ? '5H' : 'weekly';
            }
            if (stale.length === 0) return;
            console.log(`[AutoRefresh] 触发 ${stale.length} 个账号 reset 后自动刷新:`,
                stale.map(id => `${accounts.find(a => a.id === id)?.name}(${reasons[id]})`).join(', '));
            let cursor = 0;
            const worker = async () => {
                while (cursor < stale.length) {
                    const i = cursor++;
                    await handleRefreshOneRef.current(stale[i]).catch((e) => {
                        console.warn(`[AutoRefresh] ${stale[i]} 刷新失败:`, e);
                    });
                }
            };
            for (let i = 0; i < Math.min(AUTO_CONCURRENCY, stale.length); i++) worker();
        };

        scan();
        const t = setInterval(scan, 30_000);
        return () => clearInterval(t);
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

    // 把 Tauri/后端原始报错翻译成人能看懂的一句话。
    const humanizeRefreshError = (raw: string): string => {
        const s = raw.toLowerCase();
        if (s.includes('account_banned')) return '账号已被封禁';
        if (s.includes('token_invalid')) return 'Token 已失效，需要重新登录';
        if (s.includes('account_logged_out')) return '账号已登出，需要重新登录';
        if (s.includes('timeout') || s.includes('timed out')) return '请求超时（OpenAI 端慢/被节流）';
        if (s.includes('网络请求失败') || s.includes('network')) return '网络请求失败，检查代理/网络';
        if (s.includes('刷新令牌') || s.includes('refresh')) return 'refresh_token 刷新失败';
        if (s.includes('relay_account')) return '中转账号请用「中转余额刷新」';
        if (raw.length > 160) return raw.slice(0, 160) + '…';
        return raw;
    };

    // 交互处理
    const handleRefreshOne = async (id: string) => {
        setRefreshingIds(prev => new Set(prev).add(id));
        const acc = accounts.find(a => a.id === id);
        const accName = acc?.name ?? id;
        try {
            // Relay 账号走专属 fetcher（不查 OpenAI usage）
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
            // 仍然按错误类型标 UI 状态
            if (errMsg.includes('ACCOUNT_BANNED')) {
                setBannedIds(prev => new Set(prev).add(id));
                setInvalidIds(prev => new Set(prev).add(id));
            } else if (errMsg.includes('TOKEN_INVALID')) {
                setInvalidIds(prev => new Set(prev).add(id));
            }
            // 把错误 tip 出来，不再静默失败
            setPushToast({
                type: 'error',
                text: `${accName} 刷新失败：${humanizeRefreshError(errMsg)}`,
            });
            setTimeout(() => setPushToast(null), 6000);
        } finally {
            setRefreshingIds(prev => { const n = new Set(prev); n.delete(id); return n; });
        }
    };

    // 把最新的 handleRefreshOne 挂到 ref，让上面 reset 后自动刷新的 effect
    // 不必把它放进依赖里反复重建。
    handleRefreshOneRef.current = handleRefreshOne;

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
                <button className="btn-refresh" onClick={() => {
                    // 之前是 Promise.all 一把梭 — N 个账号同时打 OpenAI usage，
                    // 一旦边缘节流单个账号要 10s+，整批的尾延迟会跟着慢账号走。
                    // 改成并发上限 6 的滑动窗口：快账号先回，慢账号自然排队，
                    // 既不雷霆万钧也不串行。
                    const CONCURRENCY = 6;
                    const ids = filteredAccounts.map(a => a.id);
                    setIsRefreshingAll(true);
                    let cursor = 0;
                    const worker = async () => {
                        while (cursor < ids.length) {
                            const i = cursor++;
                            await handleRefreshOne(ids[i]);
                        }
                    };
                    const workers = Array.from({ length: Math.min(CONCURRENCY, ids.length) }, worker);
                    Promise.all(workers).finally(() => setIsRefreshingAll(false));
                }}>
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
                                        {acc.is_session_anchor && (
                                            <span
                                                className="badge anchor"
                                                title="手机锚：磁盘 ~/.codex/auth.json 永远跟随此号，Codex.app 手机远程连接绑定此号；切到其他号时 disk 不动、proxy 出口照切"
                                            >📱 手机锚</span>
                                        )}
                                        {isBanned ? <span className="badge banned" title="该账号已被 OpenAI 封禁">封号</span> : isLoggedOut ? <span className="badge logged-out" title="您已登出或登录了其他账号，请重新登录">已登出</span> : isInvalid && <span className="badge expired" title="该账号 Token 已过期或失效">过期</span>}
                                        {usage?.plan_type && <span className="badge plan">{usage.plan_type.toUpperCase()}</span>}
                                        {usage?.reset_credits != null && (
                                            usage.reset_credits > 0 ? (
                                                <span
                                                    className="badge reset-credits clickable"
                                                    title="点击立即用一次「主动重置次数」重置已耗尽的限额窗口"
                                                    onClick={() => setResetModal({ id: acc.id, name: acc.name, credits: usage.reset_credits ?? 0 })}
                                                    style={{ cursor: 'pointer' }}
                                                >🔄 {usage.reset_credits}</span>
                                            ) : (
                                                <span className="badge reset-credits" title="主动重置次数（剩余 0 次，无法重置）">🔄 {usage.reset_credits}</span>
                                            )
                                        )}
                                    </div>
                                </div>
                                <div className="col-quota-merged">
                                    {effectiveKind(acc) === 'relay' ? (
                                        <RelayQuotaItem account={acc} cache={relayUsageMap[acc.id]} />
                                    ) : usage ? (
                                        <div className="quota-grid">
                                            <QuotaItem label={usage.five_hour_label} percentage={usage.five_hour_left} reset={usage.five_hour_reset} resetAt={usage.five_hour_reset_at} />
                                            <QuotaItem label={usage.weekly_label} percentage={usage.weekly_left} reset={usage.weekly_reset} resetAt={usage.weekly_reset_at} />
                                            {usage.spark && (
                                                <>
                                                    <QuotaItem label="Spark 5H" percentage={usage.spark.five_hour_left} reset={usage.spark.five_hour_reset} resetAt={usage.spark.five_hour_reset_at} />
                                                    <QuotaItem label="Spark 周" percentage={usage.spark.weekly_left} reset={usage.spark.weekly_reset} resetAt={usage.spark.weekly_reset_at} />
                                                </>
                                            )}
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
                                    {effectiveKind(acc) !== 'relay' && (
                                        <div className="wakeup-row">
                                            <button
                                                className="wakeup-btn"
                                                onClick={() => handleLaunchCodex(acc.id, acc.name)}
                                                disabled={launchingIds.has(acc.id)}
                                                title='用该账号开一个真 codex 终端（隔离 + 直连），可发一句"你好"触发 referral 兑现'
                                            >
                                                {launchingIds.has(acc.id) ? '启动中…' : '🚀 启动 codex'}
                                            </button>
                                        </div>
                                    )}
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
                                    {effectiveKind(acc) !== 'relay' && (usage?.plan_type ?? '').toLowerCase() !== 'free' && (
                                        <button className="action-btn invite" onClick={() => openInvite(acc.id, acc.name)} title="发送 Codex 邀请"><UserPlus size={14} /></button>
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

            <ConfirmModal
                isOpen={!!resetModal}
                title="主动重置限额"
                message={<p>确定要为账号 <strong>{resetModal?.name}</strong> 立即重置限额窗口吗？<br /><br />将<strong>消耗 1 次</strong>主动重置次数（剩余 {resetModal?.credits} 次），把当前已耗尽的 5H / 周限额窗口立刻清零。此操作不可撤销。</p>}
                confirmText="立即重置"
                isLoading={resetting}
                loadingText="正在重置…"
                onConfirm={handleConsumeReset}
                onCancel={() => { if (!resetting) setResetModal(null); }}
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

            {inviteModal && (
                <div className="modal-overlay" onClick={() => !inviteSending && setInviteModal(null)}>
                    <div className="modal-content" onClick={e => e.stopPropagation()}>
                        <div className="modal-header">
                            <div className="header-top">
                                <h2>发送 Codex 邀请</h2>
                                <button className="close-btn" onClick={() => setInviteModal(null)} disabled={inviteSending}>
                                    ×
                                </button>
                            </div>
                        </div>
                        <div className="modal-body">
                            <p className="modal-tip" style={{ marginBottom: 10 }}>
                                账号 <strong>{inviteModal.name}</strong>。每行一个邮箱（逗号/空格也行），最多 50 个。奖励是<strong>主动重置次数</strong>，仅对<strong>没注册过 ChatGPT 的新邮箱</strong>有效；同一邮箱只能邀一次。
                            </p>
                            <textarea
                                value={inviteEmails}
                                onChange={e => setInviteEmails(e.target.value)}
                                rows={4}
                                placeholder={'a@example.com\nb@example.com'}
                                style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12, width: '100%' }}
                                disabled={inviteSending}
                            />
                            {inviteError && inviteResult?.failed_emails?.length === 0 && (
                                <div className="invite-result-card err" style={{ marginTop: 10 }}>
                                    <span className="invite-result-icon">✗</span>
                                    <span>{friendlyInviteMessage(inviteError)}</span>
                                </div>
                            )}
                            {inviteResult && inviteResult.invites.length > 0 && (
                                <div className="invite-result-list">
                                    {inviteResult.invites.map((inv, i) => {
                                        const benefit = inv.invite_url ? decodeReferralBenefit(inv.invite_url) : null;
                                        return (
                                            <div key={i} className="invite-result-card ok">
                                                <span className="invite-result-icon">✓</span>
                                                <div className="invite-result-main">
                                                    <div className="invite-result-email">{inv.email || '—'}</div>
                                                    {benefit && <div className="invite-result-benefit">{benefit}</div>}
                                                </div>
                                                {inv.invite_url && (
                                                    <button type="button" className="btn btn-ghost btn-sm" onClick={() => handleCopy(`inv-${i}`, inv.invite_url)}>
                                                        {copiedId === `inv-${i}` ? '已复制' : '复制链接'}
                                                    </button>
                                                )}
                                            </div>
                                        );
                                    })}
                                </div>
                            )}
                            {inviteResult && inviteResult.failed_emails.length > 0 && (
                                <div className="invite-result-list">
                                    {inviteResult.failed_emails.map((em, i) => (
                                        <div key={i} className="invite-result-card err">
                                            <span className="invite-result-icon">✗</span>
                                            <div className="invite-result-main">
                                                <div className="invite-result-email">{em}</div>
                                                <div className="invite-result-benefit">{friendlyInviteMessage(inviteResult.message)}</div>
                                            </div>
                                        </div>
                                    ))}
                                </div>
                            )}
                            {inviteResult && inviteResult.ok && inviteResult.invites.length === 0 && inviteResult.failed_emails.length === 0 && (
                                <div className="invite-result-card ok" style={{ marginTop: 10 }}>
                                    <span className="invite-result-icon">✓</span>
                                    <span>已发送，邮件已投递到对方邮箱。</span>
                                </div>
                            )}
                        </div>
                        <div className="modal-footer">
                            <button type="button" className="btn btn-ghost" onClick={() => setInviteModal(null)} disabled={inviteSending}>
                                关闭
                            </button>
                            <button type="button" className="btn btn-primary" onClick={handleSendInvite} disabled={inviteSending}>
                                {inviteSending ? '发送中…' : '发送邀请'}
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}
