import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, open, readdir, readFile, readlink, writeFile } from 'node:fs/promises';
import path from 'node:path';

assert.equal(process.platform, 'linux');
assert.equal(process.arch, 'x64');
assert.notEqual(process.getuid(), 0);
assert.equal(process.env.GITHUB_ACTIONS, 'true');
const root = '/qa/test';
const evidence = '/evidence/gui';
const executable = '/opt/Nexus Launcher/nexus-launcher';
const logs = [];
const children = [];
const report = {
  scope: 'Fedora container native RPM installation and ordinary non-root GUI/sandbox prerequisites only',
  result: 'FAIL',
  harnessFirstSelection: 'NOT RUN',
  dataPreservingPackageTransaction: 'NOT RUN',
  actualHarnessBrowserSessionAndPluginActivation: 'NOT RUN',
  uid: process.getuid(),
  node: process.versions,
  checks: [],
  phase: 'create isolated directories',
};
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
function start(program, args, env = {}) {
  assert.ok(args.every(arg => !/no-sandbox|disable-setuid-sandbox|disable-web-security/.test(arg)));
  const child = spawn(program, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
  child.qaProgram = program;
  child.qaArgs = args;
  child.qaClosed = false;
  child.qaClose = new Promise(resolve => child.once('close', () => { child.qaClosed = true; resolve(); }));
  children.push(child);
  child.on('error', error => logs.push(error.message));
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => {
    logs.push(data.toString().replace(/(https?:\/\/[^\s?#]+)[?#][^\s]*/g, '$1?<redacted>'));
    while (logs.join('').length > 32000) logs.shift();
  });
  return child;
}
async function until(child, fn) {
  const end = Date.now() + 60000;
  while (Date.now() < end) {
    if (child.exitCode !== null || child.signalCode !== null) throw new Error(`GUI exited ${child.exitCode ?? child.signalCode}`);
    const value = await fn();
    if (value) return value;
    await delay(200);
  }
  throw new Error('Ordinary GUI readiness timeout');
}
async function socket(url) {
  const address = new URL(url);
  assert.equal(address.protocol, 'ws:');
  assert.equal(address.hostname, '127.0.0.1');
  const ws = new WebSocket(url);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { ws.close(); reject(new Error('Local CDP connection timeout')); }, 5000);
    ws.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
    ws.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Local CDP connection failed')); }, { once: true });
  });
  let next = 0;
  const pending = new Map();
  ws.addEventListener('message', event => {
    const msg = JSON.parse(event.data);
    const p = pending.get(msg.id);
    if (p) { pending.delete(msg.id); msg.error ? p.reject(new Error(JSON.stringify(msg.error))) : p.resolve(msg.result); }
  });
  ws.addEventListener('close', () => {
    for (const p of pending.values()) p.reject(new Error('Local CDP connection closed'));
    pending.clear();
  });
  const cdp = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout ${method}`)); }, 5000);
    pending.set(id, { resolve: result => { clearTimeout(timer); resolve(result); }, reject: error => { clearTimeout(timer); reject(error); } });
    try { ws.send(JSON.stringify({ id, method, params })); }
    catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
  });
  return { ws, cdp };
}
async function connect(child, userData, type) {
  report.phase = `${type}: wait for DevToolsActivePort`;
  const port = await until(child, async () => {
    try { return (await readFile(path.join(userData, 'DevToolsActivePort'), 'utf8')).split('\n')[0]; } catch { return null; }
  });
  assert.match(port, /^\d+$/);
  const get = async endpoint => {
    const response = await fetch(`http://127.0.0.1:${port}/${endpoint}`, { signal: AbortSignal.timeout(5000) });
    assert.equal(response.status, 200);
    return response.json();
  };
  report.phase = `${type}: find expected page`;
  const page = await until(child, async () => (await get('json/list')).find(p => p.type === 'page' && (type === 'electron' ? p.url.startsWith('file:') : p.url.includes('browser-fixture.html'))));
  const channel = await socket(page.webSocketDebuggerUrl);
  report.phase = `${type}: connect browser CDP`;
  let host;
  try { host = await socket((await get('json/version')).webSocketDebuggerUrl); }
  catch (error) { channel.ws.close(); throw error; }
  const { cdp } = channel;
  const evaluate = async expression => {
    const result = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) throw new Error('Renderer evaluation failed');
    return result.result.value;
  };
  return { ...channel, host, evaluate };
}
async function status(pid) {
  const text = await readFile(`/proc/${pid}/status`, 'utf8');
  const values = Object.fromEntries(text.split('\n').filter(line => line.includes(':')).map(line => {
    const i = line.indexOf(':'); return [line.slice(0, i), line.slice(i + 1).trim()];
  }));
  return Object.fromEntries(['Pid', 'Uid', 'CapEff', 'NoNewPrivs', 'Seccomp', 'Seccomp_filters'].map(k => [k, values[k]]));
}
let electron;
let browser;
let desktop;
let web;
async function failureEvidence() {
  // Only this disposable QA environment and children owned by this test.
  const diagnostic = { phase: report.phase, children: [], profiles: [] };
  for (const child of children) {
    const entry = { pid: child.pid, program: child.qaProgram, args: child.qaArgs, exitCode: child.exitCode, signalCode: child.signalCode };
    try {
      entry.status = await status(child.pid);
      entry.cmdline = (await readFile(`/proc/${child.pid}/cmdline`, 'utf8')).split('\0').filter(Boolean);
      entry.exe = await readlink(`/proc/${child.pid}/exe`);
    } catch (error) { entry.processReadError = error.code ?? error.message; }
    diagnostic.children.push(entry);
  }
  for (const directory of [path.join(root, 'browser'), path.join(root, 'home', '.config', 'chromium')]) {
    const entry = { directory };
    try { entry.names = (await readdir(directory)).sort().slice(0, 40); }
    catch (error) { entry.directoryReadError = error.code ?? error.message; }
    try { entry.devToolsActivePort = (await readFile(`${directory}/DevToolsActivePort`, 'utf8')).slice(0, 1024); }
    catch (error) { entry.portReadError = error.code ?? error.message; }
    diagnostic.profiles.push(entry);
  }
  if (process.env.NEXUS_QA_BROWSER?.startsWith('/usr/')) {
    try {
      const file = await open(process.env.NEXUS_QA_BROWSER, 'r');
      try {
        const { size } = await file.stat();
        const buffer = Buffer.alloc(16384);
        const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
        const contents = buffer.subarray(0, bytesRead);
        diagnostic.browserCommand = { path: process.env.NEXUS_QA_BROWSER, bytes: size };
        if (size <= buffer.length) diagnostic.browserCommand.sha256 = createHash('sha256').update(contents).digest('hex');
        if (contents.subarray(0, 2).toString() === '#!') diagnostic.browserCommand.wrapper = contents.toString('utf8');
      } finally { await file.close(); }
    } catch (error) { diagnostic.browserCommandReadError = error.code ?? error.message; }
  }
  for (const [name, channel] of [['browser', web], ['electron', desktop]]) {
    if (!channel) continue;
    try {
      diagnostic[name] = await channel.evaluate('({url:location.href,readyState:document.readyState,title:document.title,text:document.body?.innerText?.slice(0,10000),sandboxRows:Array.from(document.querySelectorAll("#sandbox-status tr"),r=>r.innerText),evaluation:document.querySelector("#evaluation")?.innerText})');
      const image = await channel.cdp('Page.captureScreenshot', { format: 'png' });
      await writeFile(`${evidence}/failure-${name}.png`, Buffer.from(image.data, 'base64'));
    } catch (error) { diagnostic[`${name}ReadError`] = error.message; }
  }
  await writeFile(`${evidence}/failure-state.json`, JSON.stringify(diagnostic, null, 2));
}
try {
  for (const directory of ['electron', 'browser', 'business', 'dsh', 'harness']) await mkdir(path.join(root, directory), { recursive: true });
  report.executableSha256 = createHash('sha256').update(await readFile(executable)).digest('hex');
  electron = start(executable, [`--user-data-dir=${root}/electron`, '--remote-debugging-port=0'], {
    NEXUS_DATA_DIR: `${root}/business`, DSH_HOME: `${root}/dsh`, NEXUS_HARNESS_ROOT: `${root}/harness`,
    NEXUS_AGENT_PORT: '0', NEXUS_LOCALE: 'en',
  });
  desktop = await connect(electron, `${root}/electron`, 'electron');
  report.phase = 'electron: real Agent startup status';
  const startup = await until(electron, () => desktop.evaluate('window.nexusDesktop ? window.nexusDesktop.invoke("startup_status").then(s=>s.available?s:null) : null'));
  assert.equal(startup.data_root, `${root}/business`);
  assert.equal(await desktop.evaluate('typeof require'), 'undefined');
  assert.equal(await desktop.evaluate('typeof process'), 'undefined');
  const baseline = await status(process.pid);
  report.phase = 'electron: renderer sandbox';
  const { processInfo } = await desktop.host.cdp('SystemInfo.getProcessInfo');
  const browserProcess = processInfo.find(p => p.type === 'browser');
  assert.equal(browserProcess?.id, electron.pid);
  assert.equal(await readlink(`/proc/${browserProcess.id}/exe`), executable);
  const renderers = processInfo.filter(p => p.type === 'renderer');
  assert.ok(renderers.length > 0);
  report.electronSandbox = { containerBaseline: baseline, browserPid: browserProcess.id, renderers: [] };
  for (const renderer of renderers) {
    assert.ok(Number.isSafeInteger(renderer.id) && renderer.id > 0);
    const actual = await status(renderer.id);
    assert.equal(await readlink(`/proc/${renderer.id}/exe`), executable);
    assert.ok(actual.Uid.split(/\s+/).every(id => Number(id) === process.getuid()));
    assert.equal(BigInt(`0x${actual.CapEff}`), 0n);
    assert.equal(actual.NoNewPrivs, '1');
    assert.equal(actual.Seccomp, '2');
    assert.ok(Number.isInteger(Number(baseline.Seccomp_filters)));
    assert.ok(Number(actual.Seccomp_filters) > Number(baseline.Seccomp_filters), 'Renderer must add its own filter beyond container baseline');
    report.electronSandbox.renderers.push(actual);
  }
  await until(electron, () => desktop.evaluate('!!document.querySelector(".page-content")'));
  const image = await desktop.cdp('Page.captureScreenshot', { format: 'png' });
  await writeFile(`${evidence}/installed-electron.png`, Buffer.from(image.data, 'base64'));
  report.checks.push('original RPM-owned installed Electron reached real renderer and private Agent bridge as non-root; Node globals absent');
  report.checks.push('CDP-identified installed Electron renderers have zero effective capabilities, NoNewPrivs and additional Seccomp-BPF filters beyond the container baseline');
  await writeFile(`${root}/browser-fixture.html`, '<!doctype html><title>Normal browser prerequisite</title><p id="marker">nexus-qa-browser-preflight</p>');
  assert.ok(process.env.NEXUS_QA_BROWSER?.startsWith('/usr/'));
  browser = start(process.env.NEXUS_QA_BROWSER, [`--user-data-dir=${root}/browser`, '--remote-debugging-port=0', `file://${root}/browser-fixture.html`]);
  web = await connect(browser, `${root}/browser`, 'browser');
  report.phase = 'browser: fixture text';
  assert.equal(await web.evaluate('document.querySelector("#marker")?.textContent'), 'nexus-qa-browser-preflight');
  await web.cdp('Page.navigate', { url: 'chrome://sandbox' });
  report.phase = 'browser: sandbox page';
  const sandbox = await until(browser, () => web.evaluate('document.body?.innerText?.includes("Sandbox") ? document.body.innerText : null'));
  await writeFile(`${evidence}/browser-sandbox.txt`, sandbox);
  assert.match(sandbox, /Seccomp.BPF sandbox\s+Yes/i);
  report.checks.push('ordinary non-root Chromium rendered prerequisite fixture with Seccomp-BPF sandbox enabled; no security-disabling arguments');
  report.result = 'PASS prerequisites only';
  report.phase = 'prerequisites complete';
} catch (error) {
  report.error = error.message;
  process.exitCode = 1;
  try { await failureEvidence(); }
  catch (diagnosticError) { report.diagnosticError = diagnosticError.message; }
} finally {
  if (desktop && electron.exitCode === null && electron.signalCode === null) {
    try { await desktop.evaluate('window.nexusDesktop.invoke("proxy_request", {method:"POST",path:"/v1/agent",body:{action:"stop"}})'); report.agentStop = 'official stop request returned; process exit checked separately by container cleanup'; }
    catch { report.agentStop = 'official stop unavailable; full container cleanup required'; }
  }
  for (const channel of [web, desktop]) {
    try { await channel?.host.cdp('Browser.close'); } catch { /* Close event can precede the response. */ }
    channel?.ws.close();
    channel?.host.ws.close();
  }
  for (const child of children) {
    if (!child.qaClosed && child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    await Promise.race([child.qaClose, delay(5000)]);
    if (!child.qaClosed) {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await Promise.race([child.qaClose, delay(10000)]);
    }
  }
  report.children = children.map(c => ({ pid: c.pid, program: c.qaProgram, args: c.qaArgs, closed: c.qaClosed, exitCode: c.exitCode, signalCode: c.signalCode }));
  if (children.some(c => !c.qaClosed)) {
    report.result = 'FAIL';
    report.cleanupError = 'Owned GUI process did not confirm close after bounded TERM/KILL waits';
    process.exitCode = 1;
  }
  await writeFile(`${evidence}/gui-preflight.json`, JSON.stringify(report, null, 2));
  await writeFile(`${evidence}/gui-preflight.log`, logs.join(''));
  console.log(JSON.stringify({ result: report.result, error: report.error, scope: report.scope }));
}
