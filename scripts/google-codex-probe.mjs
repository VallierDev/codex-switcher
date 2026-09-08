// Isolated, ephemeral Codex CLI acceptance. No user threads or auth files changed.
import http from 'node:http';
import {spawn, execFileSync} from 'node:child_process';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
const workspace = await mkdtemp(path.join(tmpdir(),'switcher-codex-probe-'));
const upstream = Number(process.argv[2] || 18082);
const custom = process.env.GOOGLE_PROBE_CUSTOM === '1';
const model = process.argv[3] || 'gemini-3.8-flash-high';
const catalogArgs = [];
// Use exactly the metadata served by Switcher, scoped to this temporary CLI
// process. Never replace the user's model catalog or desktop configuration.
if (process.env.GOOGLE_PROBE_FETCH_CATALOG === '1') {
  const version = execFileSync('codex', ['--version'], {encoding:'utf8'}).match(/\d+\.\d+\.\d+/)?.[0];
  if (!version) throw new Error('Cannot determine Codex CLI version');
  const response = await fetch(`http://127.0.0.1:${upstream}/v1/models?client_version=${version}`, {signal:AbortSignal.timeout(30000)});
  if (!response.ok) throw new Error(`catalog HTTP ${response.status}`);
  const body = await response.json();
  const entry = (body.models || body.data || []).find(item => item.slug === model);
  if (!entry) throw new Error('Requested model missing from Switcher catalog');
  const catalogPath = path.join(workspace, 'probe-models.json');
  await writeFile(catalogPath, JSON.stringify({models:[entry]}), {mode:0o600});
  catalogArgs.push('-c', `model_catalog_json=${JSON.stringify(catalogPath)}`);
  console.log(JSON.stringify({catalogModel:entry.slug,searchTool:entry.supports_search_tool,
    efforts:entry.supported_reasoning_levels?.map(item=>item.effort)}));
}
let n = 0;
const server = http.createServer(async(req,res)=>{
  try {
    const chunks=[]; for await(const c of req) chunks.push(c);
    const body=Buffer.concat(chunks);
    if(body.length){const j=JSON.parse(body); console.log(JSON.stringify({request:++n,
      model:j.model,tools:j.tools?.map(t=>({type:t.type,name:t.name,children:t.tools?.map(c=>({type:c.type,name:c.name}))})),
      additionalTools:j.input?.filter?.(i=>i.type==='additional_tools').map(i=>({keys:Object.keys(i),tools:i.tools?.map(t=>({type:t.type,name:t.name,children:t.tools?.map(c=>({type:c.type,name:c.name}))}))})),
      inputTypes:j.input?.map?.(i=>i.type||i.role),previousResponse:!!j.previous_response_id}));}
    const r=await fetch(`http://127.0.0.1:${upstream}${req.url}`,{method:req.method,
      headers:{'Content-Type':'application/json'},body:body.length?body:undefined,signal:AbortSignal.timeout(60000)});
    res.writeHead(r.status,{'Content-Type':r.headers.get('content-type')||'text/event-stream'});
    for await(const c of r.body) res.write(c);
    res.end();
  }catch(e){res.writeHead(502);res.end(String(e));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const port=server.address().port;
const args=['exec','--ignore-user-config','--ignore-rules','--ephemeral','--skip-git-repo-check',
  '-C',workspace,'-s',custom?'workspace-write':'read-only','-m',model,...catalogArgs,
  '-c','model_provider="google_probe"',
  '-c',`model_providers.google_probe={name="Isolated Google test",base_url="http://127.0.0.1:${port}/v1",wire_api="responses",requires_openai_auth=false,stream_max_retries=0}`,
  '-c','model_reasoning_effort="low"','--json',
  custom ? '自定义工具回归测试：必须调用 apply_patch 工具在当前临时目录新建 probe.txt，内容仅为 TOOL_ROUNDTRIP_OK。然后用命令工具读取 probe.txt 核实内容，最后回复该内容。禁止访问网络、修改或读取当前临时目录以外的文件。不要用 shell 写文件。' :
  '工具回归测试：必须实际调用命令工具执行 pwd。禁止读取其他文件、网络访问或写入任何文件。执行完成后回复 TOOL_ROUNDTRIP_OK 和实际路径。'];
const child=spawn('codex',args,{env:{...process.env,CODEX_HOME:process.env.GOOGLE_PROBE_USE_USER_HOME === '1' ? (process.env.CODEX_HOME || path.join(process.env.HOME,'.codex')) : workspace,RUST_LOG:'error'},stdio:['ignore','pipe','pipe']});
let stdout='',stderr='';
child.stdout.on('data',d=>{stdout+=d;process.stdout.write(d);});
child.stderr.on('data',d=>{stderr+=d;});
const timer=setTimeout(()=>child.kill('SIGTERM'),120000);
const status=await new Promise(resolve=>child.on('exit',resolve)); clearTimeout(timer);
server.closeAllConnections(); await new Promise(resolve=>server.close(resolve));
const ok=status===0 && stdout.includes('TOOL_ROUNDTRIP_OK') && stdout.includes('command_execution') && (!custom || stdout.includes('file_change'));
console.log(JSON.stringify({status,passed:ok,requests:n,stderrTail:ok?'':stderr.slice(-1000)}));
process.exitCode=ok?0:1;
