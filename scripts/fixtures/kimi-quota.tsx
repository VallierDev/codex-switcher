import {createRoot} from 'react-dom/client';
import {RelayQuotaWindows} from '../../src/components/RelayQuotaWindows';
import '../../src/App.css';
import '../../src/components/AccountList.css';
const now=Date.now();
const cache={remaining:0,unit:'% Kimi Code',is_active:false,updated_at:new Date(now).toISOString(),windows:[
    {label:'5H',remaining_percent:0,reset_at:Math.floor(now/1000)+3600},
    {label:'7D',remaining_percent:42,reset_at:Math.floor(now/1000)+180000},
]};
createRoot(document.getElementById('root')!).render(<main style={{padding:24,maxWidth:780}}>
    <h3>Kimi 编程套餐 · 示例数据</h3><RelayQuotaWindows cache={cache}/>
    <h3>未知额度与过期数据</h3><RelayQuotaWindows cache={{...cache,updated_at:new Date(now-600000).toISOString(),windows:[{label:'5H',remaining_percent:null,reset_at:null},{label:'7D',remaining_percent:100,reset_at:null}]}}/>
</main>);
