import { useState, useEffect } from 'react';
import { listen, emit } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { readFile } from '@tauri-apps/plugin-fs';
import { useAccounts } from '../hooks/useAccounts';
import { RELAY_PRESETS } from '../data/relay_presets';
import './AddAccountModal.css';

function statusIcon(status: string): string {
    switch (status) {
        case 'pending': return '·';
        case 'running': return '⟳';
        case 'ok': return '✓';
        case 'fail': return '✕';
        default: return '·';
    }
}

interface AddAccountModalProps {
    isOpen: boolean;
    onClose: () => void;
    onAdd: (name: string, notes?: string) => Promise<void>;
    onSuccess?: () => void;  // 添加成功后的回调，用于刷新父组件列表
}

type TabType = 'official' | 'openai' | 'google' | 'otp_batch' | 'bulk' | 'relay' | 'session';

interface ImportedSessionInfo {
    email: string | null;
    plan_type: string | null;
    account_id: string | null;
    expires_at: string | null;
    has_refresh_token: boolean;
    id_token_synthetic: boolean;
}
interface ImportedAccountItem {
    account: { id: string; name: string };
    info: ImportedSessionInfo;
}
interface ImportSessionResult {
    ok: ImportedAccountItem[];
    errors: { source_path: string; reason: string }[];
}

interface BulkImportSummary {
    format: string;
    parsed: number;
    errors: string[];
}

interface BulkParsedAccountInfo {
    email: string;
    plan_type: string | null;
    account_id: string | null;
    needs_refresh: boolean;
}

interface BulkImportResult {
    summaries: BulkImportSummary[];
    accounts: BulkParsedAccountInfo[];
    fatal: string[];
}

const BULK_FORMAT_LABEL: Record<string, string> = {
    cpa: 'cpa（codex_credentials）',
    sub2api: 'sub2api',
    cockpit: 'Cockpit',
    'four-segment-rt': '四段RT',
    native: 'codex-switcher',
};

function bytesToBase64(bytes: Uint8Array): string {
    const CHUNK = 0x8000;
    let binary = '';
    for (let i = 0; i < bytes.length; i += CHUNK) {
        const slice = bytes.subarray(i, i + CHUNK);
        binary += String.fromCharCode.apply(null, Array.from(slice));
    }
    return btoa(binary);
}

type OtpProvider = 'usmail' | 'sorryios' | 'nissanserena';

interface OtpRow {
    email: string;
    provider: OtpProvider;
    status: 'pending' | 'running' | 'ok' | 'fail';
    stage?: string;
    accountId?: string;
    error?: string;
}

interface OtpEntry {
    email: string;
    provider: OtpProvider;
    token?: string;
}

interface OtpBatchProgress {
    index: number;
    total: number;
    email: string;
    provider: OtpProvider;
    status: 'pending' | 'running' | 'ok' | 'fail';
    stage?: string;
    accountId?: string;
    error?: string;
}

/** 邮箱域名 → 默认 provider 的启发规则（没显式 token 时用） */
function pickProviderByDomain(email: string): OtpProvider {
    const domain = email.split('@')[1]?.toLowerCase() || '';
    // usmail.my.id 服务的域名
    if (domain.endsWith('daymniza.dev')) return 'usmail';
    // 其他常见印尼临时邮箱域名 → nissanserena
    if (
        domain.endsWith('.my.id') ||
        domain.endsWith('.biz.id') ||
        domain.endsWith('.web.id') ||
        domain.endsWith('.co.id')
    )
        return 'nissanserena';
    // 默认还是 usmail（兼容老用法）
    return 'usmail';
}

/**
 * 把粘贴文本解析成 OTP 任务列表。支持以下格式（混用 OK）：
 *   1. xxx@yyy.com                                              → 按域名挑 provider
 *   2. xxx@yyy.com|TOKEN32                                      → sorryios
 *   3. xxx@yyy.com|https://www.sorryios.net/token/TOKEN32       → sorryios
 *   4. 多行块包含 【账号】xxx@yyy.com ... 【验证码查询链接】...TOKEN32  → sorryios
 *   5. 73. xxx@yyy.com:password                                 → 去序号、去密码、按域名挑 provider
 */
function parseOtpEntries(raw: string): OtpEntry[] {
    const text = raw.replace(/\r\n/g, '\n');
    const tokenRe = /[0-9a-f]{32}/i;
    const emailRe = /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/;
    const out: OtpEntry[] = [];
    const seen = new Set<string>();

    // 收集所有 email + 32-hex token 在文本里的位置，按距离配对（sorryios 优先）
    const emails: { email: string; idx: number }[] = [];
    const emailGlobal = new RegExp(emailRe.source, 'g');
    for (const m of text.matchAll(emailGlobal)) {
        if (m.index !== undefined) emails.push({ email: m[0], idx: m.index });
    }
    const tokenGlobal = new RegExp(tokenRe.source, 'gi');
    const tokens: { token: string; idx: number }[] = [];
    for (const m of text.matchAll(tokenGlobal)) {
        if (m.index !== undefined) tokens.push({ token: m[0].toLowerCase(), idx: m.index });
    }
    const usedTokenIdx = new Set<number>();
    for (const e of emails) {
        const key = e.email.toLowerCase();
        if (seen.has(key)) continue;
        // 找前后 400 字符内最近的 token（必须前后无字母前缀，避免误吞 chatgpt id 这种）
        let best: { token: string; idx: number; dist: number } | null = null;
        for (const t of tokens) {
            if (usedTokenIdx.has(t.idx)) continue;
            const dist = Math.abs(t.idx - e.idx);
            if (dist > 400) continue;
            if (!best || dist < best.dist) best = { ...t, dist };
        }
        if (best) {
            usedTokenIdx.add(best.idx);
            out.push({ email: e.email, provider: 'sorryios', token: best.token });
        } else {
            out.push({ email: e.email, provider: pickProviderByDomain(e.email) });
        }
        seen.add(key);
    }
    return out;
}

