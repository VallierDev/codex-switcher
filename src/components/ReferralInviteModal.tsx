import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface Grant { recipient?: string; grant_type?: string; amount?: number }
interface Offer {
    should_show?: boolean;
    offer_id?: string | null;
    grants?: Grant[];
    remaining_send_capacity?: number;
    remaining_reward_capacity?: number;
    requires_explicit_confirmation?: boolean;
}
interface Invite { email?: string; referral_id?: string; invite_url?: string; status?: string }
interface Tracking { items: Invite[]; cursor?: string | null }
interface Result { invites: Invite[]; failed_emails?: string[]; grants?: Grant[]; offer_id?: string; message?: string }

function reward(offer: Offer): string {
    const grants = (offer.grants ?? []).filter(g => g.recipient === 'referrer' && (g.amount ?? 0) > 0);
    if (grants.length) return grants.map(g => {
        const unit = g.grant_type === 'personal_credits' ? '个使用额度'
            : g.grant_type === 'workspace_credits' ? '个工作区额度'
                : g.grant_type?.includes('rate_limit_reset') ? '次限额重置' : `（${g.grant_type ?? '奖励'}）`;
        return `${g.amount?.toLocaleString()} ${unit}`;
    }).join(' + ');
    if (!offer.grants?.length) {
        const amount = { credits_250: 250, credits_500: 500, credits_1000: 1000 }[offer.offer_id ?? ''];
        if (amount) return `${amount.toLocaleString()} 个使用额度`;
    }
    return '活动未提供奖励数额';
}

function capacity(offer: Offer | null): number {
    if (!offer?.should_show) return 0;
    let cap = Math.min(5, offer.remaining_send_capacity ?? 0);
    if (offer.grants?.length || (offer.offer_id != null && offer.offer_id !== 'none'))
        cap = Math.min(cap, offer.remaining_reward_capacity ?? 0);
    return Math.max(0, cap);
}

