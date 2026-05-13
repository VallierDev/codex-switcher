import { useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Account, effectiveKind } from '../hooks/useAccounts';
import {
    CodexSession,
    SessionRoute,
    isLikelyUuid,
    shortSessionId,
    formatRelativeTime,
    basename,
    truncate,
} from '../data/session_routes';
import './AddRelayModal.css';
import './AddRouteModal.css';

interface AddRouteModalProps {
    isOpen: boolean;
    accounts: Account[];
    onClose: () => void;
    onSuccess?: (route: SessionRoute) => void;
}

type SourceMode = 'recent' | 'manual';

function categoryBadge(account: Account): { label: string; className: string } {
    const kind = effectiveKind(account);
    if (kind === 'chatgpt_oauth') return { label: '订阅', className: 'cs-rbadge cs-rbadge--sub' };
    if (kind === 'openai_key') return { label: 'API', className: 'cs-rbadge cs-rbadge--mono' };
    // relay → use relay_category
    switch (account.relay_category) {
        case 'coding_plan':
            return { label: 'Plan', className: 'cs-rbadge cs-rbadge--sub' };
        case 'third_party':
            return { label: '三方', className: 'cs-rbadge cs-rbadge--mono' };
        case 'aggregator':
        default:
            return { label: '中转', className: 'cs-rbadge cs-rbadge--mono' };
    }
}

function accountHealth(account: Account): 'ok' | 'bad' {
    if (account.is_banned || account.is_token_invalid || account.is_logged_out) return 'bad';
    if (account.cached_quota?.is_valid_for_cli === false) return 'bad';
    return 'ok';
}

function accountQuotaSummary(account: Account): string {
    const kind = effectiveKind(account);
    if (kind === 'relay') {
        const c = account.relay_usage_cache;
        if (c && c.is_active) {
            // Show unit as-is; backend uses "USD" / "tokens" / etc.
            return `余额 ${c.remaining.toFixed(2)} ${c.unit}`;
        }
        return '';
    }
    const q = account.cached_quota;
    if (!q) return '';
    // five_hour_left / weekly_left are integer percentages 0..100 in the cached
    // shape we get from useAccounts (legacy behavior — same as Dashboard).
    const fh = Number.isFinite(q.five_hour_left) ? `5h ${q.five_hour_left}%` : '';
    const wk = Number.isFinite(q.weekly_left) ? `周 ${q.weekly_left}%` : '';
    return [fh, wk].filter(Boolean).join(' · ');
}