export function AddAccountModal({ isOpen, onClose, onAdd, onSuccess }: AddAccountModalProps) {
    const { startOAuthLogin, finalizeOAuthLogin } = useAccounts();
    const [activeTab, setActiveTab] = useState<TabType>('openai');
    const [name, setName] = useState('');
    const [notes, setNotes] = useState('');
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [oauthStatus, setOauthStatus] = useState<string>('');
    const [showPasteInput, setShowPasteInput] = useState(false);
    const [callbackInput, setCallbackInput] = useState('');
    const [submittingCallback, setSubmittingCallback] = useState(false);
    // OTP 批量授权
    const [otpInput, setOtpInput] = useState('');
    const [otpTimeout, setOtpTimeout] = useState(180);
    const [otpRows, setOtpRows] = useState<OtpRow[]>([]);
    const [otpRunning, setOtpRunning] = useState(false);
    // 批量导入
    const [bulkBusy, setBulkBusy] = useState(false);
    const [bulkResult, setBulkResult] = useState<BulkImportResult | null>(null);
    const [bulkError, setBulkError] = useState<string | null>(null);
    // ChatGPT Web session 导入（无 refresh_token，access_token 过期前可用）
    const [sessionInput, setSessionInput] = useState('');
    const [sessionBusy, setSessionBusy] = useState(false);
    const [sessionResult, setSessionResult] = useState<ImportSessionResult | null>(null);
    const [sessionError, setSessionError] = useState<string | null>(null);
    // 当前提交：用于失败重试时，把 OtpEntry 数组（含 token）映射回 otpRows 行
    const [otpSubmission, setOtpSubmission] = useState<{
        entries: OtpEntry[];
        rowIndices: number[];
    } | null>(null);
    // 中转站（Relay）
    const [relayPresetId, setRelayPresetId] = useState<string>(RELAY_PRESETS[0]?.id ?? 'custom');
    const [relayName, setRelayName] = useState<string>(RELAY_PRESETS[0]?.name ?? '');
    const [relayBaseUrl, setRelayBaseUrl] = useState<string>(RELAY_PRESETS[0]?.base_url ?? '');
    const [relayApiKey, setRelayApiKey] = useState<string>('');
    const [relayUsagePreset, setRelayUsagePreset] = useState<string | null>(
        RELAY_PRESETS[0]?.usage_preset ?? null,
    );
    const [relayUsageCookie, setRelayUsageCookie] = useState<string>('');
    const [relayModelFallback, setRelayModelFallback] = useState<string>(
        RELAY_PRESETS[0]?.model_fallback ?? '',
    );
    // 上游协议：'responses'（默认 / 上游懂 codex /v1/responses）/ 'chat_completions'（GLM 等只懂 /chat/completions 的）
    const [relayProtocol, setRelayProtocol] = useState<string>(
        RELAY_PRESETS[0]?.relay_protocol ?? 'responses',
    );
    // 模型映射用 textarea（"key=value\n..." 格式）展示给用户编辑
    const [relayModelMapText, setRelayModelMapText] = useState<string>(() => {
        const m = RELAY_PRESETS[0]?.model_map;
        return m ? Object.entries(m).map(([k, v]) => `${k}=${v}`).join('\n') : '';
    });
    const [relaySubmitting, setRelaySubmitting] = useState(false);
    const [relayError, setRelayError] = useState<string | null>(null);

    const handlePickRelayPreset = (id: string) => {
        const preset = RELAY_PRESETS.find(p => p.id === id);
        setRelayPresetId(id);
        if (preset) {
            setRelayName(preset.name);
            setRelayBaseUrl(preset.base_url);
            setRelayUsagePreset(preset.usage_preset ?? null);
            setRelayUsageCookie('');
            setRelayModelFallback(preset.model_fallback ?? '');
            setRelayProtocol(preset.relay_protocol ?? 'responses');
            const m = preset.model_map ?? {};
            setRelayModelMapText(Object.entries(m).map(([k, v]) => `${k}=${v}`).join('\n'));
        }
        setRelayError(null);
    };

    /** 把 textarea 文本解析成 { key: value }，忽略空行 / 注释 / 不含 = 的行 */
    const parseModelMapText = (text: string): Record<string, string> => {
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
    };

    const handleSubmitRelay = async () => {
        setRelayError(null);
        if (!relayName.trim()) {
            setRelayError('账号名不能为空');
            return;
        }
        if (!/^https?:\/\//.test(relayBaseUrl.trim())) {
            setRelayError('Base URL 必须以 http:// 或 https:// 开头');
            return;
        }
        if (relayApiKey.trim().length < 8) {
            setRelayError('API Key 看起来太短');
            return;
        }
        if (relayUsagePreset === 'mimo_token_plan' && !relayUsageCookie.trim()) {
            setRelayError('MiMo 配额查询需要粘贴 platform.xiaomimimo.com 的 Cookie；如果暂时不查配额，请把余额查询策略改成“不拉取”。');
            return;
        }
        setRelaySubmitting(true);
        try {
            const preset = RELAY_PRESETS.find(p => p.id === relayPresetId);
            const modelMap = parseModelMapText(relayModelMapText);
            await invoke('add_relay_account', {
                name: relayName.trim(),
                baseUrl: relayBaseUrl.trim(),
                apiKey: relayApiKey.trim(),
                homepage: preset?.homepage ?? null,
                usagePreset: relayUsagePreset ?? null,
                usageCookie: relayUsageCookie.trim() || null,
                notes: `from preset:${relayPresetId}`,
                modelMap: Object.keys(modelMap).length > 0 ? modelMap : null,
                modelFallback: relayModelFallback.trim() || null,
                relayProtocol: relayProtocol === 'responses' ? null : relayProtocol,
            });
            await emit('accounts-updated');
            // 重置表单
            setRelayApiKey('');
            setRelayUsageCookie('');
            handleClose();
        } catch (e) {
            setRelayError(typeof e === 'string' ? e : String(e));
        } finally {
            setRelaySubmitting(false);
        }
    };

    // 监听后端发来的授权码
    useEffect(() => {
        if (!isOpen) return;

        const unlisten = listen<string>('oauth-callback-received', async (event) => {
            const code = event.payload;
            setOauthStatus('已获取授权码，正在交换令牌...');
            try {
                await finalizeOAuthLogin(code);
                setOauthStatus('授权成功！账号已添加。');
                setLoading(false);
                // 延迟关闭模态框，让用户看到成功提示
                setTimeout(() => {
                    onSuccess?.();  // 通知父组件刷新列表
                    onClose();
                }, 1000);
            } catch (err) {
                setError(String(err));
                setOauthStatus('');
                setLoading(false);
            }
        });

        return () => {
            unlisten.then(f => f());
        };
    }, [isOpen, finalizeOAuthLogin]);

    useEffect(() => {
        if (!isOpen) return;
        const unlisten = listen<string>('antigravity-oauth-callback-received', async (event) => {
            setOauthStatus('已获取 Google 授权码，正在验证账号和项目...');
            try {
                await invoke('finalize_antigravity_oauth_login', { code: event.payload });
                setOauthStatus('Google Antigravity 账号已添加。');
                setLoading(false);
                setTimeout(() => {
                    onSuccess?.();
                    onClose();
                }, 1000);
            } catch (err) {
                setError(String(err));
                setOauthStatus('');
                setLoading(false);
            }
        });
        return () => { unlisten.then(f => f()); };
    }, [isOpen, onClose, onSuccess]);

    // 监听 OTP 批量授权进度（重试时 backend 的 index 是子集索引，要翻译回原 rows index）
    useEffect(() => {
        if (!isOpen) return;
        const unlisten = listen<OtpBatchProgress>('otp-batch-progress', (event) => {
            const p = event.payload;
            setOtpRows(prev => {
                const next = [...prev];
                const targetIdx = otpSubmission?.rowIndices[p.index] ?? p.index;
                next[targetIdx] = {
                    email: p.email,
                    provider: p.provider,
                    status: p.status,
                    stage: p.stage,
                    accountId: p.accountId,
                    error: p.error,
                };
                return next;
            });
        });
        return () => {
            unlisten.then(f => f());
        };
    }, [isOpen, otpSubmission]);

    if (!isOpen) return null;

    // 处理官方导入
    const handleSubmitOfficial = async (e: React.FormEvent) => {
        e.preventDefault();
        if (!name.trim()) {
            setError('请输入账号名称');
            return;
        }

        setLoading(true);
        setError(null);

        try {
            await onAdd(name.trim(), notes.trim() || undefined);
            handleClose();
        } catch (err) {
            setError(String(err));
        } finally {
            setLoading(false);
        }
    };

    // 处理 OpenAI 登录
    const handleOpenAILogin = async () => {
        setLoading(true);
        setError(null);
        setOauthStatus('正在启动官方浏览器授权...');

        try {
            // 启动 OAuth 后端任务，后端会处理打开浏览器和启动监听
            await startOAuthLogin();
            setOauthStatus('请在打开的浏览器窗口中完成 OpenAI 授权...');
        } catch (err) {
            setError(String(err));
            setOauthStatus('');
            setLoading(false);
        }
    };

    // 复制授权链接（不开默认浏览器，让用户自己选要用哪个浏览器粘贴）
    const handleCopyOAuthLink = async () => {
        setLoading(true);
        setError(null);
        setOauthStatus('正在生成授权链接...');

        try {
            const url = await startOAuthLogin(false);
            // 走后端 pbcopy 而不是 navigator.clipboard：webview 跨 await 后 user-gesture 失效会触发 NotAllowedError
            try {
                await invoke('copy_to_clipboard', { text: url });
                setOauthStatus('授权链接已复制，请粘贴到目标浏览器完成授权，回调会自动回到本应用...');
            } catch (copyErr) {
                // 复制失败也别卡住流程：把 URL 显示出来让用户手动复制
                setCallbackInput(url);
                setShowPasteInput(false);
                setOauthStatus(`复制到剪贴板失败（${String(copyErr)}），请手动复制：\n${url}`);
            }
        } catch (err) {
            setError(String(err));
            setOauthStatus('');
            setLoading(false);
        }
    };

    const handleAntigravityLogin = async () => {
        setLoading(true);
        setError(null);
        setOauthStatus('正在启动 Google Antigravity 授权...');
        try {
            await invoke<string>('start_antigravity_oauth_login', { openBrowser: true });
            setOauthStatus('请在浏览器中完成 Google 授权...');
        } catch (err) {
            setError(String(err));
            setOauthStatus('');
            setLoading(false);
        }
    };

    const handleClose = () => {
        // OAuth 进行中也允许关闭：后端 oauth_server 下次 start 时会 abort 旧任务，无需显式取消
        setName('');
        setNotes('');
        setError(null);
        setOauthStatus('');
        setLoading(false);
        setShowPasteInput(false);
        setCallbackInput('');
        // OTP 批量进行中不重置进度，让用户回来继续看
        if (!otpRunning) {
            setOtpInput('');
            setOtpRows([]);
        }
        // 批量导入结果保留到下次打开（用户可能想再回来看），但 bulkBusy 防误触
        // Session 导入：成功后清空输入避免重复提交；保留 result 供回头看
        if (sessionResult && sessionResult.ok.length > 0) {
            setSessionInput('');
        }
        onClose();
    };

    const handleBulkPickAndImport = async () => {
        setBulkError(null);
        setBulkResult(null);
        const selection = await openDialog({
            multiple: true,
            filters: [
                { name: '账号导入文件', extensions: ['json', 'zip', 'txt'] },
                { name: '所有文件', extensions: ['*'] },
            ],
        });
        const paths: string[] = Array.isArray(selection) ? selection : (selection ? [selection] : []);
        if (paths.length === 0) return;
        setBulkBusy(true);
        try {
            const files = await Promise.all(paths.map(async (p) => {
                const bytes = await readFile(p);
                const filename = p.split('/').pop() || p;
                return { filename, content_b64: bytesToBase64(bytes) };
            }));
            const r = await invoke<BulkImportResult>('bulk_import_accounts', { files });
            setBulkResult(r);
            onSuccess?.();
        } catch (e: any) {
            setBulkError(`${e}`);
        } finally {
            setBulkBusy(false);
        }
    };

    // ChatGPT Web session 导入：粘贴 chatgpt.com 的 session JSON（带 accessToken）
    // → 转成我们的 auth.json 并落库。源逻辑参考 gtxx3600/GPTSession2CPAandSub2API。
    // 没有 refresh_token，约 30 天后 access_token 过期需要重新导入。
    const handleSessionImport = async () => {
        setSessionError(null);
        setSessionResult(null);
        if (!sessionInput.trim()) {
            setSessionError('请粘贴 ChatGPT session JSON');
            return;
        }
        setSessionBusy(true);
        try {
            const r = await invoke<ImportSessionResult>('import_chatgpt_session', {
                sessionJson: sessionInput,
            });
            setSessionResult(r);
            if (r.ok.length > 0) {
                await emit('accounts-updated');
                onSuccess?.();
            }
        } catch (e: any) {
            setSessionError(String(e));
        } finally {
            setSessionBusy(false);
        }
    };

    // 邮箱 OTP 批量授权
    const handleOtpBatch = async () => {
        const entries = parseOtpEntries(otpInput);
        if (entries.length === 0) {
            setError('请输入至少一个有效邮箱');
            return;
        }
        setError(null);
        setOtpRunning(true);
        setOtpRows(entries.map(e => ({ email: e.email, provider: e.provider, status: 'pending' })));
        // 首次提交：rowIndices = identity (0..N-1)
        setOtpSubmission({
            entries,
            rowIndices: entries.map((_, i) => i),
        });
        try {
            await invoke('start_otp_login_batch', {
                entries,
                timeoutSecs: otpTimeout,
            });
            onSuccess?.();
        } catch (err) {
            setError(String(err));
        } finally {
            setOtpRunning(false);
        }
    };

    // 仅重跑失败的几条
    const handleRetryFailed = async () => {
        if (!otpSubmission) return;
        // 找出所有失败行的 otpRows 下标
        const failedRowIndices: number[] = [];
        otpRows.forEach((r, i) => {
            if (r.status === 'fail') failedRowIndices.push(i);
        });
        if (failedRowIndices.length === 0) return;

        // 用 email 把 row 映射回 OtpEntry（保留原 token / provider）
        const emailToEntry = new Map<string, OtpEntry>(
            otpSubmission.entries.map(e => [e.email.toLowerCase(), e])
        );
        const retryEntries: OtpEntry[] = [];
        const retryRowIndices: number[] = [];
        for (const ri of failedRowIndices) {
            const row = otpRows[ri];
            const orig = emailToEntry.get(row.email.toLowerCase());
            if (orig) {
                retryEntries.push(orig);
                retryRowIndices.push(ri);
            }
        }
        if (retryEntries.length === 0) return;

        // 把这些行重置回 pending
        setOtpRows(prev =>
            prev.map((r, i) =>
                failedRowIndices.includes(i)
                    ? { ...r, status: 'pending', stage: undefined, error: undefined, accountId: undefined }
                    : r
            )
        );
        setOtpSubmission({ entries: retryEntries, rowIndices: retryRowIndices });
        setError(null);
        setOtpRunning(true);
        try {
            await invoke('start_otp_login_batch', {
                entries: retryEntries,
                timeoutSecs: otpTimeout,
            });
            onSuccess?.();
        } catch (err) {
            setError(String(err));
        } finally {
            setOtpRunning(false);
        }
    };

    // 浏览器跳不回本机时手动提交回调链接
    const handleSubmitCallback = async () => {
        const input = callbackInput.trim();
        if (!input) return;
        setSubmittingCallback(true);
        setError(null);
        try {
            await invoke('submit_oauth_callback', { input });
            // 后端会派发 oauth-callback-received，useEffect 里的监听会走 finalize 流程
            setOauthStatus('已提交回调链接，正在交换令牌...');
            setCallbackInput('');
            setShowPasteInput(false);
        } catch (err) {
            setError(String(err));
        } finally {
            setSubmittingCallback(false);
        }
    };

    return (
        <div className="modal-overlay" onClick={handleClose}>
            <div
                className={`modal-content${activeTab === 'otp_batch' || activeTab === 'relay' ? ' modal-wide' : ''}`}
                onClick={e => e.stopPropagation()}
            >
                <div className="modal-header">
                    <div className="header-top">
                        <h2>添加账号</h2>
                        <button className="close-btn" onClick={handleClose}>
                            ×
                        </button>
                    </div>
                    <div className="modal-tabs">
                        <button
                            className={`tab-item ${activeTab === 'openai' ? 'active' : ''}`}
                            onClick={() => !loading && setActiveTab('openai')}
                        >
                            OpenAI 登录 (推荐)
                        </button>
                        <button
                            className={`tab-item ${activeTab === 'official' ? 'active' : ''}`}
                            onClick={() => !loading && setActiveTab('official')}
                        >
                            从官方导入
                        </button>
                        <button
                            className={`tab-item ${activeTab === 'google' ? 'active' : ''}`}
                            onClick={() => !loading && setActiveTab('google')}
                        >
                            Google / Antigravity
                        </button>
                        <button
                            className={`tab-item ${activeTab === 'otp_batch' ? 'active' : ''}`}
                            onClick={() => !loading && !otpRunning && setActiveTab('otp_batch')}
                        >
                            邮箱 OTP 批量
                        </button>
                        <button
                            className={`tab-item ${activeTab === 'bulk' ? 'active' : ''}`}
                            onClick={() => !loading && !otpRunning && setActiveTab('bulk')}
                        >
                            批量导入文件
                        </button>
                        <button
                            className={`tab-item ${activeTab === 'session' ? 'active' : ''}`}
                            onClick={() => !loading && !otpRunning && setActiveTab('session')}
                        >
                            Session 导入
                        </button>
                        {/* "中转站" tab moved to dedicated AddRelayModal — see App.tsx 顶部 "+ 添加中转" 按钮 */}
                    </div>
                </div>

                <div className="modal-body">
                    {activeTab === 'bulk' ? (
                        <div className="bulk-panel">
                            <p className="modal-tip">
                                自动识别格式，可一次选多个文件：<b>cpa</b>（codex_credentials zip / 单 .json）、
                                <b> sub2api</b>、<b>Cockpit</b>、<b>四段RT</b>
                                （<code>email----xxx----xxx----rt_xxx</code>）、
                                <b> codex-switcher 原生 accounts.json</b>。
                                同邮箱已存在的账号会跳过，不覆盖现有 token。
                            </p>
                            <button
                                className="btn btn-primary btn-full"
                                style={{ padding: '14px' }}
                                onClick={handleBulkPickAndImport}
                                disabled={bulkBusy}
                            >
                                {bulkBusy ? '导入中…' : '选择文件并导入'}
                            </button>
                            {bulkError && <div className="error-msg" style={{ marginTop: 12 }}>{bulkError}</div>}
                            {bulkResult && (
                                <div className="bulk-result" style={{ marginTop: 16 }}>
                                    <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', marginBottom: 12 }}>
                                        <span className="bulk-stat">解析 {bulkResult.summaries.reduce((s, x) => s + x.parsed, 0)}</span>
                                        <span className="bulk-stat ok">新增 {bulkResult.accounts.length}</span>
                                        {bulkResult.summaries.reduce((s, x) => s + x.parsed, 0) - bulkResult.accounts.length > 0 && (
                                            <span className="bulk-stat skip">
                                                跳过 {bulkResult.summaries.reduce((s, x) => s + x.parsed, 0) - bulkResult.accounts.length}（同名）
                                            </span>
                                        )}
                                        {bulkResult.fatal.length > 0 && (
                                            <span className="bulk-stat fail">失败 {bulkResult.fatal.length}</span>
                                        )}
                                    </div>
                                    {bulkResult.summaries.map((s, i) => (
                                        <div key={i} className="bulk-summary-item">
                                            <span className="format-tag">{BULK_FORMAT_LABEL[s.format] || s.format}</span>
                                            <span>解析 {s.parsed} 个账号</span>
                                        </div>
                                    ))}
                                    {bulkResult.fatal.map((msg, i) => (
                                        <div key={`f-${i}`} className="bulk-fatal">⚠️ {msg}</div>
                                    ))}
                                    {bulkResult.accounts.length > 0 && (
                                        <details style={{ marginTop: 8 }}>
                                            <summary style={{ cursor: 'pointer', color: '#aaa', fontSize: '12.5px', padding: '6px 0' }}>
                                                新增账号详情（{bulkResult.accounts.length}）
                                            </summary>
                                            <table className="bulk-table">
                                                <thead>
                                                    <tr><th>Email</th><th>Plan</th><th>状态</th></tr>
                                                </thead>
                                                <tbody>
                                                    {bulkResult.accounts.map((a, i) => (
                                                        <tr key={i}>
                                                            <td>{a.email}</td>
                                                            <td>{a.plan_type || '—'}</td>
                                                            <td>{a.needs_refresh ? <span className="needs-refresh">⚠ 仅 RT，首次请求自动 refresh</span> : '✓ ready'}</td>
                                                        </tr>
                                                    ))}
                                                </tbody>
                                            </table>
                                        </details>
                                    )}
                                </div>
                            )}
                        </div>
                    ) : activeTab === 'session' ? (
                        <div className="bulk-panel">
                            <p className="modal-tip">
                                粘贴 <b>chatgpt.com 网页登录的 session JSON</b>（带 <code>accessToken / user.email / account.id</code>），
                                绕过 Codex 手机验证。支持单个对象、对象数组、或者嵌套容器，会自动递归识别。
                                <br />
                                <b style={{ color: 'var(--text-secondary)' }}>注意：</b>
                                Web session 没有 <code>refresh_token</code>，<code>access_token</code> 失效（约 30 天）后账号会变成不可用，
                                需要重新粘贴一次新 session。Plus 账号能正常调用模型，Free 账号即使导入也无 API 权限。
                            </p>
                            <textarea
                                className="text-input"
                                style={{ width: '100%', minHeight: 260, fontFamily: 'monospace', fontSize: 12 }}
                                value={sessionInput}
                                onChange={e => setSessionInput(e.target.value)}
                                placeholder={`{\n  "user": {"id": "user-...", "email": "you@example.com"},\n  "expires": "2026-08-06T14:29:36.155Z",\n  "account": {"id": "uuid", "planType": "plus"},\n  "accessToken": "eyJhbGciOi...",\n  "sessionToken": "..."\n}`}
                                disabled={sessionBusy}
                            />
                            <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
                                <button
                                    className="btn btn-primary"
                                    style={{ flex: 1, padding: '12px' }}
                                    onClick={handleSessionImport}
                                    disabled={sessionBusy || !sessionInput.trim()}
                                >
                                    {sessionBusy ? '导入中…' : '解析并导入'}
                                </button>
                                <button
                                    className="btn btn-ghost"
                                    onClick={() => { setSessionInput(''); setSessionResult(null); setSessionError(null); }}
                                    disabled={sessionBusy || (!sessionInput && !sessionResult && !sessionError)}
                                >
                                    清空
                                </button>
                            </div>
                            {sessionError && <div className="error-message" style={{ marginTop: 12 }}>{sessionError}</div>}
                            {sessionResult && (
                                <div className="bulk-result" style={{ marginTop: 16 }}>
                                    <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', marginBottom: 12 }}>
                                        <span className="bulk-stat ok">新增 {sessionResult.ok.length}</span>
                                        {sessionResult.errors.length > 0 && (
                                            <span className="bulk-stat fail">失败 {sessionResult.errors.length}</span>
                                        )}
                                    </div>
                                    {sessionResult.ok.length > 0 && (
                                        <details open style={{ marginTop: 8 }}>
                                            <summary style={{ cursor: 'pointer', color: 'var(--text-secondary)', fontSize: 12.5, padding: '6px 0' }}>
                                                导入账号详情（{sessionResult.ok.length}）
                                            </summary>
                                            <table className="bulk-table">
                                                <thead>
                                                    <tr><th>Email</th><th>Plan</th><th>说明</th></tr>
                                                </thead>
                                                <tbody>
                                                    {sessionResult.ok.map((item, i) => (
                                                        <tr key={i}>
                                                            <td>{item.info.email || item.account.name}</td>
                                                            <td>{item.info.plan_type || '—'}</td>
                                                            <td>
                                                                {item.info.has_refresh_token
                                                                    ? '✓ 含 refresh_token'
                                                                    : <span className="needs-refresh">⚠ 无 refresh_token，到期后需重新导入</span>}
                                                                {item.info.id_token_synthetic ? '（id_token 合成）' : ''}
                                                            </td>
                                                        </tr>
                                                    ))}
                                                </tbody>
                                            </table>
                                        </details>
                                    )}
                                    {sessionResult.errors.length > 0 && (
                                        <details open style={{ marginTop: 8 }}>
                                            <summary style={{ cursor: 'pointer', color: 'var(--danger, #c54)', fontSize: 12.5, padding: '6px 0' }}>
                                                失败项（{sessionResult.errors.length}）
                                            </summary>
                                            {sessionResult.errors.map((err, i) => (
                                                <div key={i} className="bulk-fatal">⚠ {err.source_path}: {err.reason}</div>
                                            ))}
                                        </details>
                                    )}
                                </div>
                            )}
                        </div>
                    ) : activeTab === 'otp_batch' ? (
                        <div className="otp-panel">
                            <h3>邮箱 OTP 批量自动授权</h3>
                            <p className="otp-desc">
                                每行一个，自动识别 provider：
                                <code>@daymniza.dev</code> → usmail.my.id；
                                <code>@*.my.id / .biz.id / .web.id</code> → nissanserena.my.id；
                                带 <code>|TOKEN</code> 或链接 → sorryios.net。
                                <br />
                                行首数字编号、<code>:密码</code>、<code>【账号】.../【验证码查询链接】...</code> 整段都识别（密码不会用，OTP 流程不需要）。
                            </p>

                            <div className="form-group">
                                <label htmlFor="otp-emails">邮箱列表（每行一个）</label>
                                <textarea
                                    id="otp-emails"
                                    className="otp-emails"
                                    value={otpInput}
                                    onChange={e => setOtpInput(e.target.value)}
                                    placeholder={'maryturner@daymniza.dev\njuliemoore@daymniza.dev'}
                                    disabled={otpRunning}
                                    rows={8}
                                    spellCheck={false}
                                />
                            </div>

                            <div className="otp-row-inline">
                                <label htmlFor="otp-timeout">每个邮箱 OTP 超时(秒)</label>
                                <input
                                    id="otp-timeout"
                                    type="number"
                                    min={30}
                                    max={600}
                                    value={otpTimeout}
                                    onChange={e => setOtpTimeout(Math.max(30, Math.min(600, Number(e.target.value) || 180)))}
                                    disabled={otpRunning}
                                />
                            </div>

                            <div className="otp-actions">
                                <button
                                    className="btn btn-primary"
                                    onClick={handleOtpBatch}
                                    disabled={otpRunning || !otpInput.trim()}
                                    type="button"
                                >
                                    {otpRunning ? '授权中…' : '开始批量授权'}
                                </button>
                                {(() => {
                                    const failedCount = otpRows.filter(r => r.status === 'fail').length;
                                    return failedCount > 0 ? (
                                        <button
                                            className="btn btn-secondary"
                                            onClick={handleRetryFailed}
                                            disabled={otpRunning}
                                            type="button"
                                        >
                                            重试失败 ({failedCount})
                                        </button>
                                    ) : null;
                                })()}
                                <button
                                    className="btn btn-ghost"
                                    onClick={handleClose}
                                    disabled={otpRunning}
                                    type="button"
                                >
                                    关闭
                                </button>
                            </div>

                            {error && <div className="error-message" style={{ marginTop: 12 }}>{error}</div>}

                            {otpRows.length > 0 && (
                                <div className="otp-progress">
                                    {otpRows.map((row, i) => (
                                        <div key={i} className={`otp-progress-row ${row.status}`}>
                                            <span className="icon">{statusIcon(row.status)}</span>
                                            <span className="email">
                                                <span className={`provider-badge provider-${row.provider}`}>
                                                    {row.provider}
                                                </span>
                                                {row.email}
                                            </span>
                                            <span className="stage">
                                                {row.status === 'running' && (row.stage || '运行中')}
                                                {row.status === 'pending' && '等待中'}
                                                {row.status === 'ok' && '已添加'}
                                                {row.status === 'fail' && (row.error ? row.error.slice(0, 80) : '失败')}
                                            </span>
                                        </div>
                                    ))}
                                </div>
                            )}

                            {!otpRunning && otpRows.length > 0 && (
                                <div className="otp-progress-summary">
                                    <span className="ok">成功 {otpRows.filter(r => r.status === 'ok').length}</span>
                                    {' / '}
                                    <span className="fail">失败 {otpRows.filter(r => r.status === 'fail').length}</span>
                                    {' / '}
                                    共 {otpRows.length}
                                </div>
                            )}
                        </div>
                    ) : activeTab === 'relay' ? (
                        <div className="relay-panel">
                            <p className="modal-tip" style={{ marginBottom: 12 }}>
                                选预设自动填 base_url，贴 API Key 即可。也支持 <code>codexswitch://</code> deep link。
                            </p>

                            <div className="relay-form-grid">
                            <div className="form-group form-group-full">
                                <label htmlFor="relay-preset">预设</label>
                                <select
                                    id="relay-preset"
                                    value={relayPresetId}
                                    onChange={e => handlePickRelayPreset(e.target.value)}
                                    disabled={relaySubmitting}
                                >
                                    {RELAY_PRESETS.map(p => (
                                        <option key={p.id} value={p.id}>
                                            {p.name}{p.description ? ` — ${p.description}` : ''}
                                        </option>
                                    ))}
                                </select>
                            </div>

                            <div className="form-group">
                                <label htmlFor="relay-name">账号名称 *</label>
                                <input
                                    id="relay-name"
                                    type="text"
                                    value={relayName}
                                    onChange={e => setRelayName(e.target.value)}
                                    disabled={relaySubmitting}
                                    placeholder="例如：unity2-工作"
                                />
                            </div>

                            <div className="form-group">
                                <label htmlFor="relay-base">Base URL *</label>
                                <input
                                    id="relay-base"
                                    type="text"
                                    value={relayBaseUrl}
                                    onChange={e => setRelayBaseUrl(e.target.value)}
                                    disabled={relaySubmitting}
                                    placeholder="https://unity2.ai"
                                    style={{ fontFamily: 'ui-monospace, Menlo, monospace' }}
                                />
                            </div>

                            <div className="form-group">
                                <label htmlFor="relay-key">API Key (sk-... / tp-...) *</label>
                                <input
                                    id="relay-key"
                                    type="password"
                                    value={relayApiKey}
                                    onChange={e => setRelayApiKey(e.target.value)}
                                    disabled={relaySubmitting}
                                    placeholder="sk-... / tp-..."
                                    style={{ fontFamily: 'ui-monospace, Menlo, monospace' }}
                                />
                            </div>

                            <div className="form-group">
                                <label htmlFor="relay-usage">余额查询策略</label>
                                <select
                                    id="relay-usage"
                                    value={relayUsagePreset ?? ''}
                                    onChange={e => setRelayUsagePreset(e.target.value || null)}
                                    disabled={relaySubmitting}
                                >
                                    <option value="">不拉取</option>
                                    <option value="openai_compat">openai_compat (GET /v1/usage)</option>
                                    <option value="glm_zhipu">glm_zhipu (GLM 自家 quota 接口)</option>
                                    <option value="mimo_token_plan">mimo_token_plan (MiMo 控制台 Cookie)</option>
                                </select>
                            </div>

                            {relayUsagePreset === 'mimo_token_plan' && (
                                <div className="form-group form-group-full">
                                    <label htmlFor="relay-usage-cookie">
                                        MiMo 配额 Cookie <span style={{ color: 'var(--text-muted)', fontWeight: 'normal', fontSize: 12 }}>
                                            登录 platform.xiaomimimo.com 后，从 Network 复制 Cookie header
                                        </span>
                                    </label>
                                    <textarea
                                        id="relay-usage-cookie"
                                        value={relayUsageCookie}
                                        onChange={e => setRelayUsageCookie(e.target.value)}
                                        disabled={relaySubmitting}
                                        rows={3}
                                        placeholder="Cookie: api-platform_serviceToken=...; userId=...; api-platform_ph=..."
                                        style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12, width: '100%' }}
                                    />
                                    <p className="modal-tip" style={{ margin: '6px 0 0', fontSize: 12 }}>
                                        这里的 Cookie 只用于查询 Token Plan 用量，不会参与模型请求。实际调用仍使用上面的 tp-key。
                                    </p>
                                </div>
                            )}

                            <div className="form-group">
                                <label htmlFor="relay-protocol">
                                    上游协议 <span style={{ color: 'var(--text-muted)', fontWeight: 'normal', fontSize: 12 }}>
                                        中转站讲什么 wire format
                                    </span>
                                </label>
                                <select
                                    id="relay-protocol"
                                    value={relayProtocol}
                                    onChange={e => setRelayProtocol(e.target.value)}
                                    disabled={relaySubmitting}
                                >
                                    <option value="responses">responses（默认 / Unity2、ChatGPT、OpenAI key）</option>
                                    <option value="chat_completions">chat_completions（GLM/MiMo Coding Plan / 通用 OpenAI Chat）</option>
                                </select>
                            </div>

                            <div className="form-group">
                                <label htmlFor="relay-model-fallback">
                                    模型兜底 <span style={{ color: 'var(--text-muted)', fontWeight: 'normal', fontSize: 12 }}>
                                        客户端发的 model 没命中映射时，统一替换成这个
                                    </span>
                                </label>
                                <input
                                    id="relay-model-fallback"
                                    type="text"
                                    value={relayModelFallback}
                                    onChange={e => setRelayModelFallback(e.target.value)}
                                    disabled={relaySubmitting}
                                    placeholder="如 glm-5.1（留空 = 透传不替换）"
                                    style={{ fontFamily: 'ui-monospace, Menlo, monospace' }}
                                />
                            </div>

                            <div className="form-group form-group-full">
                                <label htmlFor="relay-model-map">
                                    模型映射表 <span style={{ color: 'var(--text-muted)', fontWeight: 'normal', fontSize: 12 }}>
                                        每行 <code>客户端model=中转站model</code>
                                    </span>
                                </label>
                                <textarea
                                    id="relay-model-map"
                                    value={relayModelMapText}
                                    onChange={e => setRelayModelMapText(e.target.value)}
                                    disabled={relaySubmitting}
                                    rows={3}
                                    placeholder={'gpt-5.5=glm-5.1\ngpt-4o=glm-5\ngpt-4o-mini=glm-5.1-x'}
                                    style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12, width: '100%' }}
                                />
                            </div>
                            </div>{/* end relay-form-grid */}

                            {relayError && <div className="error-message">{relayError}</div>}

                            <div className="modal-footer" style={{ padding: '16px 0 0', border: 'none' }}>
                                <button type="button" className="btn btn-ghost" onClick={handleClose} disabled={relaySubmitting}>
                                    取消
                                </button>
                                <button type="button" className="btn btn-primary" onClick={handleSubmitRelay} disabled={relaySubmitting}>
                                    {relaySubmitting ? '导入中…' : '导入中转站'}
                                </button>
                            </div>
                        </div>
                    ) : activeTab === 'google' ? (
                        <div className="oauth-content">
                            <div className="oauth-icon">◆</div>
                            <h3 style={{ marginBottom: '8px', color: 'var(--text-primary)' }}>Google Antigravity OAuth</h3>
                            <p className="oauth-desc">
                                授权后 Gemini 模型通过 Codex Switcher 原生路由；不会修改 Codex 的 OpenAI 登录身份。
                            </p>
                            <button
                                className="btn btn-primary btn-full"
                                style={{ padding: '14px' }}
                                onClick={handleAntigravityLogin}
                                disabled={loading}
                            >
                                {loading ? '处理中...' : '连接 Google 账号'}
                            </button>
                            {!loading && (
                                <button className="btn btn-ghost btn-full" style={{ marginTop: '12px' }} onClick={handleClose}>取消</button>
                            )}
                            {oauthStatus && <div className="oauth-status">{oauthStatus}</div>}
                            {error && <div className="error-message" style={{ marginTop: '16px' }}>{error}</div>}
                        </div>
                    ) : activeTab === 'official' ? (
                        <form onSubmit={handleSubmitOfficial}>
                            <p className="modal-tip">
                                将从本地官方 Codex 的登录状态 (`auth.json`) 中提取认证信息。
                            </p>

                            <div className="form-group">
                                <label htmlFor="name">账号名称 *</label>
                                <input
                                    id="name"
                                    type="text"
                                    value={name}
                                    onChange={e => setName(e.target.value)}
                                    placeholder="例如：工作账号、个人账号"
                                    disabled={loading}
                                    autoFocus
                                />
                            </div>

                            <div className="form-group">
                                <label htmlFor="notes">备注</label>
                                <textarea
                                    id="notes"
                                    value={notes}
                                    onChange={e => setNotes(e.target.value)}
                                    placeholder="可选的备注信息..."
                                    disabled={loading}
                                    rows={3}
                                />
                            </div>

                            {error && <div className="error-message">{error}</div>}

                            <div className="modal-footer" style={{ padding: '16px 0 0', border: 'none' }}>
                                <button type="button" className="btn btn-ghost" onClick={handleClose} disabled={loading}>
                                    取消
                                </button>
                                <button type="submit" className="btn btn-primary" disabled={loading}>
                                    {loading ? '导入中...' : '导入当前账号'}
                                </button>
                            </div>
                        </form>
                    ) : (
                        <div className="oauth-content">
                            <div className="oauth-icon">🛡️</div>
                            <h3 style={{ marginBottom: '8px', color: 'var(--text-primary)' }}>官方 OAuth 授权</h3>
                            <p className="oauth-desc">
                                直接通过 OpenAI 官方渠道登录。支持令牌自动续期，多账号切换更稳定，无需再手动更新 `auth.json`。
                            </p>

                            <button
                                className="btn btn-primary btn-full"
                                style={{ padding: '14px' }}
                                onClick={handleOpenAILogin}
                                disabled={loading}
                            >
                                {loading && oauthStatus ? '处理中...' : '立即登录 OpenAI'}
                            </button>

                            <button
                                className="btn btn-ghost btn-full"
                                style={{ marginTop: '8px' }}
                                onClick={handleCopyOAuthLink}
                                disabled={loading}
                                type="button"
                                title="不打开默认浏览器，把授权链接复制到剪贴板，由你粘贴到目标浏览器"
                            >
                                复制授权链接（指定浏览器登录）
                            </button>

                            {!loading && (
                                <button
                                    className="btn btn-ghost btn-full"
                                    style={{ marginTop: '12px' }}
                                    onClick={handleClose}
                                >
                                    取消
                                </button>
                            )}

                            {oauthStatus && <div className="oauth-status">{oauthStatus}</div>}
                            {error && <div className="error-message" style={{ marginTop: '16px' }}>{error}</div>}

                            <div style={{ marginTop: '16px', fontSize: '12px', color: 'var(--text-tertiary)', textAlign: 'center' }}>
                                授权将在你系统的默认浏览器中完成，安全可信。
                            </div>

                            {!showPasteInput ? (
                                <button
                                    className="btn btn-ghost btn-full"
                                    style={{ marginTop: '12px', fontSize: '12px' }}
                                    onClick={() => setShowPasteInput(true)}
                                    type="button"
                                >
                                    浏览器没跳回来？手动粘贴回调链接
                                </button>
                            ) : (
                                <div style={{ marginTop: '12px', textAlign: 'left' }}>
                                    <div style={{ fontSize: '12px', color: 'var(--text-secondary)', marginBottom: '6px' }}>
                                        从浏览器地址栏复制完整 URL（包含 <code>?code=...&state=...</code>）粘贴到下方：
                                    </div>
                                    <textarea
                                        className="text-input"
                                        style={{ width: '100%', minHeight: '64px', fontFamily: 'monospace', fontSize: '12px' }}
                                        value={callbackInput}
                                        onChange={e => setCallbackInput(e.target.value)}
                                        placeholder="http://localhost:1455/auth/callback?code=...&state=..."
                                        disabled={submittingCallback}
                                    />
                                    <div style={{ display: 'flex', gap: '8px', marginTop: '8px' }}>
                                        <button
                                            className="btn btn-primary"
                                            style={{ flex: 1 }}
                                            onClick={handleSubmitCallback}
                                            disabled={submittingCallback || !callbackInput.trim()}
                                            type="button"
                                        >
                                            {submittingCallback ? '提交中...' : '开始授权'}
                                        </button>
                                        <button
                                            className="btn btn-ghost"
                                            onClick={() => { setShowPasteInput(false); setCallbackInput(''); }}
                                            disabled={submittingCallback}
                                            type="button"
                                        >
                                            取消
                                        </button>
                                    </div>
                                </div>
                            )}
                        </div>
                    )}
                </div>
            </div>
        </div>
    );
}
