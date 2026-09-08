import type {Account} from '../hooks/useAccounts';

export function relayModelIds(account: Account): string[] {
    if(account.kind!=='relay'||(account.relay_protocol||'responses')!=='responses')return [];
    const isAgy = /^(https?:\/\/)?(127\.0\.0\.1|localhost):28100\/v1\/?$/i.test(account.relay_base_url||'');
    const agyModels = isAgy ? [
        'gemini-3.8-flash-high','gemini-3.8-flash-medium','gemini-3.8-flash-low',
        'gemini-3.7-flash-high','gemini-3.7-flash-medium','gemini-3.7-flash-low',
        'gemini-3.6-flash-high','gemini-3.6-flash-medium','gemini-3.6-flash-low',
        'gemini-3.1-pro-high','gemini-3.1-pro-low','claude-sonnet-4-6',
        'claude-opus-4-6-thinking','gpt-oss-120b-medium',
    ] : [];
    return [...new Set([...agyModels,account.relay_model_fallback,...Object.values(account.relay_model_map||{})]
        .filter((v):v is string=>typeof v==='string'&&!!v.trim()).map(v=>v.trim()))].sort((a,b)=>{
        const rank=(id:string)=>id.startsWith('gemini-')?0:id.startsWith('claude-')?1:id.startsWith('gpt-')?2:3;
        const av=(a.match(/\d+/g)||[]).map(Number), bv=(b.match(/\d+/g)||[]).map(Number);
        return rank(a)-rank(b) || (bv[0]||0)-(av[0]||0) || (bv[1]||0)-(av[1]||0) || a.localeCompare(b);
    });
}

export function relayCurrentState(account: Account, current: Record<string,string> = {}) {
    const models=relayModelIds(account);
    const active=models.filter(model=>current[model]===account.id);
    const provider=models.every(m=>/^kimi|^k3(?:-|$)/i.test(m))?'Kimi':models.every(m=>/^deepseek/i.test(m))?'DeepSeek':'模型';
    return {models,active,isCurrent:active.length>0,allCurrent:models.length>0&&active.length===models.length,
        label:active.length===models.length?`${provider} 当前`:'部分模型当前'};
}
