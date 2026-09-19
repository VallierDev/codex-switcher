import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Copy, Check, Save } from 'lucide-react';
import './Proxy.css';
import { isMacOS } from '../platform';

interface ProxyStatus {
    enabled: boolean;
    port: number;
    is_running: boolean;
    base_url: string;
    allow_lan: boolean;
    lan_base_url?: string | null;
    total_requests: number;
    auto_switches: number;
}

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
    proxy_threshold_5h: number;
    proxy_threshold_weekly: number;
    proxy_free_guard: number;
    notify_on_switch: boolean;
    inject_switch_message: boolean;
    quota_refresh_enabled: boolean;
    quota_refresh_interval: number;
    quota_refresh_batch: number;
    switch_mode: string;
    remote_mode: string;
    proxy_bootstrap_byte_cap: number;
    proxy_bootstrap_time_cap_ms: number;
}

export function Proxy() {
    const [status, setStatus] = useState<ProxyStatus | null>(null);
    const [settings, setSettings] = useState<AppSettings | null>(null);
    const [port, setPort] = useState(18080);
    const [copied, setCopied] = useState(false);
    const [saving, setSaving] = useState(false);
    const [envWriting, setEnvWriting] = useState(false);
    const [killing, setKilling] = useState(false);
    const [message, setMessage] = useState<{ type: 'success' | 'error'; text: string } | null>(null);
    const [switchedAccount, setSwitchedAccount] = useState<string | null>(null);
    const [fastMode, setFastMode] = useState(false);
    const [goalsMode, setGoalsMode] = useState(false);

    const fetchAll = async () => {
        try {
            const [s, st, fm, gm] = await Promise.all([
                invoke<AppSettings>('get_settings'),
                invoke<ProxyStatus>('get_proxy_status'),
                invoke<boolean>('get_codex_fast_mode'),
                invoke<boolean>('get_codex_features_goals'),
            ]);
            setSettings(s);
            setFastMode(fm);
            setGoalsMode(gm);
            setStatus(st);
            setPort(s.proxy_port);
        } catch (e) {
            console.error('加载代理状态失败:', e);
        }
    };

    useEffect(() => {
        fetchAll();
        const unsub1 = listen('settings-updated', fetchAll);
        const unsub2 = listen<string>('proxy-account-switched', (e) => {
            setSwitchedAccount(e.payload);
            setTimeout(() => setSwitchedAccount(null), 5000);
            fetchAll();
        });
        const unsub3 = listen<string>('proxy-all-exhausted', (e) => {
            setMessage({ type: 'error', text: e.payload });
        });
        return () => {
            unsub1.then(fn => fn());
            unsub2.then(fn => fn());
            unsub3.then(fn => fn());
        };
    }, []);

    const toggleProxy = async (enabled: boolean) => {
        if (!settings) return;
        setSaving(true);
        setMessage(null);
        try {
            await invoke('update_settings', {
                settings: { ...settings, proxy_enabled: enabled, proxy_port: port },
            });
            setMessage({ type: 'success', text: enabled ? '代理已启动' : '代理已停止' });
            setTimeout(() => setMessage(null), 3000);
        } catch (e) {
            setMessage({ type: 'error', text: `操作失败: ${e}` });
        } finally {
            setSaving(false);
        }
    };

    const savePort = async () => {
        if (!settings) return;
        setSaving(true);
        try {
            await invoke('update_settings', {
                settings: { ...settings, proxy_port: port },
            });
            setMessage({ type: 'success', text: '端口已更新（重启代理后生效）' });
            setTimeout(() => setMessage(null), 3000);
        } catch (e) {
            setMessage({ type: 'error', text: `保存失败: ${e}` });
        } finally {
            setSaving(false);
        }
    };

    const handleSetEnv = async (enable: boolean) => {
        setEnvWriting(true);
        setMessage(null);
        try {
            const result = await invoke<string>('set_proxy_env', { port, enable });
            setMessage({ type: 'success', text: result + '（新终端窗口生效）' });
            setTimeout(() => setMessage(null), 5000);
        } catch (e) {
            setMessage({ type: 'error', text: `${e}` });
        } finally {
            setEnvWriting(false);
        }
    };

    const handleKill = async () => {
        setKilling(true);
        try {
            const result = await invoke<string>('kill_codex_processes');
            setMessage({ type: 'success', text: result });
            setTimeout(() => setMessage(null), 3000);
        } catch (e) {
            setMessage({ type: 'error', text: `${e}` });
        } finally {
            setKilling(false);
        }
    };

    const isRunning = status?.is_running ?? false;
    const isEnabled = settings?.proxy_enabled ?? false;

    return (
        <div className="proxy-page">
            <div className="proxy-header">
                <h2>代理服务</h2>
                <div className={`proxy-status-badge ${isRunning ? 'running' : 'stopped'}`}>
                    <span className="status-dot" />
                    {isRunning ? '运行中' : '已停止'}
                </div>
            </div>

            {message && (
                <div className={`settings-message ${message.type}`}>
                    {message.text}
                </div>
            )}

            {switchedAccount && (
                <div className="settings-message success">
                    代理已自动切换到账号: {switchedAccount}
                </div>
            )}

            {/* 代理开关 */}
            <div className="settings-section">
                <h3>代理控制</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">本地代理服务器</span>
                        <span className="setting-desc">
                            Codex CLI 通过代理连接 OpenAI，支持无中断自动切号和 429 智能重试
                        </span>
                    </div>
                    <button
                        className={`proxy-toggle-btn ${isEnabled ? 'on' : 'off'}`}
                        onClick={() => toggleProxy(!isEnabled)}
                        disabled={saving}
                    >
                        {saving ? '...' : isEnabled ? '关闭代理' : '启动代理'}
                    </button>
                </div>

                <div className="setting-item sub-item">
                    <div className="setting-info">
                        <span className="setting-label">代理端口</span>
                    </div>
                    <div className="port-input-group">
                        <input
                            type="number"
                            className="number-input"
                            min={1024}
                            max={65535}
                            value={port}
                            onChange={e => setPort(parseInt(e.target.value) || 18080)}
                        />
                        {port !== settings?.proxy_port && (
                            <button className="btn btn-sm btn-primary" onClick={savePort} disabled={saving}>
                                <Save size={12} /> 保存
                            </button>
                        )}
                    </div>
                </div>

                <div className="setting-item sub-item">
                    <div className="setting-info">
                        <span className="setting-label">允许局域网访问</span>
                        <span className="setting-desc">开启后监听 `0.0.0.0`，同一局域网内的 Windows 可直接连接这台机器的代理</span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings?.proxy_allow_lan ?? false}
                            onChange={async e => {
                                if (!settings) return;
                                const updated = { ...settings, proxy_allow_lan: e.target.checked };
                                setSettings(updated);
                                await invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>

                <div className="setting-item sub-item">
                    <div className="setting-info">
                        <span className="setting-label">切号模式</span>
                        <span className="setting-desc">
                            auto：代理开启时自动走热切（不写 auth.json，不杀进程，仅刷 store.current）。
                            代理关闭时回退到冷切。cold：无论如何都冷切。
                        </span>
                    </div>
                    <select
                        className="select-input"
                        value={settings?.switch_mode ?? 'auto'}
                        onChange={async e => {
                            if (!settings) return;
                            const updated = { ...settings, switch_mode: e.target.value };
                            setSettings(updated);
                            await invoke('update_settings', { settings: updated });
                        }}
                    >
                        <option value="auto">auto（代理开=热切）</option>
                        <option value="cold">cold（强制冷切）</option>
                    </select>
                </div>
            </div>

            {/* 环境变量配置 */}
            <div className="settings-section">
                <h3>环境变量</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">手动启动</span>
                        <span className="setting-desc">复制命令到终端运行</span>
                    </div>
                    <button
                        className="copy-command-button"
                        onClick={() => {
                            navigator.clipboard.writeText(
                                `OPENAI_BASE_URL=${status?.base_url ?? `http://localhost:${port}/v1`} codex`
                            );
                            setCopied(true);
                            setTimeout(() => setCopied(false), 2000);
                        }}
                    >
                        <code>OPENAI_BASE_URL={status?.base_url ?? `http://localhost:${port}/v1`} codex</code>
                        {copied ? <Check size={12} /> : <Copy size={12} />}
                    </button>
                </div>

                {status?.allow_lan && status.lan_base_url && (
                    <div className="setting-item">
                        <div className="setting-info">
                            <span className="setting-label">局域网客户端</span>
                            <span className="setting-desc">Windows 机器可把 `OPENAI_BASE_URL` 指向下面这个地址</span>
                        </div>
                        <button
                            className="copy-command-button"
                            onClick={() => {
                                navigator.clipboard.writeText(
                                    `OPENAI_BASE_URL=${status.lan_base_url} codex`
                                );
                                setCopied(true);
                                setTimeout(() => setCopied(false), 2000);
                            }}
                        >
                            <code>OPENAI_BASE_URL={status.lan_base_url} codex</code>
                            {copied ? <Check size={12} /> : <Copy size={12} />}
                        </button>
                    </div>
                )}

                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">全局代理（CLI + App 全覆盖）</span>
                        <span className="setting-desc">
                            同时写入 ~/.zshrc、launchctl 和 ~/.codex/config.toml，终端 CLI 和 Codex App 均走代理
                        </span>
                    </div>
                    <div className="env-btn-group">
                        <button
                            className="btn btn-sm btn-primary"
                            onClick={() => handleSetEnv(true)}
                            disabled={envWriting}
                        >
                            {envWriting ? '...' : '写入环境变量'}
                        </button>
                        <button
                            className="btn btn-sm btn-ghost"
                            onClick={() => handleSetEnv(false)}
                            disabled={envWriting}
                        >
                            移除
                        </button>
                    </div>
                </div>
            </div>

            {/* 定时额度刷新 */}
            <div className="settings-section">
                <h3>定时额度刷新</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">自动刷新账号额度</span>
                        <span className="setting-desc">
                            {settings?.remote_mode === 'client'
                                ? 'client 模式：本机不跑 oauth，改为从 Server /quotas 自动同步 cached_quota（约 5 分钟一轮）'
                                : '按最后更新时间排序，自动循环刷新所有账号的配额数据'}
                        </span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings?.remote_mode === 'client' ? false : (settings?.quota_refresh_enabled ?? false)}
                            disabled={settings?.remote_mode === 'client'}
                            onChange={async e => {
                                if (!settings) return;
                                const updated = { ...settings, quota_refresh_enabled: e.target.checked };
                                setSettings(updated);
                                await invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>
                {settings?.quota_refresh_enabled && settings?.remote_mode !== 'client' && (
                    <>
                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">刷新间隔（分钟/账号）</span>
                                <span className="setting-desc">每个账号之间的刷新间隔</span>
                            </div>
                            <input
                                type="number"
                                className="number-input"
                                min={1}
                                max={60}
                                value={settings.quota_refresh_interval}
                                onChange={async e => {
                                    const val = parseInt(e.target.value) || 5;
                                    const updated = { ...settings, quota_refresh_interval: val };
                                    setSettings(updated);
                                    await invoke('update_settings', { settings: updated });
                                }}
                            />
                        </div>
                        <div className="setting-item sub-item">
                            <div className="setting-info">
                                <span className="setting-label">每轮刷新账号数</span>
                                <span className="setting-desc">每轮循环刷新多少个账号</span>
                            </div>
                            <input
                                type="number"
                                className="number-input"
                                min={1}
                                max={10}
                                value={settings.quota_refresh_batch}
                                onChange={async e => {
                                    const val = parseInt(e.target.value) || 1;
                                    const updated = { ...settings, quota_refresh_batch: val };
                                    setSettings(updated);
                                    await invoke('update_settings', { settings: updated });
                                }}
                            />
                        </div>
                    </>
                )}
            </div>

            {/* 通知设置 */}
            <div className="settings-section">
                <h3>切号通知</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">macOS 系统通知</span>
                        <span className="setting-desc">切号时在屏幕右上角弹出系统通知</span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={isMacOS && (settings?.notify_on_switch ?? false)}
                            disabled={!isMacOS}
                            onChange={async e => {
                                if (!settings) return;
                                const updated = { ...settings, notify_on_switch: e.target.checked };
                                setSettings(updated);
                                await invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">对话内注入通知（实验性）</span>
                        <span className="setting-desc">切号后在 Codex 对话中插入一条切号提示消息。可能影响对话状态。</span>
                    </div>
                    <label className="toggle">
                        <input
                            type="checkbox"
                            checked={settings?.inject_switch_message ?? false}
                            onChange={async e => {
                                if (!settings) return;
                                const updated = { ...settings, inject_switch_message: e.target.checked };
                                setSettings(updated);
                                await invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="toggle-slider"></span>
                    </label>
                </div>
            </div>

            {/* Codex 配置 */}
            <div className="settings-section">
                <h3>Codex 配置</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">Fast 模式</span>
                        <span className="setting-desc">
                            更快推理速度，但消耗 2x 额度。{fastMode ? '当前：已开启' : '当前：已关闭'}
                        </span>
                    </div>
                    <button
                        className={`proxy-toggle-btn ${fastMode ? 'on' : 'off'}`}
                        onClick={async () => {
                            try {
                                const result = await invoke<string>('set_codex_fast_mode', { enable: !fastMode });
                                setFastMode(!fastMode);
                                setMessage({ type: 'success', text: result });
                                setTimeout(() => setMessage(null), 3000);
                            } catch (e) {
                                setMessage({ type: 'error', text: `${e}` });
                            }
                        }}
                    >
                        {fastMode ? '关闭 Fast' : '开启 Fast'}
                    </button>
                </div>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">Goals 实验特性</span>
                        <span className="setting-desc">
                            写入 <code>[features]</code> goals = true 到 ~/.codex/config.toml。
                            {goalsMode ? '当前：已开启' : '当前：已关闭'}
                        </span>
                    </div>
                    <button
                        className={`proxy-toggle-btn ${goalsMode ? 'on' : 'off'}`}
                        onClick={async () => {
                            try {
                                const result = await invoke<string>('set_codex_features_goals', { enable: !goalsMode });
                                setGoalsMode(!goalsMode);
                                setMessage({ type: 'success', text: result });
                                setTimeout(() => setMessage(null), 3000);
                            } catch (e) {
                                setMessage({ type: 'error', text: `${e}` });
                            }
                        }}
                    >
                        {goalsMode ? '关闭 Goals' : '开启 Goals'}
                    </button>
                </div>
            </div>

            {/* 进程管理 */}
            <div className="settings-section">
                <h3>进程管理</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">终止所有 Codex 进程</span>
                        <span className="setting-desc">
                            强制终止所有运行中的 codex 进程，用于切换代理模式后重启或排错
                        </span>
                    </div>
                    <button
                        className="action-button warning"
                        onClick={handleKill}
                        disabled={killing}
                    >
                        {killing ? '终止中...' : '终止进程'}
                    </button>
                </div>
            </div>

            {/* 智能切号策略 */}
            <div className="settings-section">
                <h3>智能切号策略</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">5h 配额预防性切号阈值</span>
                        <span className="setting-desc">剩余配额低于此百分比时提前切号（0 = 仅 429 触发，推荐 10）</span>
                    </div>
                    <div className="threshold-input-group">
                        <input
                            type="number"
                            className="number-input"
                            min={0}
                            max={50}
                            value={settings?.proxy_threshold_5h ?? 0}
                            onChange={e => {
                                if (!settings) return;
                                const val = parseInt(e.target.value) || 0;
                                const updated = { ...settings, proxy_threshold_5h: val };
                                setSettings(updated);
                                invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="threshold-unit">%</span>
                    </div>
                </div>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">周配额预防性切号阈值</span>
                        <span className="setting-desc">剩余周配额低于此百分比时提前切号（0 = 仅 429 触发，推荐 5）</span>
                    </div>
                    <div className="threshold-input-group">
                        <input
                            type="number"
                            className="number-input"
                            min={0}
                            max={50}
                            value={settings?.proxy_threshold_weekly ?? 0}
                            onChange={e => {
                                if (!settings) return;
                                const val = parseInt(e.target.value) || 0;
                                const updated = { ...settings, proxy_threshold_weekly: val };
                                setSettings(updated);
                                invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="threshold-unit">%</span>
                    </div>
                </div>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">Free 账号保护线</span>
                        <span className="setting-desc">Free 账号剩余配额低于此百分比时强制切号（0 = 不特殊处理，推荐 35）</span>
                    </div>
                    <div className="threshold-input-group">
                        <input
                            type="number"
                            className="number-input"
                            min={0}
                            max={80}
                            value={settings?.proxy_free_guard ?? 0}
                            onChange={e => {
                                if (!settings) return;
                                const val = parseInt(e.target.value) || 0;
                                const updated = { ...settings, proxy_free_guard: val };
                                setSettings(updated);
                                invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="threshold-unit">%</span>
                    </div>
                </div>
            </div>

            {/* SSE Bootstrap 嗅探窗口：拦截 mid-stream 限额事件的缓冲大小 */}
            <div className="settings-section">
                <h3>SSE Bootstrap 嗅探窗口</h3>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">字节上限</span>
                        <span className="setting-desc">
                            读到这么多字节还没见到内容事件就放行 codex（默认 32768，越大越能逮住慢上游的限额事件）
                        </span>
                    </div>
                    <div className="threshold-input-group">
                        <input
                            type="number"
                            className="number-input"
                            min={4096}
                            max={1048576}
                            step={4096}
                            value={settings?.proxy_bootstrap_byte_cap ?? 32768}
                            onChange={e => {
                                if (!settings) return;
                                const val = parseInt(e.target.value) || 32768;
                                const updated = { ...settings, proxy_bootstrap_byte_cap: val };
                                setSettings(updated);
                                invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="threshold-unit">B</span>
                    </div>
                </div>
                <div className="setting-item">
                    <div className="setting-info">
                        <span className="setting-label">时间上限</span>
                        <span className="setting-desc">
                            等首个内容事件最长等多久（默认 8000ms）。配合 SSE keep-alive 心跳，拉到 30000 也不会让 codex 那头超时。
                        </span>
                    </div>
                    <div className="threshold-input-group">
                        <input
                            type="number"
                            className="number-input"
                            min={1000}
                            max={60000}
                            step={500}
                            value={settings?.proxy_bootstrap_time_cap_ms ?? 8000}
                            onChange={e => {
                                if (!settings) return;
                                const val = parseInt(e.target.value) || 8000;
                                const updated = { ...settings, proxy_bootstrap_time_cap_ms: val };
                                setSettings(updated);
                                invoke('update_settings', { settings: updated });
                            }}
                        />
                        <span className="threshold-unit">ms</span>
                    </div>
                </div>
            </div>

        </div>
    );
}
