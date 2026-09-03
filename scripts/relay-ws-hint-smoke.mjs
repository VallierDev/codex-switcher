// Run with the isolated relay fixture server + debug Switcher on 18082.
import Ws from 'ws';
import assert from 'node:assert/strict';
const count=async()=> (await (await fetch('http://127.0.0.1:18090/__probe_counts')).json()).upgrades;
const before=await count();
const start=performance.now();
const ws=new Ws('ws://127.0.0.1:18082/v1/responses',{headers:{'x-codex-routing-hint':'model=relay-current:kimi-k3'}});
await new Promise((resolve,reject)=>{ws.once('open',resolve);ws.once('error',reject);});
const handshake=Math.round(performance.now()-start);
async function send(frame){return new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{ws.terminate();reject(Error('WS response timeout'));},10000);
    const listener=(data)=>{const event=JSON.parse(data.toString());if(event.type==='error'){clearTimeout(timer);ws.off('message',listener);reject(Error(JSON.stringify(event)));}
        if(event.type==='response.completed'){clearTimeout(timer);ws.off('message',listener);resolve(event);}};
    ws.on('message',listener);ws.send(JSON.stringify(frame));
});}
const warm=await send({type:'response.create',generate:false,input:[]});
assert.equal(warm.response.id,'');
const answer=await send({type:'response.create',input:'probe',stream:true}); // model intentionally omitted
assert.equal(answer.response.output[0].type,'custom_tool_call');
ws.close();
assert.equal(await count(),before,'Provider routing must not connect a ChatGPT/current-account WebSocket first');
console.log({passed:true,handshakeMs:handshake,upstreamWebsocketsOpened:0,modelHintUsed:true});
