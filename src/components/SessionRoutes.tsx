import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Pencil, RefreshCw, Trash2 } from 'lucide-react';
import { useAccounts, Account, effectiveKind } from '../hooks/useAccounts';
import {
    SessionRoute,
    shortSessionId,
    formatRelativeTime,
} from '../data/session_routes';
import { AddRouteModal } from './AddRouteModal';
import { ConfirmModal } from './ConfirmModal';
import './SessionRoutes.css';

function accountBadge(account: Account | undefined): { label: string; className: string } | null {
    if (!account) return null;
    const kind = effectiveKind(account);
    if (kind === 'chatgpt_oauth') return { label: '订阅', className: 'badge kind-chatgpt' };
    if (kind === 'openai_key') return { label: 'API', className: 'badge kind-openai' };
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

function accountWarning(account: Account | undefined): string | null {
    if (!account) return '目标账号已被删除';
    const flags: string[] = [];
    if (account.is_banned) flags.push('已封号');
    if (account.is_token_invalid) flags.push('Token 失效');
    if (account.is_logged_out) flags.push('需重新登录');
    return flags.length ? `目标账号异常：${flags.join(' · ')}` : null;
}

export function SessionRoutes() {
    const { accounts } = useAccounts();

    const [routes, setRoutes] = useState<SessionRoute[]>([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);

    const [search, setSearch] = useState('');
    const [showAddModal, setShowAddModal] = useState(false);

    // copy-feedback id
    const [copiedId, setCopiedId] = useState<string | null>(null);

    // edit-label inline modal
    const [editTarget, setEditTarget] = useState<SessionRoute | null>(null);
    const [editLabel, setEditLabel] = useState('');
    const [editSaving, setEditSaving] = useState(false);

    // delete confirm
    const [deleteTarget, setDeleteTarget] = useState<SessionRoute | null>(null);
    const [deleting, setDeleting] = useState(false);

    const accountMap = useMemo(() => {
        const m = new Map<string, Account>();
        for (const a of accounts) m.set(a.id, a);
        return m;
    }, [accounts]);

    const load = useCallback(async () => {
        setError(null);
        try {
            const rows = await invoke<SessionRoute[]>('list_session_routes');
            setRoutes(rows);
        } catch (e) {
            setError(typeof e === 'string' ? e : String(e));
        } finally {
            setLoading(false);
        }
    }, []);

    useEffect(() => {
        load();
    }, [load]);

    // Search across label / session_id / cwd. cwd lives only on CodexSession
    // (not on SessionRoute), so this is "label + session_id + target account
    // name" in practice. Spec says cwd, but we don't have it without an extra
    // RPC; matching against the joined account name gives a similar feel.
    const filtered = useMemo(() => {
        const q = search.trim().toLowerCase();
        if (!q) return routes;
        return routes.filter((r) => {
            const acc = accountMap.get(r.account_id);
            const hay = `${r.label ?? ''} ${r.session_id} ${acc?.name ?? ''}`.toLowerCase();
            return hay.includes(q);
        });
    }, [routes, search, accountMap]);

    const enabledCount = useMemo(() => routes.filter((r) => r.enabled).length, [routes]);

    const handleToggle = async (route: SessionRoute) => {
        // Optimistic update.
        const next = !route.enabled;
        setRoutes((prev) => prev.map((r) => (r.id === route.id ? { ...r, enabled: next } : r)));
        try {
            await invoke('toggle_session_route', { id: route.id, enabled: next });
        } catch (e) {
            // Roll back.
            setRoutes((prev) => prev.map((r) => (r.id === route.id ? { ...r, enabled: route.enabled } : r)));
            setError(typeof e === 'string' ? e : String(e));
        }
    };

    const handleCopySid = async (route: SessionRoute) => {
        try {
            await navigator.clipboard.writeText(route.session_id);
            setCopiedId(route.id);
            setTimeout(() => setCopiedId((cur) => (cur === route.id ? null : cur)), 1500);
        } catch (e) {
            console.error('copy failed', e);
        }
    };

    const openEdit = (route: SessionRoute) => {
        setEditTarget(route);
        setEditLabel(route.label ?? '');
    };

    const submitEdit = async () => {
        if (!editTarget) return;
        const newLabel = editLabel.trim() || null;
        setEditSaving(true);
        try {
            await invoke('update_session_route_label', { id: editTarget.id, label: newLabel });
            setRoutes((prev) => prev.map((r) => (r.id === editTarget.id ? { ...r, label: newLabel } : r)));
            setEditTarget(null);
        } catch (e) {
            setError(typeof e === 'string' ? e : String(e));
        } finally {
            setEditSaving(false);
        }
    };

    const confirmDelete = async () => {
        if (!deleteTarget) return;
        setDeleting(true);
        try {
            await invoke('delete_session_route', { id: deleteTarget.id });
            setRoutes((prev) => prev.filter((r) => r.id !== deleteTarget.id));
            setDeleteTarget(null);
        } catch (e) {
            setError(typeof e === 'string' ? e : String(e));
        } finally {
            setDeleting(false);
        }
    };

    return (
        <div className="sr-page">
            <div className="sr-topbar">
                <div className="sr-topbar__left">
                    <button className="sr-btn-add" onClick={() => setShowAddModal(true)}>
                        + 添加路由
                    </button>
                </div>
                <input
                    className="sr-topbar__search"
                    placeholder="搜索 label / session_id / 账号…"
                    value={search}
                    onChange={(e) => setSearch(e.target.value)}
                />
                <div className="sr-topbar__right">
                    <button
                        className="sr-iconbtn"
                        onClick={load}
                        disabled={loading}
                        title="刷新"
                    >
                        <RefreshCw size={14} />
                    </button>
                    <span className="sr-topbar__count">
                        共 <strong>{routes.length}</strong> 条规则，启用 <strong>{enabledCount}</strong> 条
                    </span>
                </div>
            </div>

            {error && <div className="sr-error">{error}</div>}

            {loading ? (
                <div className="sr-empty">加载路由规则…</div>
            ) : routes.length === 0 ? (
                <div className="sr-empty">
                    还没有路由规则。点 <strong>+ 添加</strong>。
                    <br />
                    指定 codex 会话强制使用某个账号，绕过全局自动切号。
                </div>
            ) : filtered.length === 0 ? (
                <div className="sr-empty">没有匹配「{search}」的规则。</div>
            ) : (
                <div className="sr-list">
                    {filtered.map((r) => {
                        const acc = accountMap.get(r.account_id);
                        const badge = accountBadge(acc);
                        const warning = accountWarning(acc);
                        const label = r.label || shortSessionId(r.session_id);
                        return (
                            <div
                                key={r.id}
                                className={`sr-card${r.enabled ? '' : ' sr-card--disabled'}`}
                            >
                                <div className="sr-card__row1">
                                    <label className="sr-switch" title={r.enabled ? '已启用' : '已禁用'}>
                                        <input
                                            type="checkbox"
                                            checked={r.enabled}
                                            onChange={() => handleToggle(r)}
                                        />
                                        <span className="sr-switch__slider" />
                                    </label>
                                    <span className="sr-card__label" title={r.label ?? r.session_id}>
                                        {label}
                                    </span>
                                    <div className="sr-card__actions">
                                        <button
                                            className="sr-iconbtn"
                                            onClick={() => openEdit(r)}
                                            title="编辑备注"
                                        >
                                            <Pencil size={13} />
                                        </button>
                                        <button
                                            className="sr-iconbtn"
                                            onClick={() => setDeleteTarget(r)}
                                            title="删除路由"
                                        >
                                            <Trash2 size={13} />
                                        </button>
                                    </div>
                                </div>

                                <div className="sr-card__row2">
                                    <span>session:</span>
                                    <span
                                        className={`sr-card__sid${copiedId === r.id ? ' sr-card__sid--copied' : ''}`}
                                        onClick={() => handleCopySid(r)}
                                        title="点击复制完整 session_id"
                                    >
                                        {copiedId === r.id ? '已复制 ✓' : r.session_id}
                                    </span>
                                </div>

                                <div className="sr-card__row3">
                                    <span className="sr-card__arrow">→</span>
                                    {acc ? (
                                        <>
                                            <span className="sr-card__account-name">{acc.name}</span>
                                            {badge && <span className={badge.className}>{badge.label}</span>}
                                        </>
                                    ) : (
                                        <span className="sr-card__account-missing">
                                            未知账号（已删除？ id: {r.account_id.slice(0, 8)}）
                                        </span>
                                    )}
                                </div>

                                <div className="sr-card__row4">
                                    <span>命中 {r.hit_count} 次</span>
                                    <span className="sr-card__meta-sep">·</span>
                                    <span>最近 {r.last_hit_at ? formatRelativeTime(r.last_hit_at) : '从未'}</span>
                                    <span className="sr-card__meta-sep">·</span>
                                    <span>创建于 {formatRelativeTime(r.created_at)}</span>
                                </div>

                                {warning && <div className="sr-card__warn">{warning}</div>}
                            </div>
                        );
                    })}
                </div>
            )}

            <AddRouteModal
                isOpen={showAddModal}
                accounts={accounts}
                onClose={() => setShowAddModal(false)}
                onSuccess={() => {
                    load();
                }}
            />

            {/* Inline edit-label modal */}
            {editTarget && (
                <div className="sr-edit-modal__overlay" onClick={() => !editSaving && setEditTarget(null)}>
                    <div className="sr-edit-modal__panel" onClick={(e) => e.stopPropagation()}>
                        <div className="sr-edit-modal__title">编辑备注</div>
                        <input
                            className="sr-edit-modal__input"
                            placeholder="例如：GLM 跑文档"
                            value={editLabel}
                            onChange={(e) => setEditLabel(e.target.value)}
                            maxLength={64}
                            autoFocus
                            onKeyDown={(e) => {
                                if (e.key === 'Enter') submitEdit();
                                if (e.key === 'Escape') setEditTarget(null);
                            }}
                        />
                        <div className="sr-edit-modal__footer">
                            <button
                                className="sr-btn-ghost"
                                onClick={() => setEditTarget(null)}
                                disabled={editSaving}
                            >
                                取消
                            </button>
                            <button
                                className="sr-btn-confirm"
                                onClick={submitEdit}
                                disabled={editSaving}
                            >
                                {editSaving ? '保存中…' : '保存'}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Delete confirm */}
            <ConfirmModal
                isOpen={!!deleteTarget}
                title="确认删除路由"
                message={
                    <>
                        <p>确认删除路由「{deleteTarget?.label || shortSessionId(deleteTarget?.session_id ?? '')}」？</p>
                        <p style={{ color: 'var(--text-muted)', fontSize: 12, marginTop: 8 }}>
                            删除后该 session 将恢复全局自动切号。
                        </p>
                    </>
                }
                confirmText="删除"
                cancelText="取消"
                onConfirm={confirmDelete}
                onCancel={() => setDeleteTarget(null)}
                isLoading={deleting}
            />
        </div>
    );
}

export default SessionRoutes;
