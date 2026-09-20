// Local release publication -> real NSIS upgrade -> relaunched application.
// Test-only app identity, feed, data directories, and bootstrap live in artifacts.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createReadStream } from 'node:fs';
import { mkdir, mkdtemp, readFile, writeFile, stat, readdir, open } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

assert.equal(process.platform, 'win32', 'This acceptance runner installs Windows NSIS fixtures');
const require = createRequire(import.meta.url);
const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const outputRoot = path.join(appRoot, 'electron-dist');
await mkdir(outputRoot, { recursive: true });
const fixture = await mkdtemp(path.join(outputRoot, 'update-e2e-'));
const suffix = path.basename(fixture).replaceAll('-', '').toLowerCase();
const name = `nexus-${suffix}`;
const productName = `Nexus Update Verification ${suffix}`;
const executableName = 'Nexus Update Verification';
const installDirectory = path.join(fixture, 'installed');
const executable = path.join(installDirectory, `${executableName}.exe`);
const desktop = path.join(fixture, 'desktop');
const business = path.join(fixture, 'business');
const telemetry = path.join(fixture, 'running.json');
const requests = [];
const assets = new Map();
let published;
let connected;
let launched;
let installed = false;
let interruptDownload = false;
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const progress = message => console.log(`[update-e2e] ${message}`);
async function until(label, predicate, timeout = 60000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await predicate(); if (value) return value;
    await delay(150);
  }
  throw new Error(`Timed out: ${label}`);
}
async function command(program, args, logName, timeout = 900000) {
  const log = await open(path.join(fixture, logName), 'a');
  try {
    await new Promise((resolve, reject) => {
      const child = spawn(program, args, { cwd: appRoot, windowsHide: true,
        env: { ...process.env, NEXUS_UNSIGNED_SMOKE: '1' }, stdio: ['ignore', log.fd, log.fd] });
      const timer = setTimeout(() => { child.kill(); reject(new Error(`${logName} exceeded its deadline`)); }, timeout);
      child.once('error', error => { clearTimeout(timer); reject(error); });
      child.once('exit', code => { clearTimeout(timer); code === 0 ? resolve() : reject(new Error(`${logName} exited ${code}`)); });
    });
  } finally { await log.close(); }
}
const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://localhost');
    const filename = path.basename(decodeURIComponent(url.pathname));
    const file = filename === 'latest-x64.yml' ? published?.manifest : assets.get(filename);
    requests.push({ path: url.pathname, method: request.method, version: published?.version, at: new Date().toISOString(), found: !!file });
    if (!file) { response.writeHead(404).end(); return; }
    const info = await stat(file);
    response.writeHead(200, { 'Content-Length': info.size, 'Content-Type': filename.endsWith('.yml') ? 'text/yaml' : 'application/octet-stream' });
    if (request.method === 'HEAD') response.end();
    else if (interruptDownload && filename.endsWith('.exe')) {
      // Send a real partial installer, then hold the connection until the app is killed.
      for await (const chunk of createReadStream(file, { end: 1024 * 1024 - 1, highWaterMark: 65536 })) {
        if (response.destroyed) break;
        response.write(chunk);
        await delay(250);
      }
    } else createReadStream(file).pipe(response);
  } catch (error) { response.writeHead(500).end(String(error)); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
await mkdir(desktop);
await mkdir(path.join(fixture, 'dsh'));
await mkdir(path.join(fixture, 'harness'));
await writeFile(path.join(desktop, 'desktop-update.json'), JSON.stringify({ enabled: false }));
const bootstrap = `import { app } from 'electron';
import { writeFileSync, existsSync, unlinkSync } from 'node:fs';
app.setPath('userData', ${JSON.stringify(desktop)});
app.commandLine.appendSwitch('remote-debugging-port', '0');
Object.assign(process.env, ${JSON.stringify({ NEXUS_DATA_DIR: business, NEXUS_AGENT_PORT: '0', NEXUS_LOCALE: 'zh-CN', DSH_HOME: path.join(fixture, 'dsh'), NEXUS_HARNESS_ROOT: path.join(fixture, 'harness') })});
if (!process.argv.includes('--nexus-shell')) app.whenReady().then(() => writeFileSync(${JSON.stringify(telemetry)}, JSON.stringify({pid:process.pid,version:app.getVersion(),executable:process.execPath})));
await import('./main.mjs');
if (!process.argv.includes('--nexus-shell')) setInterval(() => {
  const signal = ${JSON.stringify(path.join(fixture, 'quit-request'))};
  if (existsSync(signal)) { unlinkSync(signal); app.quit(); }
}, 100).unref();
`;
await writeFile(path.join(fixture, 'update-test-bootstrap.mjs'), bootstrap);

async function build(version, label) {
  const base = require(path.join(appRoot, 'electron-builder.cjs'));
  const directory = path.join(fixture, label);
  const filename = `nexus-update-test-${version}.exe`;
  const configuration = { ...base, appId: `com.nexus.${suffix}`, productName, executableName,
    win: { ...base.win, target: ['nsis'] },
    directories: { output: directory }, artifactName: filename, compression: 'store', forceCodeSigning: false,
    extraMetadata: { version, name, productName, main: 'electron/update-test-bootstrap.mjs' },
    files: [...base.files, { from: fixture, to: 'electron', filter: ['update-test-bootstrap.mjs'] }],
    publish: [{ provider: 'generic', url: origin, channel: 'latest-x64' }],
    nsis: { ...base.nsis, createDesktopShortcut: false, createStartMenuShortcut: false, runAfterFinish: false },
  };
  const configPath = path.join(fixture, `${label}-builder.json`);
  await writeFile(configPath, JSON.stringify(configuration, null, 2));
  progress(`Building local release ${version}`);
  await command(process.execPath, [require.resolve('electron-builder/cli.js'), '--config', configPath, '--win', '--x64', '--publish', 'never'], `${label}-build.log`);
  const release = { version, installer: path.join(directory, filename), manifest: path.join(directory, 'latest-x64.yml'), directory };
  await stat(release.manifest); await stat(release.installer);
  assets.set(filename, release.installer);
  return release;
}
async function connect() {
  const page = await until('renderer debugger', async () => {
    try {
      const port = (await readFile(path.join(desktop, 'DevToolsActivePort'), 'utf8')).split('\n')[0];
      return (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find(page => page.type === 'page' && page.url.startsWith('file:'));
    } catch { return null; }
  });
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }); });
  let next = 0; const pending = new Map();
  ws.addEventListener('message', event => {
    const message = JSON.parse(event.data); const receiver = pending.get(message.id);
    if (receiver) { pending.delete(message.id); message.error ? receiver.reject(message.error) : receiver.resolve(message.result); }
  });
  ws.addEventListener('close', () => { for (const receiver of pending.values()) receiver.reject(new Error('Desktop closed')); pending.clear(); });
  const cdp = (method, params = {}) => new Promise((resolve, reject) => {
    if (ws.readyState !== WebSocket.OPEN) return reject(new Error('Desktop debugger disconnected'));
    const id = ++next;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`Desktop command timed out: ${method}`)); }, 20000);
    pending.set(id, {resolve: value => {clearTimeout(timer);resolve(value);}, reject: error => {clearTimeout(timer);reject(error);}});
    ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async expression => {
    const result = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  const connection = { ws, cdp, evaluate };
  connected = connection;
  await until('isolated Agent ready', async () => {
    try {
      const state = await evaluate('window.nexusDesktop?.invoke("startup_status")');
      if (!state?.available) return false;
      assert.equal(state.data_root, business); return true;
    } catch (error) { if (error.code === 'ERR_ASSERTION') throw error; return false; }
  });
  return connection;
}
async function launch() {
  const child = spawn(executable, [], { cwd: installDirectory, windowsHide: true, stdio: 'ignore' });
  child.on('error', error => progress(`Launch error: ${error.message}`));
  launched = child;
  return connect();
}
async function closeDesktop() {
  if (!connected) return;
  const current = connected; connected = undefined;
  const { pid } = JSON.parse(await readFile(telemetry, 'utf8'));
  await writeFile(path.join(fixture, 'quit-request'), 'quit');
  current.ws.close();
  await until('previous desktop exit', () => {
    if (launched) return launched.exitCode !== null || launched.signalCode !== null;
    try { process.kill(pid, 0); return false; } catch (error) { if (error.code === 'ESRCH') return true; throw error; }
  }, 20000);
}
const feedRequests = () => requests.filter(request => request.path.endsWith('.yml'));
const status = () => connected.evaluate('window.nexusDesktop.invoke("update_status")');
const sha256 = async file => createHash('sha256').update(await readFile(file)).digest('hex');
async function confirmDownload(version) {
  await until('available update waiting for consent', async () => (await status()).phase === 'available');
  assert.ok(!requests.some(request => request.path.endsWith(`nexus-update-test-${version}.exe`)), 'Checking must not download an installer');
  await connected.evaluate('document.querySelector(".sidebar-footer button").click(); true');
  await until('download confirmation', () => connected.evaluate('Array.from(document.querySelectorAll("[role=dialog] button")).some(b=>b.textContent.trim()==="确认并下载")'));
  await connected.evaluate('Array.from(document.querySelectorAll("[role=dialog] button")).find(b=>b.textContent.trim()==="确认并下载").click(); true');
}


