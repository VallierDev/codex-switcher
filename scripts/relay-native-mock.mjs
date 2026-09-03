// Isolated test upstream, accepts fixture keys only; never use real account data.
import http from 'node:http';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';
let upgrades=0;
const server=http.createServer(async(req,res)=>{
  try {
    const route=new URL(req.url,'http://localhost').pathname;
    if(route==='/__probe_counts'){res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({upgrades}));return;}
    if(req.method==='GET' && route==='/catalog/v1/models') {
      assert.equal(req.headers.authorization,'Bearer fixture-current-key');
      res.writeHead(200,{'content-type':'application/json'});
      res.end(JSON.stringify({models:[{slug:'gpt-fixture',display_name:'Unchanged fixture',base_instructions:'KEEP_ME',visibility:'list'}]}));return;
    }
    const provider=route==='/kimi2/v1/responses'?'kimi2':route==='/kimi/v1/responses'?'kimi':route==='/custom/deepseek/v1/responses'?'deepseek':null;
    assert.ok(provider,'unexpected upstream path');
    assert.equal(req.headers.authorization,`Bearer fixture-${provider}-key`);
    assert.equal(req.headers['chatgpt-account-id'],undefined);
    const chunks=[];for await(const chunk of req) chunks.push(chunk);
    const body=JSON.parse(Buffer.concat(chunks));
    assert.ok(provider.startsWith('kimi')?body.model==='kimi-k3':body.model.startsWith('deepseek-v4-'));
    const continued=Array.isArray(body.input)&&body.input.some(i=>i.type==='custom_tool_call_output');
    const output=continued ? [{id:'m',type:'message',role:'assistant',status:'completed',content:[{type:'output_text',text:'MOCK_TOOL_OK'}]}]
      : [{id:'c',type:'custom_tool_call',name:'exec',namespace:'functions',call_id:'mock-call',input:'text("MOCK_REQUEST")',status:'completed'}];
    res.writeHead(200,{'content-type':'text/event-stream'});
    for(const event of [{type:'response.created',response:{id:'mock-response',status:'in_progress',output:[]}},
      {type:'response.output_item.done',output_index:0,item:output[0]},
      {type:'response.completed',response:{id:'mock-response',status:'completed',model:body.model,output}}]) res.write(`data: ${JSON.stringify(event)}\n\n`);
    res.end(); console.log(JSON.stringify({provider,path:route,model:body.model,continued}));
  }catch(error){console.error(error.message);res.writeHead(400);res.end('mock assertion failed');}
});
server.on('upgrade',(req,socket)=>{
  upgrades++;
  const accept=crypto.createHash('sha1').update(req.headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  socket.on('error',()=>{});
});
server.listen(18090,'127.0.0.1',()=>console.log('Mock relay listening on 127.0.0.1:18090'));
