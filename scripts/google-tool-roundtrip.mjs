// Opt-in live Google adapter smoke test. Does not execute model-generated code.
// Run against an isolated proxy: node scripts/google-tool-roundtrip.mjs 18082
import assert from 'node:assert/strict';

const port = Number(process.argv[2] || 18082);
const model = process.argv[3] || 'gemini-3.8-flash-high';
const base = `http://127.0.0.1:${port}`;
const wsMode = process.argv[4] === 'ws';
const socket = wsMode ? new WebSocket(`ws://127.0.0.1:${port}/v1/responses`) : null;
if (socket) await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
function wsTurn(payload) {
  return new Promise((resolve,reject)=>{
    const events=[];
    const finish=(error)=>{clearTimeout(timer);socket.removeEventListener('message',onMessage); if(error){socket.close();reject(error);}else resolve(events);};
    const onMessage=({data})=>{
      const event=JSON.parse(data); events.push(event);
      if(event.type==='error') return finish(new Error(JSON.stringify(event.error)));
      if(['response.completed','response.failed','response.incomplete'].includes(event.type)) finish();
    };
    const timer=setTimeout(()=>finish(new Error('WS test timeout')),90000);
    socket.addEventListener('message',onMessage);
    socket.send(JSON.stringify({type:'response.create',...payload}));
  });
}
const tools = [{type:'namespace',name:'functions',tools:[{
  type:'custom',name:'exec',description:'Return the supplied JavaScript code to the local test harness.',format:{type:'text'}
}]}];
async function turn(input, choice) {
  const start = performance.now();
  const payload={model,stream:true,input,tools,tool_choice:choice,reasoning:{effort:'low'}};
  let events;
  if (socket) {
    events=await wsTurn(payload);
  } else {
  const response = await fetch(`${base}/v1/responses`, {
    method:'POST', headers:{'Content-Type':'application/json'},
    body:JSON.stringify(payload),
    signal:AbortSignal.timeout(90000),
  });
  if (response.status !== 200) console.log((await response.text()).slice(0,900));
  assert.equal(response.status,200,`HTTP ${response.status}`);
  const wire = await response.text();
  events = wire.split('\n').filter(l=>l.startsWith('data: '))
    .flatMap(l=>{try{return [JSON.parse(l.slice(6))];}catch{return [];}});
  }
  const last = events.findLast(e=>['response.completed','response.failed','response.incomplete'].includes(e.type));
  assert.ok(last,'missing terminal event');
  if (last.type !== 'response.completed') {
    // Print only provider error metadata, never credentials or request history.
    console.log({type:last.type,error:last.response?.error});
  }
  assert.equal(last.type,'response.completed');
  console.log(JSON.stringify({model,ms:Math.round(performance.now()-start),events:events.length,
    outputTypes:last.response.output.map(i=>i.type)}));
  return {events,output:last.response.output};
}
const initial = [{role:'user',content:'Call functions.exec exactly once with JavaScript text("TOOL_REQUEST_OK");. Wait for its result before answering.'}];
if (socket) {
  const warm = await wsTurn({model,generate:false,input:initial,tools});
  assert.equal(warm.at(-1).response.id,'','stateless prewarm must not advertise a reusable response id');
}
// Claude's upstream forbids forced tool_choice together with extended thinking.
const first = await turn(initial,model.startsWith('claude-') ? 'auto' : {type:'custom',name:'exec',namespace:'functions'});
const call = first.output.find(i=>i.type==='custom_tool_call');
assert.ok(call,'no custom tool call');
assert.equal(call.name,'exec'); assert.equal(call.namespace,'functions');
assert.equal(typeof call.input,'string'); assert.ok(call.input.length);
assert.ok(!('arguments' in call));
assert.ok(first.events.some(e=>e.type==='response.custom_tool_call_input.done' && e.input===call.input));
// Match Codex serialization: provider extension fields do not survive roundtrip.
const history = first.output.map(item=>{
  const {thought_signature,...retained}=item;
  return retained;
});
const second = await turn([...initial,...history,
  {type:'custom_tool_call_output',call_id:call.call_id,output:'TOOL_RESULT_OK'},
  {role:'user',content:'Reply with the exact tool result only.'},
],'none');
const answer = second.output.filter(i=>i.type==='message').flatMap(i=>i.content||[]).map(p=>p.text||'').join('');
assert.ok(answer.includes('TOOL_RESULT_OK'),'tool result was not consumed');
assert.ok(!second.output.some(i=>i.type.endsWith('_call')),'tool_choice none was ignored');
console.log('PASS: custom tool -> serialized history -> tool output -> final answer (no code executed)');
socket?.close();