export function AddRouteModal({ isOpen, accounts, onClose, onSuccess }: AddRouteModalProps) {
    const [sourceMode, setSourceMode] = useState<SourceMode>('recent');

    // Recent sessions
    const [sessions, setSessions] = useState<CodexSession[]>([]);
    const [sessionsLoading, setSessionsLoading] = useState(false);
    const [sessionsError, setSessionsError] = useState<string | null>(null);
    const [sessionSearch, setSessionSearch] = useState('');
    const [cwdFilter, setCwdFilter] = useState<string>('');

    // Selected session — either from list or pasted
    const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
    const [manualSessionId, setManualSessionId] = useState('');

    // 5 分钟内被写过的最新 rollout —— 也就是用户"当前正在跟 codex 聊"的那个会话
    const [activeSession, setActiveSession] = useState<CodexSession | null>(null);

    // Account picker
    const [accountSearch, setAccountSearch] = useState('');
    const [selectedAccountId, setSelectedAccountId] = useState<string | null>(null);

    // Label
    const [label, setLabel] = useState('');

    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    // Reset everything when the modal closes.
    useEffect(() => {
        if (!isOpen) {
            setSourceMode('recent');
            setSessions([]);
            setSessionsError(null);
            setSessionSearch('');
            setCwdFilter('');
            setSelectedSessionId(null);
            setManualSessionId('');
            setActiveSession(null);
            setAccountSearch('');
            setSelectedAccountId(null);
            setLabel('');
            setSubmitting(false);
            setError(null);
        }
    }, [isOpen]);

    // Modal 打开时，检测"当前活跃 codex 会话"（5 分钟内被写过的最新 rollout）
    // 用户点 "🎯 使用当前会话" 一键填入，免去手动复制 session_id 的麻烦。
    useEffect(() => {
        if (!isOpen) return;
        let cancelled = false;
        invoke<CodexSession | null>('detect_active_codex_session', { windowSecs: 300 })
            .then((s) => {
                if (!cancelled) setActiveSession(s);
            })
            .catch(() => {
                /* 没检测到当前活跃会话不是错误，安静地忽略 */
            });
        return () => {
            cancelled = true;
        };
    }, [isOpen]);

    // Load recent sessions when modal opens / mode switches to recent.
    useEffect(() => {
        if (!isOpen || sourceMode !== 'recent') return;
        let cancelled = false;
        setSessionsLoading(true);
        setSessionsError(null);
        invoke<CodexSession[]>('list_codex_sessions', { limit: 50, daysBack: 14 })
            .then((rows) => {
                if (cancelled) return;
                setSessions(rows);
            })
            .catch((e) => {
                if (cancelled) return;
                setSessionsError(typeof e === 'string' ? e : String(e));
            })
            .finally(() => {
                if (!cancelled) setSessionsLoading(false);
            });
        return () => {
            cancelled = true;
        };
    }, [isOpen, sourceMode]);

    // Distinct cwds for the filter dropdown.
    const cwdOptions = useMemo(() => {
        const set = new Set<string>();
        for (const s of sessions) {
            if (s.cwd) set.add(s.cwd);
        }
        return Array.from(set).sort();
    }, [sessions]);

    // Filter visible sessions by search + cwd.
    const filteredSessions = useMemo(() => {
        const q = sessionSearch.trim().toLowerCase();
        return sessions.filter((s) => {
            if (cwdFilter && s.cwd !== cwdFilter) return false;
            if (!q) return true;
            const hay = `${s.cwd ?? ''} ${s.first_user_text ?? ''} ${s.session_id}`.toLowerCase();
            return hay.includes(q);
        });
    }, [sessions, sessionSearch, cwdFilter]);

    // Filter accounts.
    const filteredAccounts = useMemo(() => {
        const q = accountSearch.trim().toLowerCase();
        if (!q) return accounts;
        return accounts.filter((a) =>
            a.name.toLowerCase().includes(q) ||
            (a.notes ?? '').toLowerCase().includes(q) ||
            (a.relay_base_url ?? '').toLowerCase().includes(q),
        );
    }, [accounts, accountSearch]);

    const effectiveSessionId = sourceMode === 'recent' ? selectedSessionId : manualSessionId.trim();
    const sessionValid = sourceMode === 'recent'
        ? !!selectedSessionId
        : isLikelyUuid(manualSessionId);
    const canSubmit = sessionValid && !!selectedAccountId && !submitting;

    const handleSubmit = async () => {
        if (!effectiveSessionId || !selectedAccountId) return;
        setError(null);
        setSubmitting(true);
        try {
            const route = await invoke<SessionRoute>('add_session_route', {
                sessionId: effectiveSessionId,
                accountId: selectedAccountId,
                label: label.trim() || null,
            });
            onSuccess?.(route);
            onClose();
        } catch (e) {
            setError(typeof e === 'string' ? e : String(e));
        } finally {
            setSubmitting(false);
        }
    };

    if (!isOpen) return null;

    return (
        <div className="cs-relay-modal cs-relay-modal__overlay" onClick={onClose}>
            <div className="cs-relay-modal__panel" onClick={(e) => e.stopPropagation()}>
                <div className="cs-relay-modal__header">
                    <div className="cs-relay-modal__title">
                        <div className="cs-relay-modal__icon">↦</div>
                        <h2>添加路由</h2>
                        <span className="cs-relay-modal__sub">把会话钉到账号</span>
                    </div>
                    <button className="cs-relay-modal__close" onClick={onClose}>×</button>
                </div>

                <div className="cs-relay-modal__body">
                    <div className="cs-route-modal__body-grid">
                        {/* Step 1: 选择会话 */}
                        <div className="cs-route-section">
                            <div className="cs-route-section__head">
                                <div className="cs-route-section__title">
                                    <span className="cs-route-section__num">1</span>
                                    选择会话
                                </div>
                                <div className="cs-route-toggle">
                                    <button
                                        type="button"
                                        className={`cs-route-toggle__btn${sourceMode === 'recent' ? ' cs-route-toggle__btn--active' : ''}`}
                                        onClick={() => setSourceMode('recent')}
                                    >
                                        从最近会话挑选
                                    </button>
                                    <button
                                        type="button"
                                        className={`cs-route-toggle__btn${sourceMode === 'manual' ? ' cs-route-toggle__btn--active' : ''}`}
                                        onClick={() => setSourceMode('manual')}
                                    >
                                        手动输入 ID
                                    </button>
                                </div>
                            </div>

                            {activeSession && (
                                <button
                                    type="button"
                                    className="cs-route-active-banner"
                                    onClick={() => {
                                        setSourceMode('recent');
                                        setSelectedSessionId(activeSession.session_id);
                                    }}
                                    title={activeSession.cwd ?? ''}
                                >
                                    <span className="cs-route-active-banner__icon">●</span>
                                    <span className="cs-route-active-banner__label">
                                        当前活跃会话{activeSession.cwd ? `（${basename(activeSession.cwd) || activeSession.cwd}）` : ''}
                                    </span>
                                    <span className="cs-route-active-banner__sid">
                                        {shortSessionId(activeSession.session_id)}
                                    </span>
                                    <span className="cs-route-active-banner__cta">
                                        {selectedSessionId === activeSession.session_id ? '✓ 已选' : '一键使用 →'}
                                    </span>
                                </button>
                            )}

                            {sourceMode === 'recent' ? (
                                <>
                                    <div className="cs-route-filter-row">
                                        <input
                                            className="cs-rinput"
                                            placeholder="搜索 cwd / 首条消息…"
                                            value={sessionSearch}
                                            onChange={(e) => setSessionSearch(e.target.value)}
                                        />
                                        <select
                                            className="cs-rselect"
                                            value={cwdFilter}
                                            onChange={(e) => setCwdFilter(e.target.value)}
                                        >
                                            <option value="">全部项目</option>
                                            {cwdOptions.map((c) => (
                                                <option key={c} value={c}>{basename(c) || c}</option>
                                            ))}
                                        </select>
                                    </div>

                                    {sessionsLoading ? (
                                        <div className="cs-route-loading">加载最近会话…</div>
                                    ) : sessionsError ? (
                                        <div className="cs-rerror">{sessionsError}</div>
                                    ) : filteredSessions.length === 0 ? (
                                        <div className="cs-route-empty">
                                            没有匹配的最近会话。试试调整搜索词或切换到「手动输入 ID」。
                                        </div>
                                    ) : (
                                        <div className="cs-route-session-list">
                                            {filteredSessions.map((s) => {
                                                const selected = selectedSessionId === s.session_id;
                                                return (
                                                    <button
                                                        key={s.session_id}
                                                        type="button"
                                                        className={`cs-route-session-row${selected ? ' cs-route-session-row--selected' : ''}`}
                                                        onClick={() => setSelectedSessionId(s.session_id)}
                                                    >
                                                        <div className="cs-route-session-row__top">
                                                            <span className="cs-route-session-row__time">
                                                                {formatRelativeTime(s.started_at)}
                                                            </span>
                                                            <span className="cs-route-session-row__sid">
                                                                {shortSessionId(s.session_id)}
                                                            </span>
                                                            {s.model && (
                                                                <span className="cs-rbadge cs-rbadge--mono cs-route-session-row__model">
                                                                    {s.model}
                                                                </span>
                                                            )}
                                                            <span className="cs-route-session-row__cwd" title={s.cwd ?? ''}>
                                                                {basename(s.cwd) || '—'}
                                                            </span>
                                                        </div>
                                                        {s.first_user_text && (
                                                            <div className="cs-route-session-row__preview">
                                                                {truncate(s.first_user_text, 80)}
                                                            </div>
                                                        )}
                                                    </button>
                                                );
                                            })}
                                        </div>
                                    )}
                                </>
                            ) : (
                                <>
                                    <input
                                        className="cs-rinput cs-rinput--mono"
                                        placeholder="粘贴 session_id（UUID 格式，36 字符）"
                                        value={manualSessionId}
                                        onChange={(e) => setManualSessionId(e.target.value)}
                                    />
                                    {manualSessionId && !isLikelyUuid(manualSessionId) && (
                                        <div className="cs-route-hint" style={{ color: 'var(--r-accent-amber, #f59e0b)' }}>
                                            看起来不像 UUID。请检查长度是否为 36 字符、包含 4 个连字符。
                                        </div>
                                    )}
                                </>
                            )}
                        </div>

                        {/* Step 2: 选择目标账号 */}
                        <div className="cs-route-section">
                            <div className="cs-route-section__head">
                                <div className="cs-route-section__title">
                                    <span className="cs-route-section__num">2</span>
                                    选择目标账号
                                </div>
                                <input
                                    className="cs-rinput"
                                    style={{ flex: '0 0 220px' }}
                                    placeholder="搜索账号…"
                                    value={accountSearch}
                                    onChange={(e) => setAccountSearch(e.target.value)}
                                />
                            </div>

                            {filteredAccounts.length === 0 ? (
                                <div className="cs-route-empty">没有账号。先去「账号管理」添加。</div>
                            ) : (
                                <div className="cs-route-account-list">
                                    {filteredAccounts.map((a) => {
                                        const selected = selectedAccountId === a.id;
                                        const badge = categoryBadge(a);
                                        const health = accountHealth(a);
                                        const quota = accountQuotaSummary(a);
                                        return (
                                            <button
                                                key={a.id}
                                                type="button"
                                                className={`cs-route-account-row${selected ? ' cs-route-account-row--selected' : ''}`}
                                                onClick={() => setSelectedAccountId(a.id)}
                                            >
                                                <span className="cs-route-account-row__radio" aria-hidden />
                                                <span className="cs-route-account-row__name" title={a.name}>
                                                    {a.name}
                                                </span>
                                                <span className={badge.className}>{badge.label}</span>
                                                <span
                                                    className={`cs-route-account-row__health cs-route-account-row__health--${health}`}
                                                    title={health === 'ok' ? '健康' : '失效或封号'}
                                                />
                                                {quota && (
                                                    <span className="cs-route-account-row__quota" title={quota}>
                                                        {quota}
                                                    </span>
                                                )}
                                            </button>
                                        );
                                    })}
                                </div>
                            )}
                        </div>

                        {/* Step 3: 备注 */}
                        <div className="cs-route-section">
                            <div className="cs-route-section__head">
                                <div className="cs-route-section__title">
                                    <span className="cs-route-section__num">3</span>
                                    备注（可选）
                                </div>
                            </div>
                            <input
                                className="cs-rinput"
                                placeholder="例如：GLM 跑文档"
                                value={label}
                                onChange={(e) => setLabel(e.target.value)}
                                maxLength={64}
                            />
                            <div className="cs-route-hint">在路由列表里显示这个名字，方便识别。</div>
                        </div>

                        {error && <div className="cs-rerror">{error}</div>}
                    </div>
                </div>

                <div className="cs-relay-modal__footer">
                    <span style={{ fontSize: 11, color: 'var(--r-fg-muted)' }}>
                        {sessionValid && selectedAccountId
                            ? '准备就绪 — 点「添加路由」'
                            : '选好会话和账号即可添加'}
                    </span>
                    <div style={{ display: 'flex', gap: 8 }}>
                        <button className="cs-rbtn cs-rbtn--ghost" onClick={onClose} disabled={submitting}>
                            取消
                        </button>
                        <button
                            className="cs-rbtn cs-rbtn--purple"
                            onClick={handleSubmit}
                            disabled={!canSubmit}
                        >
                            {submitting ? '添加中…' : '添加路由'}
                        </button>
                    </div>
                </div>
            </div>
        </div>
    );
}
