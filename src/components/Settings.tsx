import { useState, useEffect, useMemo } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Palette, Server, Monitor, Wrench, Save, Github, Radio, Smartphone, Search, X } from 'lucide-react';
import { Account, effectiveKind } from '../hooks/useAccounts';
import './Settings.css';

interface AppSettings {
    auto_reload_ide: boolean;
    primary_ide: string;
    use_pkill_restart: boolean;
    background_refresh: boolean;
    refresh_interval_minutes: number;
    inactive_refresh_days: number;
    theme_palette: string;
    allow_auto_switch_to_free: boolean;
    proxy_enabled: boolean;
    proxy_port: number;
    proxy_allow_lan: boolean;
    switch_mode: string;
    remote_mode: string;
    remote_server_port: number;
    remote_server_bind: string;
    remote_server_url: string;
    remote_server_url_fallback: string;
    remote_shared_secret: string;
    solo_auto_sync_current: boolean;
    proxy_bootstrap_byte_cap: number;
    proxy_bootstrap_time_cap_ms: number;
    relay_auto_switch_out: boolean;
    relay_auto_switch_in: boolean;
    client_direct_upstream: boolean;
    client_owns_current: boolean;
}

interface RemoteHealth {
    mode: string;
    version: string;
    account_count: number;
}

const IDE_OPTIONS = [
    { value: 'Windsurf', label: 'Windsurf' },
    { value: 'Antigravity', label: 'Antigravity' },
    { value: 'Cursor', label: 'Cursor' },
    { value: 'VSCode', label: 'VS Code' },
    { value: 'Codex', label: 'Codex App' },
];

interface SettingsProps {
    accounts?: Account[];
    onSetSessionAnchor?: (id: string, enabled: boolean) => Promise<void>;
}

