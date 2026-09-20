// Real Chromium + preload + Rust Agent smoke in a disposable data root.
// Does not inspect or mutate an installed user's Harness data.
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, writeFile, mkdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const fixture = await mkdtemp(path.join(tmpdir(), 'nexus-electron-smoke-'));
const userData = path.join(fixture, 'desktop');
await mkdir(userData);
const env = { ...process.env, NEXUS_DATA_DIR: path.join(fixture, 'business'), NEXUS_AGENT_PORT: '0', NEXUS_LOCALE: 'en',
  DSH_HOME: path.join(fixture, 'dsh'), NEXUS_HARNESS_ROOT: path.join(fixture, 'harness') };
await mkdir(env.DSH_HOME); await mkdir(env.NEXUS_HARNESS_ROOT);
delete env.ELECTRON_RUN_AS_NODE;
const executable = process.env.NEXUS_SMOKE_EXECUTABLE || require('electron');
const launchedAt = performance.now();
const child = spawn(executable, [...(process.env.NEXUS_SMOKE_EXECUTABLE ? [] : [root]), `--user-data-dir=${userData}`, '--remote-debugging-port=0'], {
  cwd: root, env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true,
});
let stderr = '';
child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-12000); });
child.stdout.on('data', chunk => { stderr = (stderr + chunk).slice(-12000); });
child.on('error', e => { throw e; });
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn) {
  const deadline = Date.now() + 45000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`Electron exited ${child.exitCode}. ${stderr}`);
    const value = await fn(); if (value) return value; await delay(150);
  }
  throw new Error(`Smoke timeout. ${stderr}`);
}
let ws;
try {
  const port = await until(async () => {
    try { return (await readFile(path.join(userData, 'DevToolsActivePort'), 'utf8')).split('\n')[0]; } catch { return null; }
  });
  const page = await until(async () => (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find(p => p.type === 'page' && p.url.startsWith('file:')));
  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }); });
  let next = 0; const pending = new Map();
  ws.addEventListener('message', event => {
    const message = JSON.parse(event.data); const receiver = pending.get(message.id);
    if (receiver) { pending.delete(message.id); message.error ? receiver.reject(message.error) : receiver.resolve(message.result); }
  });
  const cdp = (method, params = {}) => new Promise((resolve, reject) => { const id = ++next; pending.set(id, { resolve, reject }); ws.send(JSON.stringify({ id, method, params })); });
  const evaluate = async expression => {
    const result = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  const ready = await until(async () => evaluate('window.nexusDesktop ? window.nexusDesktop.invoke("startup_status").then(s=>s.available?s:null) : null'));
  const agentReadyMs = Math.round(performance.now() - launchedAt);
  await until(() => evaluate('!!document.querySelector(".page-content")'));
  const workspaceReadyMs = Math.round(performance.now() - launchedAt);
  assert.equal(ready.data_root, path.join(fixture, 'business'));
  assert.equal(await evaluate('typeof require'), 'undefined');
  assert.equal(await evaluate('typeof process'), 'undefined');
  assert.equal(await evaluate('window.nexusDesktop.invoke("exec", {}).then(()=>false,()=>true)'), true);
  assert.equal(await evaluate('window.nexusDesktop.invoke("proxy_request", {method:"GET",path:"/etc/passwd"}).then(()=>false,()=>true)'), true);
  // The renderer also reads state during startup. Both reads reconcile recovery
  // under the lifecycle gate, so wait for the explicit transient busy response.
  const state = await until(() => evaluate('window.nexusDesktop.invoke("proxy_request", {method:"GET",path:"/v1/state"}).catch(error=>{if(error.code==="lifecycle_busy")return null;throw error})'));
  assert.equal(state.state.lifecycle, 'running');
  if (process.env.NEXUS_SMOKE_RECEIPTS === '1') {
    // Omit the tag deliberately: reach the real typed handler without starting
    // a network fetch, Harness installation, or version switch.
    const rejection = await evaluate('window.nexusDesktop.invoke("proxy_request", {method:"POST",path:"/v1/updates",body:{action:"switch",source:"official",mode:"portable",request_id:Math.floor(Date.now()/1000)+"-"+"1".repeat(32)}}).then(()=>null,error=>error)');
    assert.equal(rejection.status, 400);
    assert.equal(rejection.code, 'update_tag_required');
    assert.match(rejection.message, /tag is required/);
  }
  const nativeState = await evaluate('window.nexusDesktop.invoke("harness_desktop_status")');
  assert.equal(nativeState.phase, 'idle');
  assert.equal(await evaluate('typeof window.nexusShell'), 'undefined');
  const screenshot = await cdp('Page.captureScreenshot', { format: 'png' });
  await writeFile(path.join(fixture, 'launcher.png'), Buffer.from(screenshot.data, 'base64'));
  await cdp('Emulation.setDeviceMetricsOverride', { width: 680, height: 520, deviceScaleFactor: 1, mobile: false });
  await cdp('Page.bringToFront');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, 'minimum window width must not cause horizontal page overflow');
  const narrowScreenshot = await cdp('Page.captureScreenshot', { format: 'png' });
  await writeFile(path.join(fixture, 'launcher-narrow.png'), Buffer.from(narrowScreenshot.data, 'base64'));
  const report = { fixture, electron: await readFile(path.join(root, 'node_modules/electron/dist/version'), 'utf8'),
    timings: { agentReadyMs, workspaceReadyMs },
    checks: ['real renderer loaded unchanged React UI', 'sandboxed renderer has no Node globals', 'allowlist rejected arbitrary command and path', 'private Rust bridge started isolated Agent', 'authenticated business status returned', 'native Desktop status available without a replacement client shell'],
    screenshot: path.join(fixture, 'launcher.png') };
  if (process.env.NEXUS_SMOKE_CORRUPT_WORKSPACE === '1') {
    const catalog = path.join(env.NEXUS_DATA_DIR, 'profiles.json');
    const original = await readFile(catalog);
    const broken = '{\n"profiles": [1 2]\n}';
    try {
      await writeFile(catalog, broken);
      await evaluate('window.dispatchEvent(new Event("focus"))');
      await until(() => evaluate('!!document.querySelector(".workspace-repair") && document.querySelector(".workspace-repair").textContent.includes("profiles.json")'));
      const failure = await evaluate('window.nexusDesktop.invoke("proxy_request", {method:"GET",path:"/v1/profiles"}).then(()=>null,error=>error)');
      assert.equal(failure.kind, 'invalid_data');
      assert.match(failure.message, /line 2 column/);
      assert.equal(await readFile(catalog, 'utf8'), broken, 'reading a damaged catalog must not overwrite it');
      const repairScreenshot = await cdp('Page.captureScreenshot', { format: 'png' });
      await writeFile(path.join(fixture, 'workspace-repair.png'), Buffer.from(repairScreenshot.data, 'base64'));
    } finally { await writeFile(catalog, original); }
    await evaluate('window.dispatchEvent(new Event("focus"))');
    await until(() => evaluate('!document.querySelector(".workspace-repair")'));
    report.checks.push('damaged catalog exposes file and parser location, preserves bytes, and clears repair notice after restoration');
  }
  // Explicit test-owned Agent stop, not the automatic updater's shutdown path.
  await evaluate('window.nexusDesktop.invoke("proxy_request", {method:"POST",path:"/v1/agent",body:{action:"stop"}})');
  await writeFile(path.join(fixture, 'report.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report, null, 2));
  const exited = new Promise(resolve => child.once('exit', resolve));
  await Promise.race([cdp('Browser.close'), exited]);
} finally {
  ws?.close();
  // Only this smoke's Electron process may be terminated on test failure.
  if (child.exitCode === null) child.kill();
}
