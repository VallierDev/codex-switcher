import {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {mockIPC} from '@tauri-apps/api/mocks';
import {AccountList} from '../../src/components/AccountList';
import type {Account,AppSettings} from '../../src/hooks/useAccounts';
import {relayModelIds} from '../../src/utils/relayCurrent';
import '../../src/App.css';
import '../../src/components/AccountList.css';
const accounts=[['kimi-a','Kimi A','k3'],['kimi-b','Kimi B','k3'],['deepseek-a','DeepSeek A','deepseek-v4-pro']].map(([id,name,model])=>({
    id,name,kind:'relay',auth_json:{},created_at:'2026-09-03T00:00:00Z',relay_base_url:'https://fixture.example/v1',relay_model_fallback:model,
    relay_category:'coding_plan',relay_protocol:'responses',is_banned:false,is_token_invalid:false,is_logged_out:false,
})) as Account[];
const selected: Record<string,string>={k3:'kimi-b','deepseek-v4-pro':'deepseek-a'};
mockIPC((cmd,payload)=>{
    if(cmd==='switch_relay_model_account') {
        const a=accounts.find(a=>a.id===(payload as {id:string}).id)!;
        for(const model of relayModelIds(a))selected[model]=a.id;
        return null;
    }
    return null;
});
function Preview(){
    const [settings,setSettings]=useState({remote_mode:'off',current_relay_accounts:{...selected}} as AppSettings);
    return <main style={{padding:24}}><h3>独立当前号 · 无真实账号</h3><AccountList accounts={accounts} currentId="unchanged-codex"
        settings={settings} onSwitch={()=>{throw Error('Must not switch global Codex account');}} onDelete={()=>{}}
        onUpdateAccount={async()=>{}} onUpdateSettings={()=>{}} onRefreshComplete={()=>setSettings({...settings,current_relay_accounts:{...selected}})}/></main>;
}
createRoot(document.getElementById('root')!).render(<Preview/>);
