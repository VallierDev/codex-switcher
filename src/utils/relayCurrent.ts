import type {Account} from '../hooks/useAccounts';

export function relayModelIds(account: Account): string[] {
    if(account.kind!=='relay'||(account.relay_protocol||'responses')!=='responses')return [];
    return [...new Set([account.relay_model_fallback,...Object.values(account.relay_model_map||{})]
        .filter((v):v is string=>typeof v==='string'&&!!v.trim()).map(v=>v.trim()))].sort();
}

export function relayCurrentState(account: Account, current: Record<string,string> = {}) {
    const models=relayModelIds(account);
    const active=models.filter(model=>current[model]===account.id);
    const provider=models.every(m=>/^kimi|^k3(?:-|$)/i.test(m))?'Kimi':models.every(m=>/^deepseek/i.test(m))?'DeepSeek':'模型';
    return {models,active,isCurrent:active.length>0,allCurrent:models.length>0&&active.length===models.length,
        label:active.length===models.length?`${provider} 当前`:'部分模型当前'};
}
