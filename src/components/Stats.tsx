import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import {
    AreaChart, Area, BarChart, Bar, PieChart, Pie, Cell,
    XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend
} from 'recharts';
import { formatPlanLabel } from '../utils/planLabel';
import './Stats.css';

interface TokenHistoryEntry {
    timestamp: string;
    model: string;
    input_tokens: number;
    output_tokens: number;
    cost: number;
}

interface SwitchEvent {
    timestamp: string;
    from_account: string | null;
    to_account: string;
    reason: string;
    from_quota_5h: number | null;
    to_quota_5h: number | null;
}

interface SwitchStats {
    today_count: number;
    week_count: number;
    total_count: number;
    by_reason: Record<string, number>;
    by_account: Record<string, number>;
}

interface TokenStats {
    total_input_tokens: number;
    total_output_tokens: number;
    total_tokens: number;
    total_cost_usd: number;
    total_requests: number;
}

interface TurnStateObservation {
    observed_at: string;
    account_id: string;
    model: string;
    session_key_hash: string | null;
    source: string;
    status: number;
    length: number;
    classification: string;
    time_status: string;
    envelope_ok: boolean;
    blocks: number | null;
    issued_at: string | null;
    expires_at: string | null;
    heuristic_usable: boolean;
    fingerprint: string;
}

interface TurnStateStatus {
    header: string;
    total_observations: number;
    by_classification: Record<string, number>;
    last: TurnStateObservation | null;
    recent: TurnStateObservation[];
}

interface PlanCapacityEstimate {
    plan_type: string;
    window_type: '5h' | 'week';
    sample_count: number;
    avg_capacity: number;
    median_capacity: number;
    min_capacity: number;
    max_capacity: number;
}

interface SessionInCycle {
    session_key: string;
    total_tokens: number;
    turn_count: number;
    first_seen_at: number;
    last_seen_at: number;
}

interface CycleDetail {
    window_start: number;
    window_end: number;
    is_current: boolean;
    total_tokens: number;
    input_tokens: number;
    cached_input_tokens: number;
    output_tokens: number;
    turn_count: number;
    sessions: SessionInCycle[];
    estimated_capacity: number | null;
    estimate_used_pct: number | null;
    hit_limit: boolean;
    last_switch_reason: string | null;
}

interface AccountTokenHistory {
    account_id: string;
    email: string;
    plan_type: string;
    is_current: boolean;
    is_banned: boolean;
    is_token_invalid: boolean;
    current_5h: CycleDetail | null;
    last_5h: CycleDetail | null;
    current_week: CycleDetail | null;
    last_week: CycleDetail | null;
    cycles_5h: CycleDetail[];
    cycles_week: CycleDetail[];
}

const COLORS = ['#8b5cf6', '#10b981', '#f59e0b', '#ef4444', '#3b82f6', '#ec4899', '#14b8a6', '#f97316'];

function formatTokens(n: number): string {
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
    if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K';
    return n.toString();
}

