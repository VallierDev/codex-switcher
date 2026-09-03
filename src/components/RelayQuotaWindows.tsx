import { useEffect, useState } from 'react';
import { Clock } from 'lucide-react';
import type { RelayUsageCache } from '../hooks/useAccounts';

export function RelayQuotaWindows({ cache }: { cache: RelayUsageCache }) {
    const [now,setNow]=useState(Date.now());
    useEffect(()=>{const timer=setInterval(()=>setNow(Date.now()),1000);return ()=>clearInterval(timer);},[]);
    const updated=Date.parse(cache.updated_at);
    const stale=!Number.isFinite(updated)||now-updated>180_000;
    return <div className="quota-grid" title={`最后更新：${Number.isFinite(updated)?new Date(updated).toLocaleString():'未知'}`}>
        {cache.windows?.map(window=>{
            const pct=window.remaining_percent;
            const known=typeof pct==='number'&&Number.isFinite(pct);
            const tone=known?(pct>50?'green':pct>20?'orange':'red'):'muted';
            const seconds=window.reset_at==null?null:Math.max(0,Math.ceil(window.reset_at-now/1000));
            const minutes=seconds==null?null:Math.ceil(seconds/60);
            const reset=minutes==null?'--':minutes===0?'待刷新':minutes>=1440?`${Math.floor(minutes/1440)}天 ${Math.floor(minutes%1440/60)}时`:minutes>=60?`${Math.floor(minutes/60)}时 ${minutes%60}分`:`${minutes}分`;
            return <div className="quota-mini-card" key={window.label} aria-label={`${window.label} 剩余 ${known?`${Math.round(pct)}%`:'未知'}`}>
                {known&&<div className={`quota-mini-bg ${tone}`} style={{width:`${Math.min(100,Math.max(0,pct))}%`}}/>}
                <div className="quota-mini-content">
                    <span className="quota-label">{window.label}</span>
                    <span style={{display:'inline-flex',alignItems:'center',gap:4,fontSize:11}}><Clock size={12}/>{reset}</span>
                    <span className={`quota-percent ${tone}`}>{known?`${Math.round(pct)}%`:'--'}</span>
                </div>
            </div>;
        })}
        {stale&&<span style={{fontSize:11,color:'var(--text-secondary)',gridColumn:'1 / -1'}}>上次数据已过期，请刷新额度</span>}
    </div>;
}
