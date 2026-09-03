import { useId, useState } from 'react';
import { Asterisk, ChevronDown, ChevronUp, Clock, Sparkles } from 'lucide-react';
import { useShortCountdown } from '../hooks/useCountdown';
import './AntigravityQuota.css';

export interface AntigravityModelQuota {
    remaining_fraction?: number | null;
    reset_time?: string | null;
    updated_at?: string;
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
    const newest = [...models].sort((a, b) => b.localeCompare(a, 'en', { numeric: true }));
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

function ModelQuota({ model, quota, compact = false }: {
    model: string;
    quota: AntigravityModelQuota;
    compact?: boolean;
}) {
    const resetMs = quota.reset_time ? Date.parse(quota.reset_time) : NaN;
    const resetAt = Number.isFinite(resetMs) ? Math.floor(resetMs / 1000) : undefined;
    const countdown = useShortCountdown(resetAt);
    const percentage = typeof quota.remaining_fraction === 'number' && Number.isFinite(quota.remaining_fraction)
        ? Math.max(0, Math.min(100, quota.remaining_fraction * 100)) : undefined;
    const color = percentage === undefined ? 'neutral' : percentage > 50 ? 'green' : percentage > 20 ? 'orange' : 'red';
    const resetText = resetAt === undefined ? '重置未知' : countdown === '--' ? '待刷新' : countdown || '…';
    return (
        <div className={`quota-mini-card google-model-quota ${compact ? 'compact' : 'detail'}`} title={`${model}\n${resetAt === undefined ? '上游未提供重置时间' : `重置时间：${new Date(resetMs).toLocaleString()}`}`}>
            {percentage !== undefined && <div className={`quota-mini-bg ${color}`} style={{ width: `${percentage}%` }} />}
            <div className="quota-mini-content">
                <span className="quota-label">
                    {model.startsWith('claude-') ? <Asterisk className="google-provider-icon claude" aria-hidden="true" /> : <Sparkles className="google-provider-icon gemini" aria-hidden="true" />}
                    <span>{compact ? compactModelName(model) : modelName(model)}</span>
                </span>
                <span className="quota-time neutral"><Clock className="icon-tiny" /><span>{compact ? compactReset(resetText) : resetText}</span></span>
                <span className={`quota-percent ${color}`}>{percentage === undefined ? '未知' : `${Math.round(percentage)}%`}</span>
            </div>
        </div>
    );
}

export function AntigravityQuota({ quotas }: { quotas: Record<string, AntigravityModelQuota> }) {
    const [expanded, setExpanded] = useState(false);
    const panelId = useId();
    const entries = Object.entries(quotas).filter(([, quota]) => quota && typeof quota === 'object')
        .sort(([a], [b]) => a.localeCompare(b, 'en', { numeric: true }));
    const summary = summaryModels(entries.map(([model]) => model));
    if (!entries.length) return <span className="quota-empty">暂无模型额度，点击刷新</span>;
    return (
        <div className="google-quota-overview">
            <div className="quota-grid">
                {summary.map(model => <ModelQuota key={model} model={model} quota={quotas[model]} compact />)}
            </div>
            <button type="button" className="google-quota-toggle" aria-expanded={expanded} aria-controls={panelId}
                onClick={event => { event.stopPropagation(); setExpanded(value => !value); }}>
                {expanded ? <ChevronUp size={12} /> : <ChevronDown size={12} />}
                {expanded ? '收起全部模型' : '查看全部模型'}（{entries.length}）
            </button>
            {expanded && <section id={panelId} className="google-quota-details" aria-label="全部模型额度">
                <div className="google-quota-heading"><strong>全部模型额度</strong><span>剩余额度 · 重置倒计时</span></div>
                <p className="google-quota-note">各模型独立显示；5h 等窗口以 Google 返回的实际重置时间为准。</p>
                <div className="google-quota-models" tabIndex={0} role="region" aria-label="模型额度列表">
                    {entries.map(([model, quota]) => <ModelQuota key={model} model={model} quota={quota} />)}
                </div>
            </section>}
        </div>
    );
}