export function Settings({ accounts = [], onSetSessionAnchor }: SettingsProps = {}) {
    const [settings, setSettings] = useState<AppSettings>({
        auto_reload_ide: false,
        primary_ide: 'Windsurf',
        use_pkill_restart: false,
        background_refresh: false,
        refresh_interval_minutes: 30,
        inactive_refresh_days: 7,
        theme_palette: 'midnight',
        allow_auto_switch_to_free: false,
        proxy_enabled: false,
        proxy_port: 18080,
        proxy_allow_lan: false,
        switch_mode: 'auto',
        remote_mode: 'off',
        remote_server_port: 18081,
        remote_server_bind: '0.0.0.0',
        remote_server_url: '',
        remote_server_url_fallback: '',
        remote_shared_secret: '',
        solo_auto_sync_current: true,
        proxy_bootstrap_byte_cap: 32 * 1024,
        proxy_bootstrap_time_cap_ms: 8000,
        relay_auto_switch_out: true,
        relay_auto_switch_in: false,
        client_direct_upstream: false,
        client_owns_current: false,
    });
    const [saving, setSaving] = useState(false);
    const [repairing, setRepairing] = useState(false);
    const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);
    const [remoteBusy, setRemoteBusy] = useState(false);
    const [remoteStatus, setRemoteStatus] = useState<string>('');
    const [showAnchorPicker, setShowAnchorPicker] = useState(false);
    const [anchorSearch, setAnchorSearch] = useState('');
    const [anchorBusy, setAnchorBusy] = useState(false);

    // 手机锚只对 ChatGPT 订阅号有效：Codex.app `/codex/remote/control/*`
    // 必须用 chatgpt_account_id 鉴权；Relay / OpenAI API key 没有这个 claim。
    const anchorAccount = useMemo(
        () => accounts.find(a => a.is_session_anchor) || null,
        [accounts]
    );
    const anchorCandidates = useMemo(
        () => accounts.filter(a => effectiveKind(a) === 'chatgpt_oauth'),
        [accounts]
    );
    const filteredAnchorCandidates = useMemo(() => {
        const q = anchorSearch.trim().toLowerCase();
        if (!q) return anchorCandidates;
        return anchorCandidates.filter(a => {
            const plan = (a.cached_quota?.plan_type || '').toLowerCase();
            return a.name.toLowerCase().includes(q) || plan.includes(q);
        });
    }, [anchorCandidates, anchorSearch]);

    useEffect(() => {
        loadSettings();
    }, []);

    const loadSettings = async () => {
        try {
            const data = await invoke<AppSettings>('get_settings');
            setSettings(data);
        } catch (e) {
            console.error('加载设置失败:', e);
        }
    };

    const saveSettings = async () => {
        setSaving(true);
        setMessage(null);
        try {
            await invoke('update_settings', { settings });
            setMessage({ type: 'success', text: '✅ 设置已保存' });
            setTimeout(() => setMessage(null), 3000);
        } catch (e) {
            setMessage({ type: 'error', text: `❌ 保存失败: ${e}` });
        } finally {
            setSaving(false);
        }
    };

    const updateField = <K extends keyof AppSettings>(key: K, value: AppSettings[K]) => {
        setSettings(prev => ({ ...prev, [key]: value }));
    };

    const withRemote = async (label: string, fn: () => Promise<string>) => {
        setRemoteBusy(true);
        setRemoteStatus('');
        setMessage(null);
        try {
            const text = await fn();
            setRemoteStatus(`✅ ${label}：${text}`);
            setMessage({ type: 'success', text: `${label} 成功` });
        } catch (e) {
            setRemoteStatus(`❌ ${label} 失败：${e}`);
            setMessage({ type: 'error', text: `${label} 失败：${e}` });
        } finally {
            setRemoteBusy(false);
        }
    };

    const handleGenerateSecret = async () => {
        try {
            const s = await invoke<string>('remote_generate_secret');
            updateField('remote_shared_secret', s);
            setMessage({ type: 'success', text: '已生成新密钥，记得保存设置' });
        } catch (e) {
            setMessage({ type: 'error', text: `生成失败：${e}` });
        }
    };

    const handleSoloSyncNow = () =>
        withRemote('立即同号', async () => {
            const switched = await invoke<string | null>('solo_sync_current');
            return switched ? `已切换到 ${switched}` : '已与 Server 一致，无需切换';
        });

    const handleRemoteTest = () =>
        withRemote('测试连接', async () => {
            const [url, h] = await invoke<[string, RemoteHealth]>('remote_probe');
            return `使用 ${url}，Server v${h.version}，远端账号数 ${h.account_count}`;
        });

    const handleRemotePushAll = () =>
        withRemote('推送全部账号到 Server', async () => {
            const n = await invoke<number>('remote_push_all');
            return `已上传 ${n} 个账号`;
        });

    const handleRemotePullAll = () =>
        withRemote('从 Server 拉取全部账号', async () => {
            const n = await invoke<number>('remote_pull_all');
            return `已合并 ${n} 个账号`;
        });

    const handleRemotePullAllTokens = () =>
        withRemote('从 Server 同步所有 token', async () => {
            const r = await invoke<{
                pulled: number;
                refreshed: number;
                current: string | null;
                current_name: string | null;
                wrote_auth_json: boolean;
                errors: [string, string][];
            }>('remote_pull_all_tokens');
            const parts = [
                `账号 ${r.pulled}`,
                `token ${r.refreshed}`,
            ];
            if (r.current_name) parts.push(`current=${r.current_name}`);
            if (r.wrote_auth_json) parts.push('已写 auth.json');
            if (r.errors.length > 0) parts.push(`错误 ${r.errors.length}`);
            return parts.join(' · ');
        });

    const handleRemoteRestart = () =>
        withRemote('重启 HTTP 服务', async () => {
            const s = await invoke<string>('remote_restart_server');
            return s;
        });

    const handleBindAnchor = async (id: string) => {
        if (!onSetSessionAnchor) return;
        setAnchorBusy(true);
        setMessage(null);
        try {
            await onSetSessionAnchor(id, true);
            setMessage({ type: 'success', text: '✅ 已设为手机锚' });
            setTimeout(() => setMessage(null), 3000);
            setShowAnchorPicker(false);
            setAnchorSearch('');
        } catch (e) {
            setMessage({ type: 'error', text: `❌ 绑定失败：${e}` });
        } finally {
            setAnchorBusy(false);
        }
    };

    const handleUnbindAnchor = async () => {
        if (!onSetSessionAnchor || !anchorAccount) return;
        setAnchorBusy(true);
        setMessage(null);
        try {
            await onSetSessionAnchor(anchorAccount.id, false);
            setMessage({ type: 'success', text: '✅ 已解除手机锚绑定' });
            setTimeout(() => setMessage(null), 3000);
        } catch (e) {
            setMessage({ type: 'error', text: `❌ 解除失败：${e}` });
        } finally {
            setAnchorBusy(false);
        }
    };

    const handleRepair = async () => {
        if (!confirm('这将尝试移除 Codex App 的安全隔离属性。\n\n系统可能会弹窗要求输入密码以获得权限。是否继续？')) {
            return;
        }

        setRepairing(true);
        setMessage(null);
        try {
            const ticket = await invoke<string>('request_quarantine_fix_ticket');
            await invoke('fix_codex_quarantine', { ticket });
            alert('✅ 修复成功！\n\n现在请尝试重新打开 Codex App。');
        } catch (e) {
            alert(`❌ 修复失败: ${e}`);
        } finally {
            setRepairing(false);
        }
    };


    return (
        <div className="settings-page">
            <div className="settings-header">
                <h2>设置</h2>
                <button
                    className="save-button"
                    onClick={saveSettings}
                    disabled={saving}
                >
                    <Save size={14} />
                    {saving ? '保存中...' : '保存设置'}
                </button>
            </div>

            {message && (
                <div className={`settings-message ${message.type}`}>
                    {message.text}
                </div>
            )}

            <div className="settings-section">
                <h3><Palette size={16} /> 界面外观</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">界面配色</span>
                        <span className="setting-desc">主界面颜色的色调风格</span>
                    </div>
                    <select
                        className="select-input"
                        value={settings.theme_palette}
                        onChange={e => updateField('theme_palette', e.target.value)}
                    >
                        <option value="midnight">暗黑护眼</option>
                        <option value="github">经典蓝</option>
                        <option value="agate">玛瑙绿</option>
                    </select>
                </div>
            </div>

            <div className="settings-section">
                <h3><Server size={16} /> 后台服务</h3>

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">后台保活与同步</span>
                        <span className="setting-desc">
                            {settings.remote_mode === 'client'
                                ? 'client 模式：保活由 Server 负责，本机已强制关闭（避免双路刷新撞飞 refresh_token）'
                                : '当前账号只做权威回流；非活跃账号按独占策略保活刷新（保存后生效）'}
                        </span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings.remote_mode === 'client' ? false : settings.background_refresh}
                            disabled={settings.remote_mode === 'client'}
                            onChange={e => updateField('background_refresh', e.target.checked)}
                        />
                        <span className="toggle-slider"></span>
                        <span className={`toggle-text ${settings.background_refresh && settings.remote_mode !== 'client' ? 'on' : ''}`}>
                            {settings.remote_mode === 'client'
                                ? 'Server 负责'
                                : settings.background_refresh ? '已开启' : '已关闭'}
                        </span>
                    </label>
                </div>

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">智能切号允许 FREE 账号</span>
                        <span className="setting-desc">点击“切换下一个账号”时，是否允许自动寻找并切到 FREE 账号（默认优先付费账号）</span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings.allow_auto_switch_to_free}
                            onChange={e => updateField('allow_auto_switch_to_free', e.target.checked)}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">中转 / Plan / 三方 出问题时切回订阅号</span>
                        <span className="setting-desc">
                            开启（默认）：current 是中转 / Coding Plan / 三方 API 时，遇到 401/429/quota 自动切到健康的订阅号，避免请求卡死。关闭后这类号出错会把错误透传给客户端，不偷换。
                        </span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings.relay_auto_switch_out ?? true}
                            onChange={e => updateField('relay_auto_switch_out', e.target.checked)}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">自动选号可挑中 中转 / Plan / 三方</span>
                        <span className="setting-desc">
                            关闭（默认）：用订阅号时自动切号 / affinity 不会路由到中转 / Coding Plan / 三方 API，避免偷扣余额。开启后这类号跟订阅号同等参与轮询（量大但要确认你愿意花对应套餐的额度）。
                        </span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings.relay_auto_switch_in ?? false}
                            onChange={e => updateField('relay_auto_switch_in', e.target.checked)}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>

                {settings.remote_mode === 'client' && (
                    <div className="setting-item">
                        <div className="setting-info">
                            <span className="setting-label">HTTP 也走本机直连上游</span>
                            <span className="setting-desc">
                                开启后 HTTP 也跟 WebSocket 同路，本机直接打上游，不再经 Server 转发；access_token 仍从 Server 拉。适合 Server 出口不稳但本机出口稳的场景。
                            </span>
                        </div>
                        <label className="toggle">
                            <input
                                type="checkbox"
                                checked={settings.client_direct_upstream ?? false}
                                onChange={e => updateField('client_direct_upstream', e.target.checked)}
                            />
                            <span className="toggle-slider"></span>
                        </label>
                    </div>
                )}

                {settings.remote_mode === 'client' && (
                    <div className="setting-item">
                        <div className="setting-info">
                            <span className="setting-label">本机管 current 指针</span>
                            <span className="setting-desc">
                                开启后本机自己决定 current（不被 Server `/current` 反向同步覆盖），`~/.codex/auth.json` 也由本机自己写。等价于旧 solo 模式（已并入 client）。
                            </span>
                        </div>
                        <label className="toggle">
                            <input
                                type="checkbox"
                                checked={settings.client_owns_current ?? false}
                                onChange={e => updateField('client_owns_current', e.target.checked)}
                            />
                            <span className="toggle-slider"></span>
                        </label>
                    </div>
                )}

                {
                    settings.background_refresh && settings.remote_mode !== 'client' && (
                        <>
                            <div className="setting-item sub-item">
                                <div className="setting-info">
                                    <span className="setting-label">调度间隔（分钟）</span>
                                </div>
                                <input
                                    type="number"
                                    className="number-input"
                                    min={5}
                                    max={120}
                                    value={settings.refresh_interval_minutes}
                                    onChange={e => updateField('refresh_interval_minutes', parseInt(e.target.value) || 30)}
                                />
                            </div>
                            <div className="setting-item sub-item">
                                <div className="setting-info">
                                    <span className="setting-label">非活跃保活阈值（天）</span>
                                    <span className="setting-desc">当账号 last_refresh 超过该阈值时，调度器才会尝试保活刷新</span>
                                </div>
                                <input
                                    type="number"
                                    className="number-input"
                                    min={1}
                                    max={30}
                                    value={settings.inactive_refresh_days}
                                    onChange={e => updateField('inactive_refresh_days', parseInt(e.target.value) || 7)}
                                />
                            </div>
                        </>
                    )
                }
            </div >

            <div className="settings-section">
                <h3><Monitor size={16} /> IDE 重载</h3>

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">自动重载 IDE</span>
                        <span className="setting-desc">切换账号后自动重载 IDE 以应用新的 Token</span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings.auto_reload_ide}
                            onChange={e => updateField('auto_reload_ide', e.target.checked)}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>

                {settings.auto_reload_ide && (
                    <>
                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">主力 IDE</span>
                                <span className="setting-desc">仅重载选中的 IDE</span>
                            </div>
                            <select
                                className="select-input"
                                value={settings.primary_ide}
                                onChange={e => updateField('primary_ide', e.target.value)}
                            >
                                {IDE_OPTIONS.map(opt => (
                                    <option key={opt.value} value={opt.value}>{opt.label}</option>
                                ))}
                            </select>
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">使用杀进程重启</span>
                                <span className="setting-desc">使用 pkill 方式重启（Windsurf 推荐，无需权限）</span>
                            </div>
                            <label className="toggle">
                                <input
                                    type="checkbox"
                                    checked={settings.use_pkill_restart}
                                    onChange={e => updateField('use_pkill_restart', e.target.checked)}
                                />
                                <span className="toggle-slider"></span>
                            </label>
                        </div>
                    </>
                )}
            </div>

            <div className="settings-section">
                <h3><Radio size={16} /> Remote Mode（局域网同步）</h3>

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">工作模式</span>
                        <span className="setting-desc">
                            off=独立；server=Server 侧提供 API；client=本机，rt 旋转走 Server。
                            老 solo 模式已合并到 client（自动迁移：solo → client + 本机直连上游 + 本机管 current）。
                        </span>
                    </div>
                    <select
                        className="select-input"
                        value={settings.remote_mode}
                        onChange={e => updateField('remote_mode', e.target.value)}
                    >
                        <option value="off">off（关闭）</option>
                        <option value="server">server（Server 侧）</option>
                        <option value="client">client（本机）</option>
                    </select>
                </div>

                {settings.remote_mode === 'server' && (
                    <>
                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">监听端口</span>
                                <span className="setting-desc">Server 侧 HTTP API 端口（默认 18081）</span>
                            </div>
                            <input
                                type="number"
                                className="number-input"
                                min={1024}
                                max={65535}
                                value={settings.remote_server_port}
                                onChange={e => updateField('remote_server_port', parseInt(e.target.value) || 18081)}
                            />
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">绑定地址</span>
                                <span className="setting-desc">0.0.0.0 监听所有网卡；建议仅暴露给 ZeroTier 网段</span>
                            </div>
                            <input
                                type="text"
                                className="text-input"
                                value={settings.remote_server_bind}
                                onChange={e => updateField('remote_server_bind', e.target.value)}
                                placeholder="0.0.0.0"
                            />
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">共享密钥</span>
                                <span className="setting-desc">客户端访问需携带 X-Auth-Token；留空则拒绝所有请求</span>
                            </div>
                            <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
                                <input
                                    type="text"
                                    className="text-input"
                                    style={{ minWidth: 260, fontFamily: 'monospace', fontSize: 12 }}
                                    value={settings.remote_shared_secret}
                                    onChange={e => updateField('remote_shared_secret', e.target.value)}
                                    placeholder="（未设置）"
                                />
                                <button
                                    className="action-button"
                                    onClick={handleGenerateSecret}
                                    disabled={remoteBusy}
                                >
                                    生成
                                </button>
                            </div>
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">重启 HTTP 服务</span>
                                <span className="setting-desc">修改端口/绑定/密钥后点此应用（保存设置后）</span>
                            </div>
                            <button
                                className="action-button"
                                onClick={handleRemoteRestart}
                                disabled={remoteBusy}
                            >
                                {remoteBusy ? '执行中...' : '立即重启'}
                            </button>
                        </div>
                    </>
                )}

                {(settings.remote_mode === 'client' || settings.remote_mode === 'solo') && (
                    <>
                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">Server API 地址（主）</span>
                                <span className="setting-desc">优先尝试，建议填局域网 IP，如 http://192.168.2.14:18081</span>
                            </div>
                            <input
                                type="text"
                                className="text-input"
                                style={{ minWidth: 260 }}
                                value={settings.remote_server_url}
                                onChange={e => updateField('remote_server_url', e.target.value)}
                                placeholder="http://192.168.2.14:18081"
                            />
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">Server API 地址（回退）</span>
                                <span className="setting-desc">主不通时自动切换，建议填 ZeroTier IP，如 http://172.26.96.198:18081</span>
                            </div>
                            <input
                                type="text"
                                className="text-input"
                                style={{ minWidth: 260 }}
                                value={settings.remote_server_url_fallback}
                                onChange={e => updateField('remote_server_url_fallback', e.target.value)}
                                placeholder="http://172.26.96.198:18081"
                            />
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">共享密钥</span>
                                <span className="setting-desc">必须与 Server 端一致</span>
                            </div>
                            <input
                                type="text"
                                className="text-input"
                                style={{ minWidth: 260, fontFamily: 'monospace', fontSize: 12 }}
                                value={settings.remote_shared_secret}
                                onChange={e => updateField('remote_shared_secret', e.target.value)}
                                placeholder="（未设置）"
                            />
                        </div>

                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">同步操作</span>
                                <span className="setting-desc">测试连通性 / 推送本机账号 / 从 Server 合并</span>
                            </div>
                            <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
                                <button className="action-button" onClick={handleRemoteTest} disabled={remoteBusy}>
                                    测试连接
                                </button>
                                <button className="action-button" onClick={handleRemotePushAll} disabled={remoteBusy}>
                                    推送全部
                                </button>
                                <button className="action-button" onClick={handleRemotePullAll} disabled={remoteBusy}>
                                    拉取合并
                                </button>
                                <button className="action-button" onClick={handleRemotePullAllTokens} disabled={remoteBusy}>
                                    同步所有 token
                                </button>
                            </div>
                        </div>

                        {settings.remote_mode === 'solo' && (
                            <>
                                <div className="setting-item sub-item">
                                    <div className="setting-info">
                                        <span className="setting-label">自动同号</span>
                                        <span className="setting-desc">
                                            心跳时自动把本机 current 对齐到 Server 的 current。
                                            Server 不可达会静默跳过，保持本机现状。
                                        </span>
                                    </div>
                                    <label className="toggle">
                                        <input
                                            type="checkbox"
                                            checked={settings.solo_auto_sync_current}
                                            onChange={e => updateField('solo_auto_sync_current', e.target.checked)}
                                        />
                                        <span className="toggle-slider"></span>
                                    </label>
                                </div>

                                <div className="setting-item sub-item">
                                    <div className="setting-info">
                                        <span className="setting-label">立即同号</span>
                                        <span className="setting-desc">手工拉取 Server 当前账号并热切过去（自动同号关了也可用）</span>
                                    </div>
                                    <button
                                        className="action-button"
                                        onClick={handleSoloSyncNow}
                                        disabled={remoteBusy}
                                    >
                                        {remoteBusy ? '执行中...' : '立即同号'}
                                    </button>
                                </div>
                            </>
                        )}
                    </>
                )}

                {remoteStatus && (
                    <div className="setting-item sub-item">
                        <span className="setting-desc" style={{ fontFamily: 'monospace', fontSize: 12 }}>
                            {remoteStatus}
                        </span>
                    </div>
                )}
            </div>

            {onSetSessionAnchor && (
                <div className="settings-section">
                    <h3><Smartphone size={16} /> Codex.app 手机锚绑定</h3>
                    <div className="setting-item">
                        <div className="setting-info">
                            <span className="setting-label">当前绑定账号</span>
                            <span className="setting-desc">
                                磁盘 ~/.codex/auth.json 永远跟随此号，Codex.app 手机远程连接绑定此号；
                                切到其他号时 disk 不动、proxy 出口照切（手机 bridge 不掉线）。
                                仅 ChatGPT 订阅号可绑定。
                            </span>
                        </div>
                        <div className="anchor-actions">
                            <span className={`anchor-current ${anchorAccount ? 'bound' : 'unbound'}`}>
                                {anchorAccount ? (
                                    <>
                                        📱 {anchorAccount.name}
                                        <span className={`anchor-current-plan plan-${(anchorAccount.cached_quota?.plan_type || 'unknown').toLowerCase()}`}>
                                            {anchorAccount.cached_quota?.plan_type
                                                ? anchorAccount.cached_quota.plan_type.toUpperCase()
                                                : '未知'}
                                        </span>
                                    </>
                                ) : '未绑定'}
                            </span>
                            <button
                                className="action-button"
                                onClick={() => { setAnchorSearch(''); setShowAnchorPicker(true); }}
                                disabled={anchorBusy}
                            >
                                {anchorAccount ? '更换' : '选择绑定账号'}
                            </button>
                            {anchorAccount && (
                                <button
                                    className="action-button warning"
                                    onClick={handleUnbindAnchor}
                                    disabled={anchorBusy}
                                >
                                    解除
                                </button>
                            )}
                        </div>
                    </div>
                </div>
            )}

            <div className="settings-section danger">
                <h3><Wrench size={16} /> 故障修复</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">修复 Codex App 闪退</span>
                        <span className="setting-desc">移除 macOS 安全隔离属性 (需要管理员权限)</span>
                    </div>
                    <button
                        className="action-button warning"
                        onClick={handleRepair}
                        disabled={repairing}
                    >
                        {repairing ? '修复中...' : '立即修复'}
                    </button>
                </div>
            </div>

            {showAnchorPicker && (
                <div className="modal-overlay" onClick={() => !anchorBusy && setShowAnchorPicker(false)}>
                    <div className="anchor-picker" onClick={e => e.stopPropagation()}>
                        <div className="anchor-picker-header">
                            <div>
                                <h3>选择手机锚账号</h3>
                                <p className="anchor-picker-hint">
                                    仅 ChatGPT 订阅号可绑定（Codex.app 远程连接需要 chatgpt_account_id 鉴权）
                                </p>
                            </div>
                            <button
                                className="anchor-picker-close"
                                onClick={() => setShowAnchorPicker(false)}
                                disabled={anchorBusy}
                                title="关闭"
                            >
                                <X size={16} />
                            </button>
                        </div>
                        <div className="anchor-picker-search">
                            <Search size={14} />
                            <input
                                type="text"
                                placeholder="搜索邮箱 / 订阅类型 (如 team / pro / free)…"
                                value={anchorSearch}
                                onChange={e => setAnchorSearch(e.target.value)}
                                autoFocus
                            />
                        </div>
                        <div className="anchor-picker-list">
                            {filteredAnchorCandidates.length === 0 ? (
                                <div className="anchor-picker-empty">
                                    {anchorCandidates.length === 0
                                        ? '当前没有 ChatGPT 订阅号'
                                        : '没有匹配的账号'}
                                </div>
                            ) : (
                                filteredAnchorCandidates.map(acc => {
                                    const plan = acc.cached_quota?.plan_type;
                                    const planLabel = plan ? plan.toUpperCase() : '未知';
                                    const planClass = (plan || 'unknown').toLowerCase();
                                    return (
                                        <button
                                            key={acc.id}
                                            className={`anchor-picker-item ${acc.is_session_anchor ? 'current' : ''}`}
                                            onClick={() => handleBindAnchor(acc.id)}
                                            disabled={anchorBusy || acc.is_session_anchor}
                                        >
                                            <span className="anchor-picker-name">{acc.name}</span>
                                            <span className={`anchor-picker-plan plan-${planClass}`}>{planLabel}</span>
                                            {acc.is_session_anchor && (
                                                <span className="anchor-picker-tag">✓ 当前绑定</span>
                                            )}
                                        </button>
                                    );
                                })
                            )}
                        </div>
                    </div>
                </div>
            )}

            <div className="settings-section">
                <h3><Github size={16} /> 关于</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">Codex Switcher</span>
                        <span className="setting-desc">多账号智能切换 + 本地代理 + 用量统计</span>
                    </div>
                    <a
                        className="action-button github-link"
                        href="https://github.com/xtftbwvfp/codex-switcher"
                        target="_blank"
                        rel="noopener noreferrer"
                    >
                        <Github size={14} /> GitHub
                    </a>
                </div>
            </div>
        </div >
    );
}
