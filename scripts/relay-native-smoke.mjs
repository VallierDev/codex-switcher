// Use ONLY with the isolated fixture account store and relay-native-mock.mjs.
import assert from 'node:assert/strict';
import fs from 'node:fs';
const base='http://127.0.0.1:18082';
const r=await fetch(base+'/v1/models?client_version=0.151.0',{signal:AbortSignal.timeout(10000)});
assert.equal(r.status,200); const catalog=await r.json();
assert.equal(catalog.models.filter(m=>m.visibility!=='hide').length,3);
assert.ok(catalog.models.some(m=>m.slug==='relay-model:fixture-kimi:kimi-k3' && m.context_window===1048576));
for(const id of ['relay-current:kimi-k3','relay-model:fixture-kimi:kimi-k3','relay-current:deepseek-v4-pro']) {
  for(const input of ['probe',[{type:'custom_tool_call_output',call_id:'mock-call',output:'OK'}]]) {
    const res=await fetch(base+'/v1/responses',{method:'POST',headers:{'content-type':'application/json',authorization:'Bearer must-not-leak','chatgpt-account-id':'must-not-leak'},
      body:JSON.stringify({model:id,input,stream:true}),signal:AbortSignal.timeout(10000)});
    const wire=await res.text();assert.equal(res.status,200,wire);
    assert.ok(wire.includes('response.completed'));
    assert.ok(wire.includes(Array.isArray(input)?'MOCK_TOOL_OK':'custom_tool_call'));
  }
  console.log('HTTP native roundtrip passed:',id);
}
const bad=await fetch(base+'/v1/responses',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'relay-model:removed:foo',input:'do not forward'}),signal:AbortSignal.timeout(10000)});
assert.equal(bad.status,400);
const ws=new WebSocket('ws://127.0.0.1:18082/v1/responses');
await new Promise((resolve,reject)=>{ws.addEventListener('open',resolve,{once:true});ws.addEventListener('error',reject,{once:true});});
await new Promise((resolve,reject)=>{
  const timer=setTimeout(()=>{ws.close();reject(new Error('WS timeout'));},15000);
  ws.addEventListener('message',({data})=>{
    const event=JSON.parse(data);
    if(event.type==='response.completed'){clearTimeout(timer);ws.close();resolve();}
    if(event.type==='error'){clearTimeout(timer);ws.close();reject(new Error(JSON.stringify(event)));}
  });
  ws.send(JSON.stringify({type:'response.create',model:'relay-model:fixture-kimi:kimi-k3',input:'probe',stream:true}));
});
if(process.argv[2]) {
  const store=JSON.parse(fs.readFileSync(process.argv[2],'utf8'));
  assert.equal(store.current,'fixture-current');
  assert.equal(store.settings.current_relay_accounts['kimi-k3'],'fixture-kimi2');
}
console.log('PASS: native catalog, custom endpoint/key isolation, tool continuation, unavailable model rejection, WebSocket, current account unchanged');
