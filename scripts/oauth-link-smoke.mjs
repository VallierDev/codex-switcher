import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { createServer } from 'vite';

const browser = process.env.BROWSER_PATH || [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/usr/bin/chromium', '/usr/bin/google-chrome',
].find(file => fs.existsSync(file));
assert.ok(browser, 'Set BROWSER_PATH to a Chromium-compatible browser');
const output = path.resolve('.oauth-smoke.local');
fs.mkdirSync(output, { recursive: true });
const profile = fs.mkdtempSync(path.join(output, 'profile-'));
const fixture = path.resolve('scripts/fixtures/oauth-ipc.mjs');
const server = await createServer({
  logLevel: 'error', optimizeDeps: { entries: ['scripts/fixtures/oauth-link.html'] },
  server: { host: '127.0.0.1', port: 0, strictPort: false, open: false, watch: { ignored: ['**/*.local/**'] } },
  plugins: [{ name: 'oauth-link-fixture', enforce: 'pre', resolveId(id) { return id.startsWith('@tauri-apps/') ? fixture : undefined; } }],
});
let child;
let socket;
let sequence = 0;
const pending = new Map();
const errors = [];
const scenes = [];
try {
  await server.listen();
  const base = server.resolvedUrls.local[0] + 'scripts/fixtures/oauth-link.html';
  child = spawn(browser, ['--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--disable-extensions', '--disable-background-networking', '--remote-debugging-port=0',
    `--user-data-dir=${profile}`, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  child.on('error', error => errors.push(String(error)));
  child.stderr.on('data', () => {});
  const portFile = path.join(profile, 'DevToolsActivePort');
  for (let i = 0; !fs.existsSync(portFile) && i < 100; i++) await delay(100);
  assert.ok(fs.existsSync(portFile), 'Isolated browser did not start');
  const port = fs.readFileSync(portFile, 'utf8').split('\n')[0];
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  socket = new WebSocket(targets.find(target => target.type === 'page').webSocketDebuggerUrl);
  await once(socket, 'open');
  socket.addEventListener('message', event => {
    const data = JSON.parse(event.data);
    if (data.id) {
      const task = pending.get(data.id); pending.delete(data.id);
      if (data.error) task?.reject(new Error(JSON.stringify(data.error)));
      else task?.resolve(data.result);
    } else if (data.method === 'Runtime.exceptionThrown') errors.push(data.params.exceptionDetails.exception?.description ?? data.params.exceptionDetails.text);
  });
  function call(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 20000);
      pending.set(id, { resolve: value => { clearTimeout(timer); resolve(value); }, reject: error => { clearTimeout(timer); reject(error); } });
      socket.send(JSON.stringify({ id, method, params }));
    });
  }
  async function evaluate(expression) {
    const result = await call('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
    return result.result.value;
  }
  async function waitFor(expression) {
    for (let i = 0; i < 100; i++) { if (await evaluate(expression)) return; await delay(100); }
    throw new Error(`Condition not met: ${expression}`);
  }
  async function click(selector) {
    await evaluate(`(() => {const node=document.querySelector(${JSON.stringify(selector)});if(!node)throw Error('Missing '+${JSON.stringify(selector)});node.click();})()`);
  }
  async function capture(name) {
    await delay(150);
    const state = await evaluate(`({ denied: window.__compat.denied, text: document.body.innerText,
      overflow: ['html','.modal-content','.modal-body','.oauth-content','.oauth-status','.oauth-link-panel'].filter(selector=>{
        const e=document.querySelector(selector);return e && e.clientWidth>0 && e.scrollWidth>e.clientWidth+2;
      }) })`);
    assert.deepEqual(state.denied, [], `${name}: unmocked operation`);
    assert.deepEqual(state.overflow, [], `${name}: horizontal overflow`);
    const image = await call('Page.captureScreenshot', { format: 'png' });
    fs.writeFileSync(path.join(output, name + '.png'), Buffer.from(image.data, 'base64'));
    scenes.push(name);
  }
  async function navigate(query = '') {
    await call('Page.navigate', { url: base + query });
    await waitFor('!!window.__compat && !!document.querySelector("#root > *")');
  }
  async function metrics(width, height, scale = 1) {
    await call('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: scale, mobile: false });
  }
  async function prepareCopy(failure) {
    await evaluate(`window.__compat.copyFailure=${failure}`);
    await click('.oauth-content > button[title]');
    await waitFor('!!document.querySelector(".oauth-link-value") && !document.querySelector(".oauth-link-panel button").disabled');
    assert.equal(await evaluate('document.querySelector(".oauth-link-value").value === window.__compat.url'), true);
  }
  await call('Runtime.enable'); await call('Page.enable'); await call('Network.enable');
  await call('Network.setBlockedURLs', { urls: ['https://*', 'http://*.invalid/*', 'http://*.com/*', 'http://*.cn/*'] });
  await call('Emulation.setUserAgentOverride', { userAgent: 'Mozilla/5.0 Windows compatibility fixture', platform: 'Win32' });
  await metrics(1200, 760);
  await navigate();
  await waitFor('!!document.querySelector(".oauth-content > button[title]")');
  await prepareCopy(true);
  await capture('oauth-copy-failure');
  await evaluate('window.__compat.copyFailure=false');
  await click('.oauth-link-panel button');
  await waitFor('window.__compat.clipboard === window.__compat.url');
  assert.equal(await evaluate('window.__compat.calls.filter(c=>c.command==="start_oauth_login").length'), 1, 'Copy retry must not rotate PKCE/state');
  await capture('oauth-copy-retry');
  await click('.oauth-content > button:last-child');
  await waitFor('!!document.querySelector(".oauth-content textarea.text-input")');
  assert.equal(await evaluate('document.querySelector(".oauth-content textarea.text-input").value'), '', 'Authorization URL must not become callback input');
  for (const [width, height, scale] of [[960, 640, 1.25], [800, 600, 1.5], [600, 500, 2]]) {
    await metrics(width, height, scale);
    await capture(`oauth-layout-${scale}`);
  }
  await metrics(1200, 760);
  await click('.close-btn'); await waitFor('!document.querySelector(".modal-overlay")');
  await click('#fixture-open'); await waitFor('!!document.querySelector(".modal-overlay")');
  await evaluate('window.__compat.loginDelay=600');
  await click('.oauth-content > button[title]');
  await click('.close-btn'); await click('#fixture-open'); await delay(900);
  assert.equal(await evaluate('!!document.querySelector(".oauth-link-value")'), false, 'Closed login attempt updated reopened modal');
  await capture('oauth-close-reopen');
  await evaluate('window.__compat.loginDelay=0; document.querySelectorAll(".tab-item")[2].click()');
  await prepareCopy(true);
  await capture('google-copy-failure');
  await evaluate('window.__compat.copyFailure=false; window.__compat.clipboard=null');
  await click('.oauth-link-panel button');
  await waitFor('window.__compat.clipboard===window.__compat.url');
  assert.equal(await evaluate('window.__compat.calls.filter(c=>c.command==="start_antigravity_oauth_login").length'), 1);
  await capture('google-copy-retry');
  assert.deepEqual(errors, [], 'Unhandled browser exception');
  console.log(`OAuth link regression: ${scenes.length} scenes passed; native IPC and account data mocked.`);
} finally {
  fs.writeFileSync(path.join(output, 'result.json'), JSON.stringify({ scenes, errors }, null, 2));
  if (socket?.readyState === WebSocket.OPEN) { socket.send(JSON.stringify({ id: ++sequence, method: 'Browser.close' })); await delay(500); }
  socket?.close();
  if (child && child.exitCode === null) { child.kill(); await Promise.race([once(child, 'exit'), delay(5000)]); }
  await server.close();
  try { fs.rmSync(profile, { recursive: true, force: true }); } catch { /* Browser shutdown can briefly hold the isolated profile. */ }
}
