import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Account, effectiveKind } from '../hooks/useAccounts';
import type { UsageDisplay } from '../hooks/useUsage';
import { ConfirmModal } from './ConfirmModal';

export function AnchorRecoveryPanel({ accounts }: { accounts: Account[] }) {
    const anchor = accounts.find(account => account.is_session_anchor);
    const [usage, setUsage] = useState<UsageDisplay | null>(null);
    const [error, setError] = useState('');
    const [busy, setBusy] = useState(false);
    const [target, setTarget] = useState('');
    const [confirmTarget, setConfirmTarget] = useState('');
    const [recovered, setRecovered] = useState(false);
    const generation = useRef(0);
    const candidates = accounts.filter(account => account.id !== anchor?.id
        && effectiveKind(account) === 'chatgpt_oauth'
        && !account.is_banned && !account.is_token_invalid && !account.is_logged_out);

    const check = async () => {
        if (!anchor) return;
        const current = ++generation.current;
        setBusy(true); setError('');
        try {
            const result = await invoke<UsageDisplay>('get_quota_by_id', { id: anchor.id });
            if (current === generation.current) setUsage(result);
        } catch (err) {
            if (current === generation.current) { setUsage(null); setError(String(err)); }
        } finally { if (current === generation.current) setBusy(false); }
    };

    useEffect(() => {
        setUsage(null); setTarget(''); setConfirmTarget('');
        void check();
        return () => { generation.current++; };
    }, [anchor?.id]);

    const recover = async () => {
        if (!anchor || !confirmTarget) return;
        const current = generation.current;
        setBusy(true); setError('');
        try {
            await invoke('recover_session_anchor', { expectedAnchor: anchor.id, target: confirmTarget });
            setConfirmTarget(''); setRecovered(true);
        } catch (err) { if (current === generation.current) setError(String(err)); }
        finally { if (current === generation.current) setBusy(false); }
    };

    if (!anchor) return null;
    const gate = usage?.desktop_gate;
    const reason = gate?.reason || (gate?.spend_limit_reached ? 'spend_control_reached' : null);
    const blocked = !!reason || gate?.allowed === false || gate?.limit_reached === true;
    const chosen = candidates.find(account => account.id === confirmTarget);

    return <div className="setting-item sub-item" style={{ display: 'block' }}>
        <div className="setting-info">
            <span className="setting-label">手机锚发送检查</span>
            <p className="setting-desc">代理出口有额度，不代表手机锚的工作区允许 Desktop 发送。迁移前会重新检查目标账号。</p>
            <p role="status" className="setting-desc">
                {busy ? '正在检查…' : usage
                    ? blocked ? '手机锚可能阻止 Desktop 发送' : gate?.allowed === true ? '手机锚允许使用' : '手机锚状态无法确认'
                    : '手机锚状态无法确认'}
                {reason && <> · <code>{reason}</code></>}
            </p>
            {reason?.includes('workspace_') && <p className="setting-desc">工作区额度或使用上限已阻止访问，仅切换代理出口不能恢复发送。</p>}
            {error && <p role="alert">{error}</p>}
            {recovered && <p role="status">手机锚已迁移。请重新登录或重载 Codex Desktop；手机端可能需要重新连接。这不代表已验证 Desktop 恢复发送。</p>}
        </div>
        <button type="button" className="action-button" onClick={check} disabled={busy}>检查手机锚</button>
        <select aria-label="迁移目标账号" className="select-input" value={target}
            onChange={event => setTarget(event.target.value)} disabled={busy}>
            <option value="">选择新的手机锚账号</option>
            {candidates.map(account => <option key={account.id} value={account.id} translate="no">{account.name}</option>)}
        </select>
        <button type="button" className="action-button warning" disabled={busy || !target}
            onClick={() => setConfirmTarget(target)}>迁移手机锚</button>
        <ConfirmModal isOpen={!!confirmTarget} title="确认迁移手机锚"
            message={<><p>新的手机锚：<strong translate="no">{chosen?.name}</strong></p>
                <p>会改变 Desktop 登录身份，手机端可能需要重新连接。代理当前账号保持不变。迁移后请重新登录或重载 Desktop。</p></>}
            confirmText="检查并迁移" loadingText="正在检查并迁移…" isLoading={busy}
            onConfirm={recover} onCancel={() => { if (!busy) setConfirmTarget(''); }} />
    </div>;
}
