import { useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { emit } from '@tauri-apps/api/event';
import { ChevronRight } from 'lucide-react';
import { RELAY_PRESETS, RelayPreset } from '../data/relay_presets';
import './AddRelayModal.css';

interface AddRelayModalProps {
    isOpen: boolean;
    onClose: () => void;
    onSuccess?: () => void;
}

const GROUPS: Array<{ id: NonNullable<RelayPreset['group']>; note: string }> = [
    { id: '通用中转', note: '基于 new-api / CLIProxyAPI / sub2api 的第三方中转，原生 /v1/responses' },
    { id: 'CODING PLAN', note: '厂商编程订阅，按服务支持选择 Responses 或 Chat Completions' },
    { id: '三方模型', note: '厂商按量付费 API（DeepSeek / Kimi / 通义 / OpenRouter 等）' },
    { id: '自定义', note: '手动填 base_url' },
];

function ProviderLogo({ preset, large }: { preset: RelayPreset; large?: boolean }) {
    return (
        <div
            className={`cs-logo${large ? ' cs-logo--lg' : ''}`}
            style={{ background: preset.color ?? '#64748B' }}
            aria-hidden
        >
            {preset.mark ?? preset.name.slice(0, 2)}
        </div>
    );
}

function ProtocolBadge({ proto }: { proto: RelayPreset['relay_protocol'] }) {
    const text = proto === 'chat_completions' ? '/chat/completions' : '/v1/responses';
    return <span className="cs-rbadge cs-rbadge--mono">{text}</span>;
}

function ProviderCard({
    preset,
    selected,
    onSelect,
}: {
    preset: RelayPreset;
    selected: boolean;
    onSelect: (p: RelayPreset) => void;
}) {
    const isSubscription = preset.category === 'coding_plan';
    return (
        <button
            type="button"
            className={`cs-pcard${selected ? ' cs-pcard--selected' : ''}`}
            onClick={() => onSelect(preset)}
        >
            <ProviderLogo preset={preset} />
            <div className="cs-pcard__body">
                <div className="cs-pcard__top">
                    <span className="cs-pcard__name">{preset.name}</span>
                    <div className="cs-pcard__tags">
                        {isSubscription && <span className="cs-rbadge cs-rbadge--sub">订阅</span>}
                        <ProtocolBadge proto={preset.relay_protocol} />
                    </div>
                </div>
                {preset.description && <div className="cs-pcard__desc">{preset.description}</div>}
            </div>
        </button>
    );
}

function Step1Picker({
    pickedId,
    onPick,
}: {
    pickedId: string | null;
    onPick: (p: RelayPreset) => void;
}) {
    const grouped = useMemo(() => {
        const map = new Map<string, RelayPreset[]>();
        for (const p of RELAY_PRESETS) {
            const key = p.group ?? '自定义';
            if (!map.has(key)) map.set(key, []);
            map.get(key)!.push(p);
        }
        return map;
    }, []);

    return (
        <div>
            <div className="cs-relay-tip">
                选一个 <strong>中转服务</strong>，base URL 自动填好，下一步只用粘 API Key。
                需要在多家 Coding Plan 之间切换额度的话，可以把同一个服务添加多次（用账号名区分）。
                也支持 <code>codexswitch://</code> deep link 添加。
            </div>
            {GROUPS.map((g) => {
                const items = grouped.get(g.id) ?? [];
                if (items.length === 0) return null;
                return (
                    <div key={g.id} className="cs-relay-section">
                        <div className="cs-relay-section__head">
                            <span className="cs-relay-section__title">{g.id}</span>
                            <span className="cs-relay-section__note">{g.note}</span>
                        </div>
                        <div className="cs-relay-grid">
                            {items.map((p) => (
                                <ProviderCard
                                    key={p.id}
                                    preset={p}
                                    selected={pickedId === p.id}
                                    onSelect={onPick}
                                />
                            ))}
                        </div>
                    </div>
                );
            })}
        </div>
    );
}

interface Step2Props {
    preset: RelayPreset;
    name: string; setName: (v: string) => void;
    baseUrl: string; setBaseUrl: (v: string) => void;
    apiKey: string; setApiKey: (v: string) => void;
    protocol: 'responses' | 'chat_completions'; setProtocol: (v: 'responses' | 'chat_completions') => void;
    usagePreset: string | null; setUsagePreset: (v: string | null) => void;
    usageCookie: string; setUsageCookie: (v: string) => void;
    modelFallback: string; setModelFallback: (v: string) => void;
    modelMapText: string; setModelMapText: (v: string) => void;
    advOpen: boolean; setAdvOpen: (v: boolean) => void;
    onChangeProvider: () => void;
}

function Step2Form(props: Step2Props) {
    const {
        preset,
        name, setName, baseUrl, setBaseUrl, apiKey, setApiKey,
        protocol, setProtocol, usagePreset, setUsagePreset, usageCookie, setUsageCookie,
        modelFallback, setModelFallback, modelMapText, setModelMapText,
        advOpen, setAdvOpen, onChangeProvider,
    } = props;

    const needsCookie = usagePreset === 'mimo_token_plan';
    const keyPlaceholder = `${preset.auth_prefix ?? 'sk-'}••••••••`;

    return (
        <div>
            <div className="cs-selected-card">
                <ProviderLogo preset={preset} large />
                <div className="cs-selected-card__body">
                    <div className="cs-selected-card__top">
                        <span className="cs-selected-card__name">{preset.name}</span>
                        <ProtocolBadge proto={protocol} />
                    </div>
                    <div className="cs-selected-card__url">{baseUrl || '（自定义 base URL）'}</div>
                    {protocol === 'responses' && modelFallback && (
                        <div className="cs-rfield__hint">保存后可在 Codex 中选择此中转的模型，无需切换 ChatGPT 账号。</div>
                    )}
                </div>
                <button type="button" className="cs-selected-card__change" onClick={onChangeProvider}>
                    切换服务
                </button>
            </div>

            <div className="cs-rgrid2">
                <div className="cs-rfield">
                    <label className="cs-rfield__label" htmlFor="cs-relay-name">
                        账号名称<span className="cs-rfield__req">*</span>
                    </label>
                    <input
                        id="cs-relay-name"
                        className="cs-rinput"
                        value={name}
                        onChange={(e) => setName(e.target.value)}
                        placeholder="例如：工作 · GLM Coding"
                    />
                </div>

                <div className="cs-rfield">
                    <label className="cs-rfield__label" htmlFor="cs-relay-key">
                        API Key<span className="cs-rfield__req">*</span>
                        <span className="cs-rfield__hint">{preset.auth_prefix ?? 'sk-'}前缀</span>
                    </label>
                    <input
                        id="cs-relay-key"
                        className="cs-rinput cs-rinput--mono"
                        type="password"
                        value={apiKey}
                        onChange={(e) => setApiKey(e.target.value)}
                        placeholder={keyPlaceholder}
                    />
                </div>

                <div className="cs-rfield cs-rfield--full">
                    <label className="cs-rfield__label" htmlFor="cs-relay-base">
                        API 地址（可修改）<span className="cs-rfield__req">*</span>
                    </label>
                    <input
                        id="cs-relay-base"
                        className="cs-rinput cs-rinput--mono"
                        value={baseUrl}
                        onChange={(e) => setBaseUrl(e.target.value)}
                        placeholder="https://api.example.com/v1"
                    />
                    <span className="cs-rfield__hint">预设仅提供默认地址；使用自有中转时，请填写该中转的 API 地址和对应 Key。</span>
                </div>

                {protocol === 'responses' && (
                    <div className="cs-rfield cs-rfield--full">
                        <label className="cs-rfield__label" htmlFor="cs-relay-model">
                            默认模型 ID（可修改）
                        </label>
                        <input id="cs-relay-model" className="cs-rinput cs-rinput--mono"
                            value={modelFallback} onChange={(e) => setModelFallback(e.target.value)}
                            placeholder="填写此 API 实际支持的模型 ID" />
                        <span className="cs-rfield__hint">会显示在 Codex 模型列表；高级设置可添加其他模型。</span>
                    </div>
                )}

                <div className="cs-rfield">
                    <label className="cs-rfield__label" htmlFor="cs-relay-proto">
                        上游协议
                        <span className="cs-rfield__hint">中转站讲什么 wire format</span>
                    </label>
                    <select
                        id="cs-relay-proto"
                        className="cs-rselect"
                        value={protocol}
                        onChange={(e) => setProtocol(e.target.value as 'responses' | 'chat_completions')}
                    >
                        <option value="responses">responses · /v1/responses</option>
                        <option value="chat_completions">chat_completions · /chat/completions</option>
                    </select>
                </div>

                <div className="cs-rfield">
                    <label className="cs-rfield__label" htmlFor="cs-relay-usage">
                        余额查询
                        <span className="cs-rfield__hint">默认自动探测</span>
                    </label>
                    <select
                        id="cs-relay-usage"
                        className="cs-rselect"
                        value={usagePreset ?? 'auto'}
                        onChange={(e) => setUsagePreset(e.target.value || null)}
                    >
                        <option value="auto">自动探测（推荐 · new-api / sub2api 都能识别）</option>
                        <option value="new_api_dashboard">new_api_dashboard · /v1/dashboard/billing/*</option>
                        <option value="openai_compat">openai_compat · GET /v1/usage</option>
                        <option value="glm_zhipu">glm_zhipu · GLM 自家 quota</option>
                        <option value="kimi_coding">kimi_coding · Kimi 编程套餐 5H / 7D</option>
                        <option value="mimo_token_plan">mimo_token_plan · 需 Cookie</option>
                        <option value="">不拉取</option>
                    </select>
                </div>

                {needsCookie && (
                    <div className="cs-rfield cs-rfield--full">
                        <label className="cs-rfield__label" htmlFor="cs-relay-cookie">
                            MiMo 配额 Cookie
                            <span className="cs-rfield__hint">从 platform.xiaomimimo.com Network 复制 Cookie header</span>
                        </label>
                        <textarea
                            id="cs-relay-cookie"
                            className="cs-rtextarea cs-rinput--mono"
                            rows={3}
                            value={usageCookie}
                            onChange={(e) => setUsageCookie(e.target.value)}
                            placeholder="Cookie: api-platform_serviceToken=...; userId=...; api-platform_ph=..."
                            style={{ resize: 'vertical', fontSize: 12 }}
                        />
                    </div>
                )}
            </div>

            <div className="cs-radv">
                <button
                    type="button"
                    className="cs-radv__toggle"
                    onClick={() => setAdvOpen(!advOpen)}
                >
                    <ChevronRight
                        size={14}
                        className={`cs-radv__chevron${advOpen ? ' cs-radv__chevron--open' : ''}`}
                    />
                    高级设置（模型兜底 / 映射表）
                </button>
                {advOpen && (
                    <div className="cs-radv__body">
                        {protocol !== 'responses' && <div className="cs-rfield">
                            <label className="cs-rfield__label" htmlFor="cs-relay-fallback">
                                模型兜底
                                <span className="cs-rfield__hint">客户端发的 model 未命中映射时统一替换</span>
                            </label>
                            <input
                                id="cs-relay-fallback"
                                className="cs-rinput cs-rinput--mono"
                                value={modelFallback}
                                onChange={(e) => setModelFallback(e.target.value)}
                                placeholder={preset.model_fallback ?? '留空 = 透传不替换'}
                            />
                        </div>}
                        <div className="cs-rfield">
                            <label className="cs-rfield__label" htmlFor="cs-relay-modelmap">
                                模型映射表
                                <span className="cs-rfield__hint">每行 客户端model=中转站model</span>
                            </label>
                            <textarea
                                id="cs-relay-modelmap"
                                className="cs-rtextarea cs-rinput--mono"
                                rows={4}
                                value={modelMapText}
                                onChange={(e) => setModelMapText(e.target.value)}
                                placeholder={'gpt-5.5=glm-5.1\ngpt-4o=glm-5\ngpt-4o-mini=glm-5.1-x'}
                                style={{ resize: 'vertical', fontSize: 12 }}
                            />
                        </div>
                    </div>
                )}
            </div>
        </div>
    );
}

function parseModelMapText(text: string): Record<string, string> {
    const out: Record<string, string> = {};
    for (const line of text.split('\n')) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith('#')) continue;
        const eq = trimmed.indexOf('=');
        if (eq <= 0) continue;
        const k = trimmed.slice(0, eq).trim();
        const v = trimmed.slice(eq + 1).trim();
        if (k && v) out[k] = v;
    }
    return out;
}

export function AddRelayModal({ isOpen, onClose, onSuccess }: AddRelayModalProps) {
    const [step, setStep] = useState<1 | 2>(1);
    const [picked, setPicked] = useState<RelayPreset | null>(null);

    const [name, setName] = useState('');
    const [baseUrl, setBaseUrl] = useState('');
    const [apiKey, setApiKey] = useState('');
    const [protocol, setProtocol] = useState<'responses' | 'chat_completions'>('responses');
    const [usagePreset, setUsagePreset] = useState<string | null>(null);
    const [usageCookie, setUsageCookie] = useState('');
    const [modelFallback, setModelFallback] = useState('');
    const [modelMapText, setModelMapText] = useState('');
    const [advOpen, setAdvOpen] = useState(false);

    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState<string | null>(null);

    // 重置 step 当 modal 关闭
    useEffect(() => {
        if (!isOpen) {
            setStep(1);
            setPicked(null);
            setName('');
            setApiKey('');
            setUsageCookie('');
            setAdvOpen(false);
            setError(null);
            setSubmitting(false);
        }
    }, [isOpen]);

    const handlePick = (p: RelayPreset) => {
        setPicked(p);
        setName(p.name);
        setBaseUrl(p.base_url);
        setProtocol(p.relay_protocol ?? 'responses');
        setUsagePreset(p.usage_preset ?? null);
        setUsageCookie('');
        setModelFallback(p.model_fallback ?? '');
        setModelMapText(p.model_map
            ? Object.entries(p.model_map).map(([k, v]) => `${k}=${v}`).join('\n')
            : '');
        setAdvOpen(false);
        setError(null);
        setStep(2);
    };

    const handleBack = () => {
        setStep(1);
        setError(null);
    };

    const handleSubmit = async () => {
        if (!picked) return;
        setError(null);
        if (!name.trim()) { setError('账号名不能为空'); return; }
        if (!/^https?:\/\//.test(baseUrl.trim())) {
            setError('Base URL 必须以 http:// 或 https:// 开头');
            return;
        }
        // Ollama 等本地推理不需要真 key，宽松校验：非空 + ≥1 字符即可。
        // 真的 sk- / tp- key 通常 ≥30 字符，这里不卡死方便本地场景。
        if (apiKey.trim().length < 1) { setError('API Key 不能为空'); return; }
        if (usagePreset === 'mimo_token_plan' && !usageCookie.trim()) {
            setError('MiMo 配额查询需要粘贴 platform.xiaomimimo.com 的 Cookie；不查配额请把策略改成「不拉取」。');
            return;
        }
        setSubmitting(true);
        try {
            const modelMap = parseModelMapText(modelMapText);
            await invoke('add_relay_account', {
                name: name.trim(),
                baseUrl: baseUrl.trim(),
                apiKey: apiKey.trim(),
                homepage: picked.homepage ?? null,
                usagePreset: usagePreset ?? null,
                usageCookie: usageCookie.trim() || null,
                notes: `from preset:${picked.id}`,
                modelMap: Object.keys(modelMap).length > 0 ? modelMap : null,
                modelFallback: modelFallback.trim() || null,
                relayProtocol: protocol === 'responses' ? null : protocol,
                relayCategory: picked.category ?? 'aggregator',
            });
            await emit('accounts-updated');
            onSuccess?.();
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
                        <div className="cs-relay-modal__icon">⇄</div>
                        <h2>添加中转</h2>
                        <span className="cs-relay-modal__sub">选预设 · 填 Key</span>
                    </div>
                    <button className="cs-relay-modal__close" onClick={onClose}>×</button>
                </div>

                <div className="cs-relay-steps">
                    <div className={`cs-relay-step${step === 1 ? ' cs-relay-step--active' : ' cs-relay-step--done'}`}>
                        <span className="cs-relay-step__num">{step > 1 ? '✓' : '1'}</span>
                        选择中转服务
                    </div>
                    <div className={`cs-relay-step${step === 2 ? ' cs-relay-step--active' : ''}`}>
                        <span className="cs-relay-step__num">2</span>
                        填写凭据
                    </div>
                </div>

                <div className="cs-relay-modal__body">
                    {step === 1 ? (
                        <Step1Picker
                            pickedId={picked?.id ?? null}
                            onPick={handlePick}
                        />
                    ) : picked ? (
                        <>
                            <Step2Form
                                preset={picked}
                                name={name} setName={setName}
                                baseUrl={baseUrl} setBaseUrl={setBaseUrl}
                                apiKey={apiKey} setApiKey={setApiKey}
                                protocol={protocol} setProtocol={setProtocol}
                                usagePreset={usagePreset} setUsagePreset={setUsagePreset}
                                usageCookie={usageCookie} setUsageCookie={setUsageCookie}
                                modelFallback={modelFallback} setModelFallback={setModelFallback}
                                modelMapText={modelMapText} setModelMapText={setModelMapText}
                                advOpen={advOpen} setAdvOpen={setAdvOpen}
                                onChangeProvider={handleBack}
                            />
                            {error && <div className="cs-rerror">{error}</div>}
                        </>
                    ) : null}
                </div>

                <div className="cs-relay-modal__footer">
                    {step === 2 ? (
                        <button className="cs-rbtn cs-rbtn--ghost" onClick={handleBack} disabled={submitting}>
                            ← 返回选择
                        </button>
                    ) : (
                        <span style={{ fontSize: 11, color: 'var(--r-fg-muted)' }}>
                            选完进入下一步，base URL 已自动填好
                        </span>
                    )}
                    <div style={{ display: 'flex', gap: 8 }}>
                        <button className="cs-rbtn cs-rbtn--ghost" onClick={onClose} disabled={submitting}>
                            取消
                        </button>
                        {step === 2 && (
                            <button
                                className="cs-rbtn cs-rbtn--purple"
                                onClick={handleSubmit}
                                disabled={submitting}
                            >
                                {submitting ? '导入中…' : '导入中转站'}
                            </button>
                        )}
                    </div>
                </div>
            </div>
        </div>
    );
}
