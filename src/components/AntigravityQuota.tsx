import { useId, useState } from 'react';
import { Asterisk, ChevronDown, ChevronUp, Clock, Sparkles } from 'lucide-react';
import { useShortCountdown } from '../hooks/useCountdown';
import './AntigravityQuota.css';

export interface QuotaWindow {
    remaining_fraction?: number | null;
    reset_time?: string | null;
}

export interface AntigravityModelQuota extends QuotaWindow {
    updated_at?: string;
    five_hour?: QuotaWindow | null;
    weekly?: QuotaWindow | null;
}

function modelName(model: string): string {
    return model.replace(/(\d)-(\d)/g, '$1.$2').split('-').map(word => ({
        gemini: 'Gemini', claude: 'Claude', pro: 'Pro', flash: 'Flash', sonnet: 'Sonnet',
        opus: 'Opus', thinking: '(Thinking)', high: '(High)', low: '(Low)',
        medium: '(Medium)', agent: 'Agent', lite: 'Lite', image: 'Image',
        gpt: 'GPT', oss: 'OSS',
    }[word] || word)).join(' ');
}

function compactModelName(model: string): string {
    if (model === 'gemini-pro-agent') return 'Gemini Pro';
    return modelName(model).replace(/^Gemini /, 'G').replace(/^Claude /, '')
        .replace(/\s*\((Thinking|High|Low|Medium)\)/g, '');
}

function compactReset(text: string): string {
    return text.replace(/^(\d+)天 (\d+)时.*$/, '$1d $2h')
        .replace(/时/g, 'h').replace(/分/g, 'm').replace(/秒/g, 's').replace(/\s+/g, '');
}

function summaryModels(models: string[]): string[] {
    const newest = [...models].sort(modelOrder);
    const groups = [
        (id: string) => /^gemini-.*pro/.test(id) && (id.endsWith('-high') || id === 'gemini-pro-agent'),
        (id: string) => /^gemini-.*flash/.test(id) && !/image|lite|thinking/.test(id) && (id.endsWith('-high') || id.endsWith('-flash')),
        (id: string) => id.startsWith('claude-sonnet-'),
        (id: string) => id.startsWith('claude-opus-'),
    ];
    const featured = groups.map(matches => newest.find(matches)).filter((id): id is string => !!id);
    const publicModels = newest.filter(id => /^(gemini-|claude-|gpt-oss-)/.test(id));
    return [...new Set([...featured, ...publicModels, ...newest])].slice(0, 4);
}

function modelOrder(a: string, b: string): number {
    const rank = (id: string) => id.startsWith('gemini-') ? 0 : id.startsWith('claude-') ? 1 : id.startsWith('gpt-oss-') ? 2 : 3;
    if (rank(a) !== rank(b)) return rank(a) - rank(b);
    const av = (a.match(/\d+/g) || []).map(Number);
    const bv = (b.match(/\d+/g) || []).map(Number);
    for (let i = 0; i < Math.max(av.length, bv.length); i++) {
        const difference = (bv[i] || 0) - (av[i] || 0);
        if (difference) return difference;
    }
    return a.localeCompare(b, 'en', { numeric: true });
}

function weeklyBlocked(quota: AntigravityModelQuota): boolean {
    if (quota.weekly?.remaining_fraction !== 0) return false;
    const reset = quota.weekly.reset_time ? Date.parse(quota.weekly.reset_time) : NaN;
    return !Number.isFinite(reset) || reset > Date.now();
}

