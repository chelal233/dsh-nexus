import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { lstat, mkdir, open, readdir, readFile, readlink, statfs, writeFile } from 'node:fs/promises';
import path from 'node:path';

assert.equal(process.platform, 'linux');
assert.equal(process.arch, 'x64');
assert.notEqual(process.getuid(), 0);
assert.equal(process.env.GITHUB_ACTIONS, 'true');
const root = '/qa/test';
const business = process.env.NEXUS_QA_BUSINESS === '1';
const phase = process.env.NEXUS_QA_PHASE ?? 'fresh';
assert.ok(['fresh', 'old', 'final'].includes(phase));
const sharedDeadlineAt = Number(process.env.NEXUS_QA_DEADLINE_MS);
assert.ok(Number.isSafeInteger(sharedDeadlineAt) && sharedDeadlineAt > Date.now());
const evidence = business ? `/evidence/gui/${phase}` : '/evidence/gui';
const executable = '/opt/Nexus Launcher/nexus-launcher';
const logs = [];
const children = [];
const extraChannels = [];
const browserTargetEvents = [];
let lastOfficialOpen;
function urlIdentity(raw) {
  const sha256 = createHash('sha256').update(String(raw)).digest('hex');
  try {
    const url = new URL(raw);
    return { origin: url.origin, pathname: url.pathname, queryKeys: [...url.searchParams.keys()], sha256 };
  } catch { return { sha256, invalid: true }; }
}
async function browserInventory() {
  return {
    expected: lastOfficialOpen,
    targets: (await web.getPages()).slice(0, 100).map(page => ({ id: page.id, type: page.type, url: urlIdentity(page.url) })),
    events: browserTargetEvents.slice(-200).map(({ url, ...event }) => ({ ...event, url: urlIdentity(url) })),
  };
}
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
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout ${method}`)); }, method === 'Runtime.evaluate' ? 120000 : 5000);
    pending.set(id, { resolve: result => { clearTimeout(timer); resolve(result); }, reject: error => { clearTimeout(timer); reject(error); } });
    try { ws.send(JSON.stringify({ id, method, params })); }
    catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
  });
  return { ws, cdp };
}
async function connect(child, userData, type) {
  report.phase = `${type}: wait for DevToolsActivePort`;
  const port = await until(child, async () => {
    try {
      const candidate = (await readFile(path.join(userData, 'DevToolsActivePort'), 'utf8')).split('\n')[0];
      assert.match(candidate, /^\d+$/);
      // A reused QA profile can retain the previous closed process port file.
      const response = await fetch(`http://127.0.0.1:${candidate}/json/version`, { signal: AbortSignal.timeout(5000) });
      assert.equal(response.status, 200);
      return candidate;
    } catch (error) {
      if (error.code === 'ENOENT' || error.cause?.code === 'ECONNREFUSED') return null;
      throw error;
    }
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
  return { ...channel, host, evaluate, port, getPages: () => get('json/list') };
}
async function status(pid) {
  const text = await readFile(`/proc/${pid}/status`, 'utf8');
  const values = Object.fromEntries(text.split('\n').filter(line => line.includes(':')).map(line => {
    const i = line.indexOf(':'); return [line.slice(0, i), line.slice(i + 1).trim()];
  }));
  return Object.fromEntries(['Pid', 'Uid', 'CapEff', 'NoNewPrivs', 'Seccomp', 'Seccomp_filters'].map(k => [k, values[k]]));
}
async function x11(args) {
  const child = start('/usr/bin/xdotool', args);
  let output = '';
  child.stdout.on('data', data => { output = (output + data).slice(0, 16384); });
  await Promise.race([child.qaClose, delay(5000)]);
  assert.ok(child.qaClosed && child.exitCode === 0, 'Owned X11 QA input/query did not complete');
  return output.trim();
}
async function command(program, args, env = {}) {
  const child = start(program, args, env);
  await Promise.race([child.qaClose, delay(15000)]);
  assert.ok(child.qaClosed && child.exitCode === 0, `Owned QA command failed: ${program}`);
}
async function browserAssociation(home) {
  const env = { HOME: home, XDG_CONFIG_HOME: `${home}/.config`, XDG_DATA_HOME: `${home}/.local/share` };
  await mkdir(`${home}/.local/share/applications`, { recursive: true });
  await mkdir(`${home}/.config`, { recursive: true });
  await writeFile(`${home}/.local/share/applications/nexus-qa-browser.desktop`, `[Desktop Entry]\nType=Application\nName=Nexus QA Browser\nExec=${process.env.NEXUS_QA_BROWSER} --user-data-dir=${root}/browser %U\nNoDisplay=true\nMimeType=x-scheme-handler/http;x-scheme-handler/https;\n`);
  for (const scheme of ['http', 'https']) await command('/usr/bin/xdg-mime', ['default', 'nexus-qa-browser.desktop', `x-scheme-handler/${scheme}`], env);
}
async function launchInstance(spec) {
  assert.equal(spec.executable, executable);
  for (const value of [spec.dataRoot, spec.home, spec.dshHome, spec.userDataDir]) {
    assert.ok(value.startsWith(`${root}/`) && !(await lstat(value)).isSymbolicLink());
  }
  await browserAssociation(spec.home);
  const proc = start(executable, [`--user-data-dir=${spec.userDataDir}`, '--remote-debugging-port=0'], {
    HOME: spec.home, TMPDIR: `${root}/tmp`, XDG_CONFIG_HOME: `${spec.home}/.config`,
    XDG_CACHE_HOME: `${spec.home}/.cache`, XDG_DATA_HOME: `${spec.home}/.local/share`,
    NEXUS_DATA_DIR: spec.dataRoot, DSH_HOME: spec.dshHome, NEXUS_AGENT_PORT: '0', NEXUS_LOCALE: 'en',
  });
  const channel = await connect(proc, spec.userDataDir, 'electron');
  extraChannels.push(channel);
  return { proc, desktop: channel };
}
async function restartInstance({ instance, spec }) {
  assert.ok(children.includes(instance.proc) && instance.proc.qaProgram === executable);
  await instance.desktop.evaluate('window.nexusDesktop.invoke("proxy_request", {method:"POST",path:"/v1/agent",body:{action:"stop"}})');
  try { await instance.desktop.host.cdp('Browser.close'); } catch { /* Close may precede response. */ }
  instance.desktop.ws.close(); instance.desktop.host.ws.close();
  await Promise.race([instance.proc.qaClose, delay(15000)]);
  assert.ok(instance.proc.qaClosed && instance.proc.exitCode === 0, 'Normal QA instance restart requires confirmed prior GUI exit');
  return launchInstance(spec);
}
async function findOfficialPage({ url, openedAfter }) {
  assert.ok(Number.isFinite(openedAfter) && openedAfter <= Date.now());
  const expected = new URL(url);
  assert.equal(expected.protocol, 'http:');
  assert.ok(['127.0.0.1', 'localhost'].includes(expected.hostname));
  lastOfficialOpen = { openedAfter, url: urlIdentity(url) };
  await writeFile(`${evidence}/official-open-expected.json`, JSON.stringify(lastOfficialOpen, null, 2));
  let page;
  try { page = await until(browser, async () => (await web.getPages()).find(p => {
    if (p.type !== 'page') return false;
    try {
      return new URL(p.url).href === expected.href && browserTargetEvents.some(event =>
        event.targetId === p.id && event.url === p.url && event.time >= openedAfter);
    } catch { return false; }
  })); } catch (error) {
    await writeFile(`${evidence}/official-open-targets.json`, JSON.stringify(await browserInventory(), null, 2));
    const pages = (await web.getPages()).filter(item => {
      try { return item.type === 'page' && new URL(item.url).origin === expected.origin; }
      catch { return false; }
    }).slice(0, 3);
    for (const item of pages) {
      assert.match(item.id, /^[A-Za-z0-9_-]+$/);
      const channel = await socket(item.webSocketDebuggerUrl);
      try {
        const document = await channel.cdp('Runtime.evaluate', { expression: '({readyState:document.readyState,title:document.title,text:document.body?.innerText?.slice(0,16000)})', returnByValue: true });
        await writeFile(`${evidence}/official-page-${item.id}.json`, JSON.stringify({ id: item.id, url: urlIdentity(item.url), document: document.result?.value }, null, 2));
        const image = await channel.cdp('Page.captureScreenshot', { format: 'png' });
        await writeFile(`${evidence}/official-page-${item.id}.png`, Buffer.from(image.data, 'base64'));
      } finally { channel.ws.close(); }
    }
    throw error;
  }
  assert.match(page.id, /^[A-Za-z0-9_-]+$/, 'Official browser target ID must be safe for evidence naming');
  const event = browserTargetEvents.findLast(item => item.targetId === page.id && item.url === page.url && item.time >= openedAfter);
  await writeFile(`${evidence}/official-open-${page.id}.json`, JSON.stringify({ targetId: page.id, openedAfter, observedAt: event.time, event: event.method, urlSha256: createHash('sha256').update(url).digest('hex') }, null, 2));
  const channel = await socket(page.webSocketDebuggerUrl);
  extraChannels.push(channel);
  const evaluate = async expression => {
    const value = await channel.cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (value.exceptionDetails) throw new Error('Official browser renderer evaluation failed');
    return value.result.value;
  };
  return { page: { evaluate, cdp: channel.cdp }, targetId: page.id, method: 'existing-cdp-target' };
}
let exportPayloadBytes;
let sourceEnvironmentBytes;
const admittedStages = new Set();
const businessStages = new Set(['native_identity', 'fresh_official_fetch', 'normal_promote', 'synthetic_files', 'old_preserved_data', 'official_browser_session_and_plugin', 'normal_stop', 'full_runtime_export', 'launch_fresh_B', 'inspect_complete_archive', 'full_import_and_publication', 'B_real_session_and_plugin', 'B_stop_before_recovery', 'inflight_import_cancel', 'normal_B_restart', 'retry_full_import_same_B', 'recovered_B_real_session_and_plugin', 'recovered_B_stop', 'same_Agent_next_write']);
async function logicalBytes(directory) {
  const metadata = await lstat(directory);
  if (metadata.isSymbolicLink()) return 4096;
  if (metadata.isFile()) return Math.ceil(metadata.size / 4096) * 4096 + 4096;
  assert.ok(metadata.isDirectory(), 'Unexpected QA payload entry');
  let bytes = 4096;
  for (const entry of await readdir(directory)) bytes += await logicalBytes(path.join(directory, entry));
  return bytes;
}
async function budgetGate({ stage, dataRoot, dshHome }) {
  assert.ok(businessStages.has(stage), 'Unknown business QA stage');
  for (const directory of [dataRoot, dshHome]) {
    assert.ok(directory.startsWith(`${root}/`) && !(await lstat(directory)).isSymbolicLink(), 'Budget paths must be owned ordinary QA directories');
  }
  const lines = (await readFile('/evidence/storage-timeline.txt', 'utf8')).trim().split('\n');
  const [time, occupiedText] = lines.at(-1).split(' ');
  assert.ok(Date.now() - Date.parse(time) <= 15000, 'Host QA physical measurement is stale');
  const occupied = Number(occupiedText);
  assert.ok(Number.isSafeInteger(occupied) && occupied >= 0 && occupied <= 23 * 2 ** 30);
  const fs = await statfs(root);
  const free = fs.bavail * fs.bsize, floor = Math.max(20 * 2 ** 30, fs.blocks * fs.bsize / 10);
  let increment = 0;
  if (!admittedStages.has(stage)) {
    if (stage === 'fresh_official_fetch') increment = 8 * 2 ** 30;
    if (['full_runtime_export', 'full_import_and_publication', 'inflight_import_cancel', 'retry_full_import_same_B'].includes(stage)) {
      sourceEnvironmentBytes ??= await logicalBytes(`${root}/dsh`);
      // Include all source slots/runtime and the complete source environment,
      // rounding regular files and directory entries to 4 KiB allocation units.
      // Import additionally copies the receiver HOME to merged-environment and
      // overlays incoming environment files while the extracted payload exists.
      exportPayloadBytes ??= await logicalBytes(`${root}/business/releases`) + await logicalBytes('/opt/Nexus Launcher/resources/runtime') + sourceEnvironmentBytes;
      const receiverEnvironmentBytes = await logicalBytes(dshHome);
      increment = stage === 'full_runtime_export' ? 2 * exportPayloadBytes : exportPayloadBytes + receiverEnvironmentBytes + sourceEnvironmentBytes;
      increment += 512 * 1024 ** 2;
    }
    assert.ok(occupied + increment <= 23 * 2 ** 30 && free - increment >= floor + 2 ** 30, 'Measured QA phase estimate exceeds approved envelope');
    admittedStages.add(stage);
    await writeFile(`${evidence}/budget-${stage}.json`, JSON.stringify({ stage, dataRoot, dshHome, occupied, free, floor, increment, exportPayloadBytes, sourceEnvironmentBytes, limit: 24 * 2 ** 30, stopLine: 23 * 2 ** 30 }, null, 2));
  }
  assert.ok(free >= floor + 2 ** 30);
  return { ok: true, occupied, free, floor, increment };
}
let electron;
let browser;
let desktop;
let web;
async function failureEvidence() {
  // Only this disposable QA environment and children owned by this test.
  const diagnostic = { phase: report.phase, children: [], profiles: [] };
  const capture = start('/usr/bin/xwd', ['-root', '-silent', '-out', `${evidence}/failure-display.xwd`]);
  await Promise.race([capture.qaClose, delay(5000)]);
  if (!capture.qaClosed) capture.kill('SIGTERM');
  diagnostic.displayCapture = { pid: capture.pid, closed: capture.qaClosed, exitCode: capture.exitCode, signalCode: capture.signalCode };
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
  diagnostic.browserPolicies = [];
  const policyKeys = ['RemoteDebuggingAllowed', 'DeveloperToolsAvailability', 'UserDataDir'];
  for (const directory of ['/etc/chromium/policies/managed', '/etc/chromium/policies/recommended']) {
    let names;
    try { names = await readdir(directory); }
    catch (error) { diagnostic.browserPolicies.push({ directory, readError: error.code ?? error.message }); continue; }
    assert.ok(names.length <= 32, 'Unexpected policy inventory in disposable container');
    for (const name of names.filter(n => n.endsWith('.json'))) {
      const file = await open(path.join(directory, name), 'r');
      try {
        assert.ok((await file.stat()).size <= 32768, 'Oversized QA policy file');
        const value = JSON.parse(await file.readFile('utf8'));
        diagnostic.browserPolicies.push({ file: path.join(directory, name), configuredKeys: Object.keys(value), relevantValues: Object.fromEntries(policyKeys.filter(k => Object.hasOwn(value, k)).map(k => [k, value[k]])) });
      } finally { await file.close(); }
    }
  }
  try {
    const value = JSON.parse(await readFile(`${root}/browser/Local State`, 'utf8'));
    diagnostic.browserLocalStateDevTools = Object.fromEntries(['remote_debugging_allowed', 'remote_debugging_enabled', 'availability'].filter(k => Object.hasOwn(value.devtools ?? {}, k)).map(k => [k, value.devtools[k]]));
  } catch (error) { diagnostic.browserLocalStateReadError = error.code ?? error.message; }
  if (web) {
    try { diagnostic.browserInventory = await browserInventory(); }
    catch (error) { diagnostic.browserInventoryError = error.message; }
  }
  for (const [name, channel] of [['browser', web], ['electron', desktop]]) {
    if (!channel) continue;
    try {
      diagnostic[name] = await channel.evaluate('({url:location.href,readyState:document.readyState,title:document.title,text:document.body?.innerText?.slice(0,10000),sandboxRows:Array.from(document.querySelectorAll("#sandbox-status tr"),r=>r.innerText),evaluation:document.querySelector("#evaluation")?.innerText})');
      diagnostic[name].url = urlIdentity(diagnostic[name].url);
      const image = await channel.cdp('Page.captureScreenshot', { format: 'png' });
      await writeFile(`${evidence}/failure-${name}.png`, Buffer.from(image.data, 'base64'));
    } catch (error) { diagnostic[`${name}ReadError`] = error.message; }
  }
  await writeFile(`${evidence}/failure-state.json`, JSON.stringify(diagnostic, null, 2));
}
try {
  await mkdir(evidence, { recursive: true });
  for (const directory of ['electron', 'browser', 'business', 'dsh', 'harness']) await mkdir(path.join(root, directory), { recursive: true });
  report.executableSha256 = createHash('sha256').update(await readFile(executable)).digest('hex');
  if (business) {
    assert.equal(process.env.NEXUS_HARNESS_ROOT, undefined, 'Full first selection must use normal managed releases');
    const manifestBytes = await readFile('/opt/Nexus Launcher/resources/release-manifest.json');
    const manifest = JSON.parse(manifestBytes);
    const expectedCommit = phase === 'old' ? '81ae28ed65df7630f3b1aa3b5e0215383218341d' : 'c34b51ccfe5467e76340e6e3bc1201d304cadde7';
    const expectedRun = phase === 'old' ? 36832892260 : 36834335424;
    assert.equal(manifest.commit, expectedCommit);
    assert.equal(manifest.buildId, `electron-${expectedRun}-1-x86_64-unknown-linux-gnu`);
    report.installedIdentity = { commit: manifest.commit, buildId: manifest.buildId, manifestSha256: createHash('sha256').update(manifestBytes).digest('hex') };
  }
  electron = start(executable, [`--user-data-dir=${root}/electron`, '--remote-debugging-port=0'], {
    NEXUS_DATA_DIR: `${root}/business`, DSH_HOME: `${root}/dsh`, ...(!business && { NEXUS_HARNESS_ROOT: `${root}/harness` }),
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
  browser = start(process.env.NEXUS_QA_BROWSER, [`--user-data-dir=${root}/browser`, '--remote-debugging-port=0', '--enable-logging=stderr', `file://${root}/browser-fixture.html`]);
  report.phase = 'browser: visible first-run confirmation';
  await delay(2000);
  try {
    await readFile(`${root}/browser/DevToolsActivePort`, 'utf8');
  } catch (error) {
    assert.equal(error.code, 'ENOENT');
    // Fedora's unbranded first-run dialog blocks DevTools initialization.
    // Use its normal focused Accept action; never suppress first-run or policy.
    const windows = (await x11(['search', '--onlyvisible', '--pid', String(browser.pid)])).split(/\s+/);
    assert.equal(windows.length, 1, 'First-run input requires one visible window owned by this browser');
    assert.match(windows[0], /^\d+$/);
    const title = await x11(['getwindowname', windows[0]]);
    assert.equal(title, 'Chromium Additional Terms of Service', 'Do not accept an unidentified dialog');
    const localePath = '/usr/lib64/chromium-browser/locales/en-US.pak';
    const localeFile = await open(localePath, 'r');
    let locale;
    try {
      assert.ok((await localeFile.stat()).size <= 8 * 1024 * 1024);
      locale = await localeFile.readFile();
    } finally { await localeFile.close(); }
    const placeholder = 'This Space Intentionally Blank\n\nIn official builds this space will show the terms of service.';
    assert.ok(locale.includes(Buffer.from(placeholder)), 'Installed unbranded terms resource must match the verified empty placeholder');
    await x11(['windowfocus', '--sync', windows[0]]);
    assert.equal(await x11(['getwindowpid', windows[0]]), String(browser.pid));
    assert.equal(await x11(['getwindowname', windows[0]]), title);
    const geometry = Object.fromEntries((await x11(['getwindowgeometry', '--shell', windows[0]])).split('\n').map(line => line.split('=')));
    const gx = Number(geometry.X), gy = Number(geometry.Y), gw = Number(geometry.WIDTH), gh = Number(geometry.HEIGHT);
    assert.ok([gx, gy, gw, gh].every(Number.isInteger));
    assert.ok(gx >= 0 && gy >= 0 && gw > 0 && gh > 0 && gx + gw <= 1280 && gy + gh <= 800, 'Whole first-run dialog must be visible within the enlarged display');
    const before = start('/usr/bin/xwd', ['-id', windows[0], '-silent', '-out', `${evidence}/browser-first-run.xwd`]);
    await Promise.race([before.qaClose, delay(5000)]);
    assert.ok(before.qaClosed && before.exitCode === 0, 'First-run screenshot must complete before input');
    const display = await readFile(`${evidence}/browser-first-run.xwd`);
    assert.ok(display.length <= 8 * 1024 ** 2);
    const header = Array.from({ length: 25 }, (_, i) => display.readUInt32BE(i * 4));
    assert.equal(header[1], 7); assert.equal(header[2], 2); assert.equal(header[3], 24);
    assert.equal(header[4], gw); assert.equal(header[5], gh);
    assert.equal(header[6], 0); assert.equal(header[7], 0); assert.equal(header[11], 32);
    assert.deepEqual(header.slice(14, 17), [16711680, 65280, 255]);
    const offset = header[0] + header[19] * 12;
    assert.equal(offset + header[12] * header[5], display.length);
    // Hash the complete owned window, including every line and both buttons.
    // Never reuse a screen-clipped baseline or unrelated background pixels.
    const rgb = Buffer.alloc(gw * gh * 3);
    for (let y = 0; y < gh; y++) for (let x = 0; x < gw; x++) {
      const i = offset + y * header[12] + x * 4;
      const j = (y * gw + x) * 3;
      rgb[j] = display[i + 2]; rgb[j + 1] = display[i + 1]; rgb[j + 2] = display[i];
    }
    const pixelsSha256 = createHash('sha256').update(rgb).digest('hex');
    report.browserFirstRun = { windowId: windows[0], browserPid: browser.pid, title, geometry, placeholder, localePath, localeSha256: createHash('sha256').update(locale).digest('hex'), pixelsSha256, wholeWindowSha256: createHash('sha256').update(display).digest('hex'), action: 'NOT RUN: complete owned-window baseline needs review' };
    assert.equal(gw, 600); assert.equal(gh, 510);
    assert.equal(report.browserFirstRun.localeSha256, '50395219c8bf086711f03846ad530c5e01d8f07cc3da0ae3450b838629d9dfba');
    assert.equal(pixelsSha256, '33965247b87d24598ffea070cbc0116fb391562a7c91eb89f447333b24c4f1f9', 'Complete first-run window must exactly match the reviewed empty-placeholder dialog before input');
    report.browserFirstRun.sourceEvidenceRunId = 37134369402;
    report.browserFirstRun.sourceWindowSha256 = '820aea06e6f9b04adbd638ab84436cf80d62b46181b0a5624a116a3ecdb79c3c';
    report.browserFirstRun.action = 'normal Return key after exact reviewed complete-window match';
    await x11(['key', '--window', windows[0], 'Return']);
  }
  web = await connect(browser, `${root}/browser`, 'browser');
  report.phase = 'browser: fixture text';
  assert.equal(await web.evaluate('document.querySelector("#marker")?.textContent'), 'nexus-qa-browser-preflight');
  await web.cdp('Page.navigate', { url: 'chrome://sandbox' });
  report.phase = 'browser: sandbox page';
  const sandbox = await until(browser, () => web.evaluate('/Seccomp.BPF sandbox\\s+(Yes|No)/i.test(document.body?.innerText ?? "") ? document.body.innerText : null'));
  await writeFile(`${evidence}/browser-sandbox.txt`, sandbox);
  assert.match(sandbox, /Seccomp.BPF sandbox\s+Yes/i);
  report.checks.push('ordinary non-root Chromium rendered prerequisite fixture with Seccomp-BPF sandbox enabled; no security-disabling arguments');
  if (business) {
    report.scope = 'Frozen original RPM fresh installation, same-version package replacement and full synthetic runtime migration';
    web.host.ws.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (['Target.targetCreated', 'Target.targetInfoChanged'].includes(message.method)) {
        const info = message.params.targetInfo;
        browserTargetEvents.push({ time: Date.now(), method: message.method, targetId: info.targetId, url: info.url });
        if (browserTargetEvents.length > 1000) browserTargetEvents.shift();
      }
    });
    await web.host.cdp('Target.setDiscoverTargets', { discover: true });
    const { runBusinessQA } = await import('./linux-rpm-business.mjs');
    await runBusinessQA({ phase, root, evidence, report, desktop, browser, web, electron,
      expectedIdentity: { commit: report.installedIdentity.commit, buildId: report.installedIdentity.buildId },
      // The shared deadline starts before DNF and leaves two minutes for owned
      // cleanup, five more before the host command deadline and upload time.
      deadlineAt: Math.min(sharedDeadlineAt - 120000, Date.now() + (phase === 'final' ? 65 : 20) * 60 * 1000),
      budgetGate, findOfficialPage, launchInstance, restartInstance });
    assert.equal(report.businessQA?.status, 'PASS');
    report.harnessFirstSelection = phase === 'fresh' ? 'PASS' : 'Previously selected release preserved';
    report.actualHarnessBrowserSessionAndPluginActivation = 'PASS synthetic session and Cordis fixture';
    report.dataPreservingPackageTransaction = phase === 'fresh' ? 'Fresh install; transaction verified externally' : 'PASS preserved QA data; DNF transaction verified externally';
  }
  report.result = 'PASS prerequisites only';
  if (business) report.result = `PASS business ${phase}`;
  report.phase = business ? `business ${phase} complete` : 'prerequisites complete';
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
  for (const channel of [web, desktop, ...extraChannels]) {
    try { await channel?.host.cdp('Browser.close'); } catch { /* Close event can precede the response. */ }
    channel?.ws.close();
    channel?.host?.ws.close();
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