export function ReferralInviteModal({ id, name, onClose }: { id: string; name: string; onClose: () => void }) {
    const [program, setProgram] = useState('codex_referral_consumer');
    const [offer, setOffer] = useState<Offer | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    const [input, setInput] = useState('');
    const [confirmed, setConfirmed] = useState(false);
    const [sending, setSending] = useState(false);
    const [result, setResult] = useState<Result | null>(null);
    const [records, setRecords] = useState<Invite[]>([]);
    const [cursor, setCursor] = useState<string | null>(null);
    const [trackingLoading, setTrackingLoading] = useState(false);
    const [trackingLoaded, setTrackingLoaded] = useState(false);
    const [trackingError, setTrackingError] = useState('');
    const [submitted, setSubmitted] = useState(false);
    const generation = useRef(0);
    const sendLock = useRef(false);

    async function refreshOffer() {
        const gen = ++generation.current;
        setLoading(true); setError(''); setOffer(null); setConfirmed(false);
        try {
            const data = await invoke<Offer>('get_desktop_referral_eligibility', { id, program });
            if (gen === generation.current) setOffer(data);
        } catch (e) { if (gen === generation.current) setError(String(e)); }
        finally { if (gen === generation.current) setLoading(false); }
    }
    useEffect(() => {
        setInput(''); setResult(null); setRecords([]); setCursor(null);
        setTrackingLoaded(false); setTrackingError(''); setSubmitted(false);
        void refreshOffer();
        return () => { generation.current++; };
    }, [id, program]);

    async function tracking(more = false) {
        const gen = generation.current;
        setTrackingLoading(true); setTrackingError('');
        try {
            const data = await invoke<Tracking>('get_desktop_referral_tracking', { id, program, cursor: more ? cursor : null });
            if (gen !== generation.current) return;
            setRecords(prev => more ? [...prev, ...data.items] : data.items);
            setCursor(data.cursor ?? null); setTrackingLoaded(true);
        } catch (e) { if (gen === generation.current) setTrackingError(String(e)); }
        finally { setTrackingLoading(false); }
    }
    const emails = [...new Map(input.split(/[\s,;]+/).filter(Boolean).map(e => [e.toLowerCase(), e])).values()];
    const cap = capacity(offer);
    const valid = emails.length > 0 && emails.length <= cap && emails.every(e => /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(e));
    const ready = valid && !!offer && (offer.requires_explicit_confirmation === false || confirmed);
    async function send() {
        if (!ready || sendLock.current || submitted) return;
        sendLock.current = true; setSending(true); setSubmitted(true); setError('');
        try {
            setResult(await invoke<Result>('send_desktop_referral_invite', { id, program, emails, expected: offer }));
            await tracking();
        } catch (e) { setError(String(e)); }
        finally { setSending(false); sendLock.current = false; }
    }
    return <div className="modal-overlay" onClick={() => !sending && onClose()}>
        <div className="modal-content" onClick={e => e.stopPropagation()} style={{ maxHeight: '85vh', overflowY: 'auto' }}>
            <div className="modal-header"><div className="header-top">
                <h2>邀请使用 ChatGPT 桌面版</h2>
                <button className="close-btn" onClick={onClose} disabled={sending}>×</button>
            </div></div>
            <div className="modal-body">
                <p className="modal-tip">发邀账号：<strong>{name}</strong></p>
                <label>活动范围 <select value={program} onChange={e => setProgram(e.target.value)} disabled={sending || loading || trackingLoading}>
                    <option value="codex_referral_consumer">个人账号</option>
                    <option value="codex_referral_workspace">工作区</option>
                </select></label>
                <button className="btn btn-ghost btn-sm" onClick={() => void refreshOffer()} disabled={loading || sending || trackingLoading}>刷新资格</button>
                {loading && <p role="status">正在查询活动资格…</p>}
                {offer && <div className="invite-result-card">
                    <div><strong>{offer.should_show ? `每位符合条件的邀请奖励：${reward(offer)}` : '当前账号暂未开放此邀请活动'}</strong>
                        <p>本次最多邀请 {cap} 个邮箱；剩余奖励名额：{offer.remaining_reward_capacity ?? '未提供'}</p>
                        <p>这是活动奖励数额，不是当前可用余额。奖励需对方接受邀请并完成官方要求后到账。</p>
                    </div>
                </div>}
                <textarea aria-label="受邀邮箱" value={input} onChange={e => setInput(e.target.value)} rows={3}
                    placeholder="每行一个邮箱，也可用逗号分隔" style={{ width: '100%', marginTop: 12 }} disabled={sending || submitted} />
                {input && !valid && <p role="alert">请检查邮箱格式和本次可邀请人数（最多 {cap} 个）。</p>}
                {offer && offer.requires_explicit_confirmation !== false && <label>
                    <input type="checkbox" checked={confirmed} onChange={e => setConfirmed(e.target.checked)} disabled={sending || submitted} />
                    我已确认上述收件人及奖励条件，发送邀请不代表奖励已到账。
                </label>}
                {error && <div className="invite-result-card err" role="alert">{error}</div>}
                {result && <div role="status">
                    {result.invites.length > 0 && <p>上游已创建 {result.invites.length} 条邀请。邮件送达及奖励到账以官方记录为准。</p>}
                    {result.grants && <p>本次奖励：{reward(result)}</p>}
                    {result.invites.map((v, i) => <div className="invite-result-card ok" key={v.referral_id || i}>{v.email || '邀请记录已创建'}</div>)}
                    {!!result.failed_emails?.length && <div className="invite-result-card err">未成功：{result.failed_emails.join('、')} {result.message}</div>}
                    {!result.invites.length && !result.failed_emails?.length && <p>未返回已创建的邀请，请查询记录确认。</p>}
                </div>}
                {submitted && <p>本次提交已结束。需要再次邀请时，请先核对记录，再关闭并重新打开此窗口。</p>}
                <hr />
                <button className="btn btn-ghost" onClick={() => void tracking()} disabled={trackingLoading || sending || loading}>
                    {trackingLoading ? '查询中…' : '查询近 90 天邀请记录'}
                </button>
                {trackingError && <p role="alert">{trackingError}</p>}
                {trackingLoaded && !records.length && <p>近 90 天没有邀请记录。</p>}
                {records.map((v, i) => <div className="invite-result-card" key={`${v.referral_id}-${i}`}>
                    <span>{v.email || '—'}</span> <span>{v.status || '上游未提供状态'}</span>
                </div>)}
                {cursor && <button className="btn btn-ghost" onClick={() => void tracking(true)} disabled={trackingLoading || sending || loading}>加载更多</button>}
            </div>
            <div className="modal-footer">
                <button className="btn btn-ghost" onClick={onClose} disabled={sending}>关闭</button>
                <button className="btn btn-primary" onClick={() => void send()} disabled={!ready || sending || loading || submitted}>
                    {sending ? '发送中…' : '发送邀请'}
                </button>
            </div>
        </div>
    </div>;
}