function formatTime(ts: string): string {
    const d = new Date(ts);
    return d.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

function formatDuration(from: string, to: string): string {
    const diff = new Date(to).getTime() - new Date(from).getTime();
    if (diff < 0) return '-';
    const mins = Math.floor(diff / 60000);
    const hours = Math.floor(mins / 60);
    if (hours > 0) return `${hours}h ${mins % 60}m`;
    return `${mins}m`;
}

type TimeRange = 'day' | 'week' | 'month';

export function Stats() {
    const [range, setRange] = useState<TimeRange>('week');
    const [tokenHistory, setTokenHistory] = useState<TokenHistoryEntry[]>([]);
    const [switchHistory, setSwitchHistory] = useState<SwitchEvent[]>([]);
    const [switchStats, setSwitchStats] = useState<SwitchStats | null>(null);
    const [tokenStats, setTokenStats] = useState<TokenStats | null>(null);
    const [turnStateStatus, setTurnStateStatus] = useState<TurnStateStatus | null>(null);
    const [planCaps, setPlanCaps] = useState<PlanCapacityEstimate[]>([]);
    const [accountHistory, setAccountHistory] = useState<AccountTokenHistory[]>([]);
    const [expandedAccount, setExpandedAccount] = useState<string | null>(null);
    const [expandedCycle, setExpandedCycle] = useState<string | null>(null);

    const days = range === 'day' ? 1 : range === 'week' ? 7 : 30;

    const fetchData = async () => {
        try {
            const [th, sh, ss, ts, pc, ah] = await Promise.all([
                invoke<TokenHistoryEntry[]>('get_token_history', { days }),
                invoke<SwitchEvent[]>('get_switch_history', { days }),
                invoke<SwitchStats>('get_switch_stats'),
                invoke<TokenStats>('get_token_stats'),
                invoke<PlanCapacityEstimate[]>('get_plan_capacity_estimates'),
                invoke<AccountTokenHistory[]>('get_account_token_history'),
            ]);
            setTokenHistory(th);
            setSwitchHistory(sh.reverse());
            setSwitchStats(ss);
            setTokenStats(ts);
            setPlanCaps(pc);
            setAccountHistory(ah);
        } catch (e) {
            console.error('加载统计数据失败:', e);
        }
    };

    useEffect(() => { fetchData(); }, [range]);

    useEffect(() => {
        let disposed = false;
        const refreshTurnState = async () => {
            try {
                const status = await invoke<TurnStateStatus>('get_turn_state_status');
                if (!disposed) setTurnStateStatus(status);
            } catch {
                // The proxy may be stopped; keep the last read-only snapshot.
            }
        };
        refreshTurnState();
        const timer = window.setInterval(refreshTurnState, 5000);
        return () => {
            disposed = true;
            window.clearInterval(timer);
        };
    }, []);

    // 聚合 token 趋势数据（按小时/天）
    const trendData = (() => {
        const buckets: Record<string, { label: string; input: number; output: number; cost: number }> = {};
        for (const entry of tokenHistory) {
            const d = new Date(entry.timestamp);
            const key = range === 'day'
                ? d.toLocaleTimeString('zh-CN', { hour: '2-digit' })
                : d.toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' });
            if (!buckets[key]) buckets[key] = { label: key, input: 0, output: 0, cost: 0 };
            buckets[key].input += entry.input_tokens;
            buckets[key].output += entry.output_tokens;
            buckets[key].cost += entry.cost;
        }
        return Object.values(buckets);
    })();

    // 按模型分布（饼图）
    const modelData = (() => {
        const map: Record<string, number> = {};
        for (const entry of tokenHistory) {
            map[entry.model] = (map[entry.model] || 0) + entry.input_tokens + entry.output_tokens;
        }
        return Object.entries(map).map(([name, value]) => ({ name, value }));
    })();

    // 切号原因分布
    const reasonData = switchStats
        ? Object.entries(switchStats.by_reason).map(([name, value]) => ({ name, value }))
        : [];

    const accountCount = switchStats ? Object.keys(switchStats.by_account).length : 0;

    // 分离常规切号与系统后台任务
    const actualSwitches = switchHistory.filter(e => e.reason !== '自动刷新' && e.reason !== '后台保活');
    const systemLogs = switchHistory.filter(e => e.reason === '自动刷新' || e.reason === '后台保活');

    return (
        <div className="stats-page">
            <div className="stats-header">
                <h2>统计</h2>
                <div className="time-range-btns">
                    {(['day', 'week', 'month'] as TimeRange[]).map(r => (
                        <button
                            key={r}
                            className={`range-btn ${range === r ? 'active' : ''}`}
                            onClick={() => setRange(r)}
                        >
                            {r === 'day' ? '日' : r === 'week' ? '周' : '月'}
                        </button>
                    ))}
                </div>
            </div>

            {/* 摘要卡片 */}
            <div className="stats-cards">
                <div className="stat-card purple">
                    <div className="stat-card-value">{formatTokens(tokenStats?.total_tokens ?? 0)}</div>
                    <div className="stat-card-label">Token 总量</div>
                </div>
                <div className="stat-card yellow">
                    <div className="stat-card-value">${(tokenStats?.total_cost_usd ?? 0).toFixed(2)}</div>
                    <div className="stat-card-label">总费用</div>
                </div>
                <div className="stat-card green">
                    <div className="stat-card-value">{switchStats?.total_count ?? 0}</div>
                    <div className="stat-card-label">切号次数</div>
                </div>
                <div className="stat-card blue">
                    <div className="stat-card-value">{accountCount}</div>
                    <div className="stat-card-label">使用账号数</div>
                </div>
            </div>

            <div className="stats-section turn-state-section">
                <h3>Turn-State 被动观测</h3>
                <div className="turn-state-summary">
                    <div>
                        <span className="turn-state-value">{turnStateStatus?.total_observations ?? 0}</span>
                        <span className="turn-state-label">响应头观测</span>
                    </div>
                    <div>
                        <span className="turn-state-value good">{turnStateStatus?.by_classification.personal_normal ?? 0}</span>
                        <span className="turn-state-label">292 正常</span>
                    </div>
                    <div>
                        <span className="turn-state-value good">{turnStateStatus?.by_classification.team_normal ?? 0}</span>
                        <span className="turn-state-label">332 Team</span>
                    </div>
                    <div>
                        <span className="turn-state-value warn">{turnStateStatus?.by_classification.limited_or_degraded ?? 0}</span>
                        <span className="turn-state-label">312/356 受限形状</span>
                    </div>
                    <div>
                        <span className="turn-state-value muted">{(turnStateStatus?.by_classification.other_length ?? 0) + (turnStateStatus?.by_classification.malformed ?? 0) + (turnStateStatus?.by_classification.unknown_envelope ?? 0)}</span>
                        <span className="turn-state-label">未知/异常</span>
                    </div>
                </div>
                <div className="turn-state-note">
                    仅读取上游响应头，不主动探测、不注入、不重放；长度和时间判断是经验性观测，不代表官方质量指标。
                </div>
                {turnStateStatus?.recent.length ? (
                    <div className="turn-state-table">
                        <div className="turn-state-row turn-state-header">
                            <span>时间</span><span>账号 / 模型</span><span>形状</span><span>时间状态</span><span>来源</span>
                        </div>
                        {turnStateStatus.recent.slice(0, 8).map((item, index) => (
                            <div className="turn-state-row" key={`${item.observed_at}-${item.fingerprint}-${index}`}>
                                <span className="log-time">{formatTime(item.observed_at)}</span>
                                <span title={item.account_id}>{shortName(item.account_id || '—')} / {item.model || '—'}</span>
                                <span className={item.heuristic_usable ? 'turn-state-good' : 'turn-state-warn'}>
                                    {turnStateLabel(item.classification)} ({item.length})
                                </span>
                                <span>{turnStateTimeLabel(item.time_status)}</span>
                                <span>{item.source === 'websocket_handshake' ? 'WS 握手' : 'HTTP 响应'}</span>
                            </div>
                        ))}
                    </div>
                ) : (
                    <div className="log-empty">代理启动后，收到官方 ChatGPT HTTP/SSE 或 WebSocket 握手响应头才会出现观测。</div>
                )}
            </div>

            {/* Plan 配额上限估算（基于 quota 快照 Δpct） */}
            {planCaps.length > 0 && (() => {
                const allPlans = Array.from(new Set(planCaps.map(p => p.plan_type))).sort();
                return (
                    <div className="stats-section">
                        <h3>Plan 配额上限估算（Δpct 反推，不依赖账号打满）</h3>
                        <div className="cycle-summary">
                            <div className="cycle-summary-row cycle-summary-header capacity-header">
                                <span>Plan</span>
                                <span className="num">5h 样本</span>
                                <span className="num">5h 中位</span>
                                <span className="num">5h 平均</span>
                                <span className="num">5h min–max</span>
                                <span className="num">周 样本</span>
                                <span className="num">周 中位</span>
                                <span className="num">周 平均</span>
                                <span className="num">周 min–max</span>
                            </div>
                            {allPlans.map(plan => {
                                const f = planCaps.find(p => p.plan_type === plan && p.window_type === '5h');
                                const w = planCaps.find(p => p.plan_type === plan && p.window_type === 'week');
                                return (
                                    <div key={plan} className="cycle-summary-row capacity-row">
                                        <span>
                                            <span className={`quota-plan plan-${plan.toLowerCase()}`}>{plan || '—'}</span>
                                        </span>
                                        <span className="num">{f?.sample_count ?? '—'}</span>
                                        <span className="num total">{f ? formatTokens(f.median_capacity) : '—'}</span>
                                        <span className="num">{f ? formatTokens(f.avg_capacity) : '—'}</span>
                                        <span className="num observed">
                                            {f ? `${formatTokens(f.min_capacity)}–${formatTokens(f.max_capacity)}` : '—'}
                                        </span>
                                        <span className="num">{w?.sample_count ?? '—'}</span>
                                        <span className="num total">{w ? formatTokens(w.median_capacity) : '—'}</span>
                                        <span className="num">{w ? formatTokens(w.avg_capacity) : '—'}</span>
                                        <span className="num observed">
                                            {w ? `${formatTokens(w.min_capacity)}–${formatTokens(w.max_capacity)}` : '—'}
                                        </span>
                                    </div>
                                );
                            })}
                        </div>
                        <div className="cycle-hint">
                            <b>原理</b>：每次切号前后强制抓 quota 写 `~/.codex-switcher/quota-snapshots.jsonl`。同一窗口内任意两次快照的 Δused_pct 配合期间代理 tokens → 推出该 Plan 总容量（capacity = Δtokens / Δpct × 100）。<br/>
                            <b>中位</b>是去掉异常值后最稳的估计。Δpct&lt;3% 的样本被丢弃（used_pct 是整数，量化误差会失真）。<br/>
                            样本会随每次切号自动累积；样本数低于 ~5 时估计仍有偏差，多用几小时即可。
                        </div>
                    </div>
                );
            })()}

            {/* 每号 Token 历史（三级下钻：号 → 周期 → session） */}
            {accountHistory.length > 0 && (
                <div className="stats-section">
                    <h3>每号 Token 历史（精确累加 + 估算上限）</h3>
                    <div className="acct-hist-table">
                        <div className="acct-hist-row acct-hist-header">
                            <span></span>
                            <span>邮箱 / Plan</span>
                            <span className="num">当前 5h</span>
                            <span className="num">上次 5h</span>
                            <span className="num">当前 周</span>
                            <span className="num">上次 周</span>
                        </div>
                        {accountHistory.map(acc => {
                            const expanded = expandedAccount === acc.account_id;
                            return (
                                <div key={acc.account_id}>
                                    <div
                                        className={`acct-hist-row clickable ${acc.is_current ? 'current' : ''}`}
                                        onClick={() => {
                                            setExpandedAccount(expanded ? null : acc.account_id);
                                            setExpandedCycle(null);
                                        }}
                                    >
                                        <span className="acct-toggle">{expanded ? '▼' : '▶'}</span>
                                        <span className="acct-email" title={acc.email}>
                                            {acc.is_current && <span className="quota-badge current">当前</span>}
                                            {acc.is_banned && <span className="quota-badge banned">封</span>}
                                            {acc.is_token_invalid && <span className="quota-badge invalid">失效</span>}
                                            <span className={`quota-plan plan-${(acc.plan_type || 'unknown').toLowerCase()}`}>{formatPlanLabel(acc.plan_type) || '—'}</span>
                                            <span className="acct-email-text" translate="no">{acc.email}</span>
                                        </span>
                                        <CellPair cycle={acc.current_5h} />
                                        <CellPair cycle={acc.last_5h} />
                                        <CellPair cycle={acc.current_week} />
                                        <CellPair cycle={acc.last_week} />
                                    </div>
                                    {expanded && (
                                        <div className="acct-hist-expand">
                                            <div className="acct-cycle-header">5h 周期（{acc.cycles_5h.length}）</div>
                                            <CycleHistoryTable
                                                cycles={acc.cycles_5h}
                                                accountId={acc.account_id}
                                                windowLabel="5h"
                                                expandedCycle={expandedCycle}
                                                setExpandedCycle={setExpandedCycle}
                                            />
                                            <div className="acct-cycle-header">周周期（{acc.cycles_week.length}）</div>
                                            <CycleHistoryTable
                                                cycles={acc.cycles_week}
                                                accountId={acc.account_id}
                                                windowLabel="week"
                                                expandedCycle={expandedCycle}
                                                setExpandedCycle={setExpandedCycle}
                                            />
                                        </div>
                                    )}
                                </div>
                            );
                        })}
                    </div>
                    <div className="cycle-hint">
                        每格显示「<b>实测累加 / 估算上限</b>」。<br/>
                        <b>实测累加</b> = token-history.jsonl 在该窗口内的精确求和（不平均、不打折，无论中间切号几次）。<br/>
                        <b>估算上限</b> = `实测累加 ÷ snapshot used_pct × 100`，用快照里 used_pct 最大那个算（量化误差最小）。`?%` 标记的 used_pct 偏小，估算误差大。<br/>
                        🔴 = 窗口内触发过限额切号 —— 此时实测累加 ≈ Plan 实际窗口配额。<br/>
                        点开账号看历史周期，点开周期看 session 明细。
                    </div>
                </div>
            )}

            {/* Token 趋势图 */}
            {trendData.length > 0 && (
                <div className="stats-section">
                    <h3>Token 趋势</h3>
                    <ResponsiveContainer width="100%" height={250}>
                        <AreaChart data={trendData}>
                            <CartesianGrid strokeDasharray="3 3" stroke="rgba(255,255,255,0.06)" />
                            <XAxis dataKey="label" stroke="rgba(255,255,255,0.3)" fontSize={11} />
                            <YAxis stroke="rgba(255,255,255,0.3)" fontSize={11} tickFormatter={formatTokens} />
                            <Tooltip
                                contentStyle={{ background: '#1e1245', border: '1px solid rgba(255,255,255,0.1)', borderRadius: 8 }}
                                labelStyle={{ color: '#fff' }}
                            />
                            <Area type="monotone" dataKey="input" stackId="1" stroke="#8b5cf6" fill="#8b5cf6" fillOpacity={0.4} name="Input" />
                            <Area type="monotone" dataKey="output" stackId="1" stroke="#10b981" fill="#10b981" fillOpacity={0.4} name="Output" />
                            <Legend />
                        </AreaChart>
                    </ResponsiveContainer>
                </div>
            )}

            {/* 下半部：费用 + 模型分布 */}
            <div className="stats-grid">
                {trendData.length > 0 && (
                    <div className="stats-section">
                        <h3>费用趋势</h3>
                        <ResponsiveContainer width="100%" height={200}>
                            <BarChart data={trendData}>
                                <CartesianGrid strokeDasharray="3 3" stroke="rgba(255,255,255,0.06)" />
                                <XAxis dataKey="label" stroke="rgba(255,255,255,0.3)" fontSize={11} />
                                <YAxis stroke="rgba(255,255,255,0.3)" fontSize={11} tickFormatter={v => `$${v}`} />
                                <Tooltip
                                    contentStyle={{ background: '#1e1245', border: '1px solid rgba(255,255,255,0.1)', borderRadius: 8 }}
                                    formatter={(v) => [`$${Number(v).toFixed(4)}`, '费用']}
                                />
                                <Bar dataKey="cost" fill="#fbbf24" radius={[4, 4, 0, 0]} />
                            </BarChart>
                        </ResponsiveContainer>
                    </div>
                )}

                {(modelData.length > 0 || reasonData.length > 0) && (
                    <div className="stats-section">
                        <h3>{modelData.length > 0 ? '模型分布' : '切号原因'}</h3>
                        <ResponsiveContainer width="100%" height={200}>
                            <PieChart>
                                <Pie
                                    data={modelData.length > 0 ? modelData : reasonData}
                                    cx="50%"
                                    cy="50%"
                                    innerRadius={50}
                                    outerRadius={80}
                                    paddingAngle={3}
                                    dataKey="value"
                                >
                                    {(modelData.length > 0 ? modelData : reasonData).map((_, i) => (
                                        <Cell key={i} fill={COLORS[i % COLORS.length]} />
                                    ))}
                                </Pie>
                                <Tooltip
                                    contentStyle={{ background: '#1e1245', border: '1px solid rgba(255,255,255,0.1)', borderRadius: 8 }}
                                />
                                <Legend />
                            </PieChart>
                        </ResponsiveContainer>
                    </div>
                )}
            </div>

            {/* 切号日志 */}
            <div className="stats-section">
                <h3>切号日志 ({actualSwitches.length} 条)</h3>
                <div className="switch-log-table">
                    <div className="log-header">
                        <span>时间</span>
                        <span>切换路径</span>
                        <span>原因</span>
                        <span>使用时长</span>
                    </div>
                    {actualSwitches.length === 0 ? (
                        <div className="log-empty">暂无切号记录</div>
                    ) : (
                        actualSwitches.map((e, i) => (
                            <div key={i} className="log-row">
                                <span className="log-time">{formatTime(e.timestamp)}</span>
                                <span className="log-path">
                                    {e.from_account ? (
                                        <>{shortName(e.from_account)} → {shortName(e.to_account)}</>
                                    ) : (
                                        <>→ {shortName(e.to_account)}</>
                                    )}
                                </span>
                                <span className={`log-reason ${reasonClass(e.reason)}`}>{e.reason}</span>
                                <span className="log-duration">
                                    {i < actualSwitches.length - 1
                                        ? formatDuration(actualSwitches[i + 1].timestamp, e.timestamp)
                                        : '-'}
                                </span>
                            </div>
                        ))
                    )}
                </div>
            </div>

            {/* 后台任务日志 */}
            {systemLogs.length > 0 && (
                <div className="stats-section">
                    <h3>后台任务日志 ({systemLogs.length} 条)</h3>
                    <div className="switch-log-table">
                        <div className="log-header">
                            <span>时间</span>
                            <span>目标账号</span>
                            <span>任务类型</span>
                            <span>刷新后额度</span>
                        </div>
                        {systemLogs.map((e, i) => (
                            <div key={`sys-${i}`} className="log-row">
                                <span className="log-time">{formatTime(e.timestamp)}</span>
                                <span className="log-path">
                                    {shortName(e.to_account)}
                                </span>
                                <span className={`log-reason ${reasonClass(e.reason)}`}>{e.reason}</span>
                                <span className="log-duration" style={{ color: 'var(--success-color, #10b981)' }}>
                                    {e.to_quota_5h !== null ? `${e.to_quota_5h}%` : '成功'}
                                </span>
                            </div>
                        ))}
                    </div>
                </div>
            )}
        </div>
    );
}

function shortName(name: string): string {
    if (name.length > 18) return name.slice(0, 15) + '...';
    return name;
}

function turnStateLabel(classification: string): string {
    switch (classification) {
        case 'personal_normal': return '292 正常';
        case 'team_normal': return '332 Team';
        case 'limited_or_degraded': return '受限';
        case 'malformed': return '格式错误';
        case 'unknown_envelope': return '未知封装';
        default: return '其他长度';
    }
}

function turnStateTimeLabel(status: string): string {
    switch (status) {
        case 'unparsed': return '时间未解析';
        case 'invalid_timestamp': return '时间戳无效';
        case 'future': return '时间在未来';
        case 'expired_or_near_expiry': return '已过期或即将过期';
        case 'current': return '时间有效';
        default: return status;
    }
}

function formatWindow(startSec: number, endSec: number): string {
    const s = new Date(startSec * 1000);
    const e = new Date(endSec * 1000);
    const fmtDate = (d: Date) => `${d.getMonth() + 1}/${d.getDate()}`;
    const fmtTime = (d: Date) => `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
    if (fmtDate(s) === fmtDate(e)) {
        return `${fmtDate(s)} ${fmtTime(s)}-${fmtTime(e)}`;
    }
    return `${fmtDate(s)} ${fmtTime(s)} → ${fmtDate(e)} ${fmtTime(e)}`;
}

/// 顶层一格：「实测累加 / 估算上限」
/// null 也用 cell-pair 双行结构，保证上下与有数据的格子严格对齐。
function CellPair({ cycle }: { cycle: CycleDetail | null }) {
    if (!cycle) {
        return (
            <span className="num cell-pair cell-empty">
                <span className="cell-actual">—</span>
                <span className="cell-est">&nbsp;</span>
            </span>
        );
    }
    const cap = cycle.estimated_capacity;
    const pct = cycle.estimate_used_pct;
    const lowConfidence = pct != null && pct < 10;
    const isFallback = !cycle.is_current;
    return (
        <span className={`num cell-pair ${isFallback ? 'cell-fallback' : ''}`}>
            <span className="cell-actual">
                {cycle.hit_limit && <span className="cell-fire">🔴</span>}
                {formatTokens(cycle.total_tokens)}
            </span>
            <span className="cell-est" title={pct != null ? `quota snapshot used_pct=${pct}% · 窗口 ${formatWindow(cycle.window_start, cycle.window_end)}` : `窗口 ${formatWindow(cycle.window_start, cycle.window_end)}`}>
                {isFallback && <span className="cell-fallback-tag">最近</span>}
                {cap != null
                    ? `~${formatTokens(cap)}${lowConfidence ? '?' : ''}`
                    : (isFallback ? <>&nbsp;</> : ' ')}
            </span>
        </span>
    );
}

/// 周期列表（5h 或周）。点开一行看 session 明细。
function CycleHistoryTable({
    cycles,
    accountId,
    windowLabel,
    expandedCycle,
    setExpandedCycle,
}: {
    cycles: CycleDetail[];
    accountId: string;
    windowLabel: '5h' | 'week';
    expandedCycle: string | null;
    setExpandedCycle: (k: string | null) => void;
}) {
    if (cycles.length === 0) {
        return <div className="acct-empty">无数据</div>;
    }
    return (
        <div className="hist-cycle-table">
            <div className="hist-cycle-row hist-cycle-header">
                <span></span>
                <span>窗口</span>
                <span className="num">实测累加</span>
                <span className="num">估算上限</span>
                <span className="num">used_pct</span>
                <span className="num">轮数</span>
                <span className="num">session 数</span>
                <span>状态</span>
            </div>
            {cycles.map(c => {
                const key = `${accountId}-${windowLabel}-${c.window_end}`;
                const expanded = expandedCycle === key;
                const rowCls = c.is_current
                    ? 'current'
                    : c.hit_limit
                        ? 'limit-hit'
                        : '';
                const statusLabel = c.is_current
                    ? '进行中'
                    : c.hit_limit
                        ? `🔴 ${c.last_switch_reason ?? '限额'}`
                        : '正常';
                return (
                    <div key={key}>
                        <div
                            className={`hist-cycle-row clickable ${rowCls}`}
                            onClick={() => setExpandedCycle(expanded ? null : key)}
                        >
                            <span className="acct-toggle">{expanded ? '▼' : '▶'}</span>
                            <span className="cycle-window">{formatWindow(c.window_start, c.window_end)}</span>
                            <span className="num total">{c.total_tokens.toLocaleString()}</span>
                            <span className="num">
                                {c.estimated_capacity != null ? `~${formatTokens(c.estimated_capacity)}` : '—'}
                            </span>
                            <span className="num">
                                {c.estimate_used_pct != null ? `${c.estimate_used_pct}%` : '—'}
                            </span>
                            <span className="num">{c.turn_count}</span>
                            <span className="num">{c.sessions.length}</span>
                            <span className={c.is_current ? 'cycle-status current' : c.hit_limit ? 'cycle-status limit' : 'cycle-status done'}>
                                {statusLabel}
                            </span>
                        </div>
                        {expanded && (
                            <div className="hist-session-table">
                                <div className="hist-session-row hist-session-header">
                                    <span>Session</span>
                                    <span className="num">轮数</span>
                                    <span className="num">Token</span>
                                    <span>首次 → 末次</span>
                                </div>
                                {c.sessions.map((s, idx) => (
                                    <div key={idx} className="hist-session-row">
                                        <span className="cycle-window" title={s.session_key}>{s.session_key.length > 40 ? s.session_key.slice(0, 40) + '…' : s.session_key}</span>
                                        <span className="num">{s.turn_count}</span>
                                        <span className="num total">{s.total_tokens.toLocaleString()}</span>
                                        <span className="cycle-window">
                                            {formatWindow(s.first_seen_at, s.last_seen_at)}
                                        </span>
                                    </div>
                                ))}
                            </div>
                        )}
                    </div>
                );
            })}
        </div>
    );
}

function reasonClass(reason: string): string {
    if (reason.includes('手动')) return 'manual';
    if (reason.includes('429') || reason.includes('限额')) return 'ratelimit';
    if (reason.includes('封号')) return 'banned';
    if (reason.includes('保活')) return 'keepalive';
    if (reason.includes('刷新')) return 'refresh';
    return 'auto';
}
