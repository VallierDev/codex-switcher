import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { Account, RelayUsageCache } from './useAccounts';

export interface UsageDisplay {
    plan_type: string;
    five_hour_used: number;
    five_hour_left: number;
    five_hour_reset: string;
    five_hour_reset_at?: number;
    weekly_used: number;
    weekly_left: number;
    weekly_reset: string;
    weekly_reset_at?: number;
    credits_balance: number | null;
    has_credits: boolean;
    reset_credits?: number | null;
}

/// Relay (中转账号) 没有 OpenAI 5h+周窗口模型，把 GLM 这类返回的百分比剩余值
/// 映射到 UsageDisplay.five_hour_left，UsageCard 复用同一个进度条渲染。
function relayCacheToUsage(cache: RelayUsageCache, planLabel: string): UsageDisplay {
    const isPercent = (cache.unit ?? '').includes('%');
    const remaining = Number.isFinite(cache.remaining) ? cache.remaining : 0;
    return {
        plan_type: planLabel || 'relay',
        five_hour_used: isPercent ? Math.max(0, 100 - remaining) : 0,
        five_hour_left: isPercent ? remaining : 0,
        five_hour_reset: '',
        five_hour_reset_at: cache.next_reset_at ?? undefined,
        weekly_used: 0,
        weekly_left: 0,
        weekly_reset: '',
        weekly_reset_at: undefined,
        // 金额型 Relay：把 remaining 直接当 credits 显示
        credits_balance: isPercent ? null : remaining,
        has_credits: !isPercent,
    };
}

export function useUsage() {
    const [usage, setUsage] = useState<UsageDisplay | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const fetchUsage = useCallback(async () => {
        setLoading(true);
        setError(null);

        try {
            const currentId = await invoke<string | null>('get_current_account_id');
            if (!currentId) {
                setError('未设置当前账号');
                return;
            }
            // Relay 账号：走专属 fetcher（GLM /api/monitor/usage/quota/limit 等），
            // 不调 OpenAI usage（那条会返回 RELAY_ACCOUNT 错误）。
            const accounts = await invoke<Account[]>('get_accounts');
            const acc = accounts.find(a => a.id === currentId);
            const isRelay = (acc?.kind ?? '').toLowerCase() === 'relay';
            if (isRelay) {
                const cache = await invoke<RelayUsageCache>('refresh_relay_usage', { id: currentId });
                const label = (acc?.relay_homepage ? '中转' : 'GLM');
                setUsage(relayCacheToUsage(cache, label));
                return;
            }
            const data = await invoke<UsageDisplay>('get_quota_by_id', { id: currentId });
            setUsage(data);
        } catch (err) {
            setError(String(err));
        } finally {
            setLoading(false);
        }
    }, []);

    useEffect(() => {
        fetchUsage();
    }, [fetchUsage]);

    return {
        usage,
        loading,
        error,
        refresh: fetchUsage,
    };
}