function ModelQuota({ model, quota, compact = false, windowLabel, blocked = false }: {
    model: string;
    quota: QuotaWindow;
    compact?: boolean;
    windowLabel?: '5H' | '7D' | '额度';
    blocked?: boolean;
}) {
    const resetMs = quota.reset_time ? Date.parse(quota.reset_time) : NaN;
    const resetAt = Number.isFinite(resetMs) ? Math.floor(resetMs / 1000) : undefined;
    const countdown = useShortCountdown(resetAt);
    const percentage = typeof quota.remaining_fraction === 'number' && Number.isFinite(quota.remaining_fraction)
        ? Math.max(0, Math.min(100, quota.remaining_fraction * 100)) : undefined;
    const color = percentage === undefined ? 'neutral' : percentage > 50 ? 'green' : percentage > 20 ? 'orange' : 'red';
    const resetText = resetAt === undefined ? '重置未知' : countdown === '--' ? '待刷新' : countdown || '…';
    return (
        <div className={`quota-mini-card google-model-quota ${compact ? 'compact' : 'detail'} ${windowLabel && !compact ? 'google-window-row' : ''}`} title={`${model}\n${windowLabel || ''} ${resetAt === undefined ? '上游未提供重置时间' : `重置时间：${new Date(resetMs).toLocaleString()}`}${blocked ? '\n7D 额度已耗尽，请展开查看' : ''}`}>
            {percentage !== undefined && <div className={`quota-mini-bg ${color}`} style={{ width: `${percentage}%` }} />}
            <div className="quota-mini-content">
                <span className="quota-label">
                    {!compact && windowLabel ? <span>{windowLabel}</span> : <>
                        {model.startsWith('claude-') ? <Asterisk className="google-provider-icon claude" aria-hidden="true" /> : <Sparkles className="google-provider-icon gemini" aria-hidden="true" />}
                        <span>{compact ? compactModelName(model) : modelName(model)}</span>
                    </>}
                </span>
                <span className={`quota-time ${blocked ? 'quota-blocked' : 'neutral'}`}><Clock className="icon-tiny" /><span>{blocked ? '7D耗尽' : compact ? compactReset(resetText) : resetText}</span></span>
                <span className={`quota-percent ${color}`}>{compact && windowLabel && <span className="quota-window-tag">{windowLabel}</span>}{percentage === undefined ? '未知' : `${Math.round(percentage)}%`}</span>
            </div>
        </div>
    );
}

export function AntigravityQuota({ quotas }: { quotas: Record<string, AntigravityModelQuota> }) {
    const [expanded, setExpanded] = useState(false);
    const panelId = useId();
    const entries = Object.entries(quotas).filter(([, quota]) => quota && typeof quota === 'object')
        .sort(([a], [b]) => modelOrder(a, b));
    const summary = summaryModels(entries.map(([model]) => model));
    if (!entries.length) return <span className="quota-empty">暂无模型额度，点击刷新</span>;
    return (
        <div className="google-quota-overview">
            <div className="quota-grid">
                {summary.map(model => {
                    const quota = quotas[model];
                    return <ModelQuota key={model} model={model} quota={quota.five_hour || quota.weekly || quota}
                        windowLabel={quota.five_hour ? '5H' : quota.weekly ? '7D' : '额度'} blocked={weeklyBlocked(quota)} compact />;
                })}
            </div>
            <button type="button" className="google-quota-toggle" aria-expanded={expanded} aria-controls={panelId}
                onClick={event => { event.stopPropagation(); setExpanded(value => !value); }}>
                {expanded ? <ChevronUp size={12} /> : <ChevronDown size={12} />}
                {expanded ? '收起全部模型' : '查看全部模型'}（{entries.length}）
            </button>
            {expanded && <section id={panelId} className="google-quota-details" aria-label="全部模型额度">
                <div className="google-quota-heading"><strong>全部模型额度</strong><span>剩余额度 · 重置倒计时</span></div>
                <p className="google-quota-note">默认优先显示 5H；5H 与 7D 同时生效，同组模型共享额度。未获取的窗口显示未知。</p>
                <div className="google-quota-models" tabIndex={0} role="region" aria-label="模型额度列表">
                    {entries.map(([model, quota]) => <div className="google-model-windows" key={model} title={model}>
                        <strong className="google-model-window-name">{modelName(model)}</strong>
                        <ModelQuota model={model} quota={quota.five_hour || {}} windowLabel="5H" />
                        <ModelQuota model={model} quota={quota.weekly || {}} windowLabel="7D" />
                        {!quota.five_hour && !quota.weekly && <small className="google-quota-note">上游模型额度：{typeof quota.remaining_fraction === 'number' ? `${Math.round(quota.remaining_fraction * 100)}%` : '未知'}；窗口明细未获取</small>}
                    </div>)}
                </div>
            </section>}
        </div>
    );
}
