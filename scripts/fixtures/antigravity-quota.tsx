// Dev-only visual regression fixture: no Tauri bridge, real accounts, or network calls.
import React, { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { AntigravityQuota, type AntigravityModelQuota } from '../../src/components/AntigravityQuota';
import '../../src/App.css';
import '../../src/components/AccountList.css';

const reset = new Date(Date.now() + 4 * 3600_000).toISOString();
const quotas: Record<string, AntigravityModelQuota> = {
    'gemini-3.7-flash-high': { remaining_fraction: 0.99, reset_time: reset },
    'gemini-3.6-flash-high': { remaining_fraction: 0.99, reset_time: reset },
    'gemini-pro-agent': { remaining_fraction: 0.99, reset_time: reset },
    'gemini-3.1-pro-low': { remaining_fraction: 0.99, reset_time: reset },
    'claude-sonnet-4-6': { remaining_fraction: 0, reset_time: reset },
    'claude-opus-4-6-thinking': { remaining_fraction: 0.35, reset_time: null },
    'model-with-unknown-quota': { remaining_fraction: null, reset_time: 'invalid' },
    ...Object.fromEntries(Array.from({ length: 21 }, (_, i) => [`preview-model-${i}`, { remaining_fraction: 0.8, reset_time: reset }])),
};

function Preview() {
    const [live, setLive] = useState(quotas);
    const [dark, setDark] = useState(false);
    return <main data-theme={dark ? 'dark' : 'light'} style={{ padding: 24, maxWidth: 800, background: 'var(--bg-primary)', color: 'var(--text-primary)' }}>
        <h2>Google 模型额度验收（模拟数据）</h2>
        <button onClick={() => setLive({ ...live, 'new-model-after-refresh': { remaining_fraction: 0.7, reset_time: reset } })}>模拟刷新：新增模型</button>
        <button onClick={() => setDark(value => !value)}>切换深浅色</button>
        <h3>账号 A · 长邮箱回归</h3><div className="account-list-container"><div className="account-row">
            <div className="col-email"><span className="email-text">agwffgrtyrdfrebsdtgst@gmail.com</span><div className="badges" style={{ marginLeft: 8 }}>Google</div></div>
            <div className="col-quota-merged google-quota-column"><AntigravityQuota quotas={live} /></div>
        </div></div>
        <h3>账号 B</h3><div className="col-quota-merged google-quota-column"><AntigravityQuota quotas={quotas} /></div>
        <h3>未同步账号</h3><AntigravityQuota quotas={{}} />
    </main>;
}
createRoot(document.getElementById('root')!).render(<React.StrictMode><Preview /></React.StrictMode>);