try {
  const baseline = await build('0.1.3', 'baseline');
  const interrupted = await build('0.1.4-local.0', 'interrupted');
  const update = await build('0.1.4-local.1', 'published');
  published = baseline;
  progress('Installing isolated baseline');
  await command(baseline.installer, ['/S', '/currentuser', `/D=${installDirectory}`], 'baseline-install.log', 180000);
  installed = true;
  await stat(executable);
  await launch();
  assert.equal((await status()).enabled, false);
  assert.equal(feedRequests().length, 0, 'Disabled startup must not contact the release feed');
  assert.equal(await connected.evaluate('Array.from(document.querySelectorAll(".sidebar-footer button")).some(b=>b.textContent.trim()==="更新")'), false);
  await connected.evaluate('window.nexusDesktop.invoke("update_settings", {enabled:true})');
  const survivingAgent = JSON.parse(await readFile(path.join(business, 'run/agent.json'), 'utf8'));
  await closeDesktop();
  assert.doesNotThrow(() => process.kill(survivingAgent.pid, 0), 'Closing Launcher preserves its independent Agent');
  progress('Verifying enabled startup against the current release');
  await launch();
  assert.equal(JSON.parse(await readFile(path.join(business, 'run/agent.json'), 'utf8')).instance_id, survivingAgent.instance_id);
  await until('startup no-update result', async () => feedRequests().length === 1 && (await status()).phase === 'idle');
  assert.equal(await connected.evaluate('Array.from(document.querySelectorAll(".sidebar-footer button")).some(b=>b.textContent.trim()==="更新")'), false);
  await closeDesktop();
  progress('Force-closing during a real partial download before publishing a newer version');
  published = interrupted; interruptDownload = true;
  await launch();
  progress('Waiting for explicit consent for interrupted download');
  await confirmDownload(interrupted.version);
  progress('Consent accepted; waiting for partial download progress');
  await until('partial installer download', async () => {
    const state = await status();
    return state.phase === 'downloading' && state.percent > 0 && state.percent < 100;
  });
  assert.equal(await connected.evaluate('!!document.querySelector("[role=dialog] progress")'), true);
  connected.ws.close(); connected = undefined;
  launched.kill();
  await until('force-closed desktop', () => launched.exitCode !== null || launched.signalCode !== null);
  interruptDownload = false;
  progress('Publishing a newer release on the local feed');
  published = update;
  const oldHash = await sha256(path.join(installDirectory, 'resources/app.asar'));
  await launch();
  const before = JSON.parse(await readFile(telemetry, 'utf8'));
  assert.equal(before.version, baseline.version);
  await confirmDownload(update.version);
  await until('downloaded new release', async () => {
    const state = await status();
    if (state.phase === 'error') throw new Error(state.error);
    return state.phase === 'ready' && state.version === update.version;
  }, 120000);
  assert.equal(feedRequests().length, 3, 'Each enabled launch checks once');
  assert.ok(requests.some(request => request.path.endsWith(path.basename(update.installer))), 'Real installer was downloaded from the local feed');
  await until('footer update button', () => connected.evaluate('Array.from(document.querySelectorAll(".sidebar-footer button")).some(b=>b.textContent.trim()==="更新")'));
  await connected.evaluate('document.querySelector(".sidebar-footer").scrollIntoView({block:"end"}); true');
  const screenshot = await connected.cdp('Page.captureScreenshot', { format: 'png' });
  await writeFile(path.join(fixture, 'update-ready.png'), Buffer.from(screenshot.data, 'base64'));
  progress('Clicking the real footer update button; awaiting NSIS replacement and restart');
  await connected.evaluate('Array.from(document.querySelectorAll(".sidebar-footer button")).find(b=>b.textContent.trim()==="更新").click(); true');
  await until('verified update restart choice', () => connected.evaluate('Array.from(document.querySelectorAll("[role=dialog] button")).some(b=>b.textContent.trim()==="更新并重启")'));
  await connected.evaluate('Array.from(document.querySelectorAll("[role=dialog] button")).find(b=>b.textContent.trim()==="稍后重启").click(); true');
  assert.equal((await status()).phase,'ready','Deferring restart must preserve the verified download');
  await connected.evaluate('document.querySelector(".sidebar-footer button").click(); true');
  await connected.evaluate('Array.from(document.querySelectorAll("[role=dialog] button")).find(b=>b.textContent.trim()==="更新并重启").click(); true');
  connected.ws.close(); connected = undefined;
  const after = await until('upgraded application relaunched', async () => {
    try { const value = JSON.parse(await readFile(telemetry, 'utf8')); return value.pid !== before.pid && value.version === update.version && value; }
    catch { return false; }
  }, 180000);
  assert.equal(after.executable.toLowerCase(), executable.toLowerCase());
  const installedHash = await sha256(path.join(installDirectory, 'resources/app.asar'));
  assert.notEqual(installedHash, oldHash);
  assert.equal(installedHash, await sha256(path.join(update.directory, 'win-unpacked/resources/app.asar')));
  launched = undefined;
  await connect();
  await until('new version startup check', async () => feedRequests().length === 4 && (await status()).phase === 'idle');
  assert.equal((await status()).enabled, true, 'Update preference survives installation');
  assert.equal(await connected.evaluate('Array.from(document.querySelectorAll(".sidebar-footer button")).some(b=>b.textContent.trim()==="更新")'), false);
  const report = { passed: true, fixture, origin, baseline: baseline.version, updated: update.version,
    oldPid: before.pid, newPid: after.pid, installedAsarSha256: installedHash,
    checks: ['disabled startup made no feed request', 'normal desktop exit preserves the independent Agent and reconnects', 'enabled startup checked once', 'local publication was detected on restart',
      'force-close interrupted an actual installer transfer; restart fetched and installed the newer release',
      'real NSIS installer downloaded and checksum-verified by electron-updater', 'explicit download consent, visible progress, deferred restart, then confirmed install and restart',
      'installed application hash matches the published package', 'isolated Agent available after restart', 'preference preserved and update button hidden when current'], requests };
  await connected.evaluate('window.nexusDesktop.invoke("proxy_request", {method:"POST",path:"/v1/agent",body:{action:"stop"}})');
  await closeDesktop();
  await writeFile(path.join(fixture, 'report.json'), JSON.stringify(report, null, 2));
  progress(`PASS: ${path.join(fixture, 'report.json')}`);
} catch (error) {
  await writeFile(path.join(fixture, 'failure.json'), JSON.stringify({ error: error.stack, requests }, null, 2));
  throw error;
} finally {
  if (connected) {
    await connected.evaluate('window.nexusDesktop.invoke("proxy_request", {method:"POST",path:"/v1/agent",body:{action:"stop"}})').catch(() => {});
    await closeDesktop().catch(() => {});
  }
  if (launched && launched.exitCode === null) launched.kill();
  server.close();
  if (installed) {
    const uninstaller = (await readdir(installDirectory)).find(name => /^Uninstall .*\.exe$/i.test(name));
    if (uninstaller) {
      await command(path.join(installDirectory, uninstaller), ['/S', '/currentuser'], 'uninstall.log', 180000);
      await until('isolated test application removed', async () => { try { await stat(executable); return false; } catch (error) { if (error.code === 'ENOENT') return true; throw error; } }, 180000);
      progress('Isolated test installation removed; evidence and release artifacts retained');
    }
  }
}
