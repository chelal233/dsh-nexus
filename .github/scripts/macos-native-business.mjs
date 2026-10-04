import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { runBusinessQA } from './linux-rpm-business.mjs';
import { connect, socket, until, delay } from './acceptance-cdp.mjs';
import { ownedDirectory, storageGate, treeFootprint } from './macos-acceptance-storage.mjs';
import { GIB, exportIncrement, importIncrement } from './macos-acceptance-contract.mjs';
import { releaseOwnedChildHandles } from './linux-rpm-preflight-lifecycle.mjs';
import { SESSION_TEXT } from './migration-session-fixture.mjs';

assert.equal(process.platform, 'darwin'); assert.ok(process.getuid() > 0);
const root = await fs.realpath(process.env.QA_ROOT), evidence = await fs.realpath(process.env.QA_PHASE_EVIDENCE);
await ownedDirectory(root, root);
const phase = process.env.QA_PHASE, app = await ownedDirectory(root, process.env.QA_INSTALLED_APP);
assert.ok(['fresh', 'old', 'final'].includes(phase));
const executable = path.join(app, 'Contents/MacOS/Nexus Launcher');
const manifestBytes = await fs.readFile(path.join(app, 'Contents/Resources/release-manifest.json'));
const manifest = JSON.parse(manifestBytes);
const expected = JSON.parse(await fs.readFile(process.env.QA_PACKAGE_PROOF, 'utf8'));
for (const name of ['version', 'commit', 'buildId']) assert.equal(manifest[name], expected.build[name]);
assert.equal(manifest.runtime.target, `${process.arch === 'x64' ? 'x86_64' : 'aarch64'}-apple-darwin`);
const expectedIdentity = { version: manifest.version, commit: manifest.commit, buildId: manifest.buildId,
  manifestSha256: createHash('sha256').update(manifestBytes).digest('hex') };
const report = { schema: 1, result: 'FAIL', phase, format: process.env.QA_PACKAGE_FORMAT, expectedIdentity, checks: [], children: [],
  realDeviceAcceptance: false, gatekeeperFirstOpen: 'NOT RUN', crashRecovery: 'NOT RUN' };
const children = [], channels = [], targetEvents = [], gate = storageGate(root, evidence);
const abort = new AbortController();
process.once('SIGTERM', () => abort.abort('Parent stopped this owned worker after capacity or deadline failure'));
const checkAbort = () => { if (abort.signal.aborted) throw Object.assign(new Error(String(abort.signal.reason)), { code: 'ABORTED' }); };
let browser, web, initial, oldHandlers;
const redact = value => String(value).replace(/([?&]token=)[^\s&"']+/gi, '$1[redacted]');
function start(program, args, env) {
  assert.ok(args.every(arg => !/no-sandbox|disable-setuid-sandbox|disable-web-security/.test(arg)));
  const child = spawn(program, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
  child.qaProgram = program; child.qaClosed = false; child.qaLog = '';
  child.qaClose = new Promise(resolve => child.once('close', () => { child.qaClosed = true; resolve(); }));
  child.on('error', error => { child.qaLog = redact(error.message); });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', bytes => {
    child.qaLog = (child.qaLog + redact(bytes)).slice(-32000);
  });
  children.push(child); return child;
}
const jxa = script => execFileSync('/usr/bin/osascript', ['-l', 'JavaScript', '-e', script],
  { encoding: 'utf8', timeout: 30000, maxBuffer: 8192 }).trim();
async function launchInstance(spec) {
  checkAbort();
  assert.equal(spec.executable, executable);
  for (const directory of [spec.dataRoot, spec.home, spec.dshHome, spec.userDataDir]) await ownedDirectory(root, directory);
  const proc = start(executable, [`--user-data-dir=${spec.userDataDir}`, '--remote-debugging-port=0'], {
    HOME: spec.home, TMPDIR: path.join(root, 'tmp'), NEXUS_DATA_DIR: spec.dataRoot,
    DSH_HOME: spec.dshHome, NEXUS_AGENT_PORT: '0', NEXUS_LOCALE: 'en',
  });
  const desktop = await connect(proc, spec.userDataDir, item => item.type === 'page' && item.url.startsWith('file:'));
  channels.push(desktop);
  await until(proc, () => desktop.evaluate('window.nexusDesktop?.invoke("startup_status").then(s=>s.available?s:null)'));
  assert.equal(await desktop.evaluate('typeof require'), 'undefined');
  assert.equal(await desktop.evaluate('typeof process'), 'undefined');
  return { proc, desktop };
}
async function closeInstance(instance) {
  if (!instance || instance.proc.qaClosed) return;
  await instance.desktop.evaluate('window.nexusDesktop.invoke("harness_desktop_stop")');
  await instance.desktop.evaluate('window.nexusDesktop.invoke("proxy_request",{method:"POST",path:"/v1/agent",body:{action:"stop"}})');
  try { await instance.desktop.host.cdp('Browser.close'); } catch { /* May close before replying. */ }
  instance.desktop.ws.close(); instance.desktop.host.ws.close();
  await Promise.race([instance.proc.qaClose, delay(15000)]);
  assert.ok(instance.proc.qaClosed && instance.proc.exitCode === 0, 'Normal owned GUI close must complete');
}
async function restartInstance({ instance, spec }) { await closeInstance(instance); return launchInstance(spec); }
async function findOfficialPage({ url, openedAfter }) {
  const expectedUrl = new URL(url);
  assert.equal(expectedUrl.protocol, 'http:');
  assert.ok(['127.0.0.1', 'localhost'].includes(expectedUrl.hostname));
  const clean = expectedUrl.pathname === '/' && expectedUrl.searchParams.size === 1 && expectedUrl.searchParams.has('token')
    ? new URL('./', expectedUrl).href : undefined;
  const page = await until(browser, async () => (await web.getPages()).find(item => {
    const opened = targetEvents.findLast(event => event.id === item.id && event.url === expectedUrl.href && event.time >= openedAfter);
    return item.type === 'page' && opened && [expectedUrl.href, clean].includes(item.url)
      && targetEvents.some(event => event.id === item.id && event.url === item.url && event.time >= opened.time);
  }));
  const channel = await socket(page.webSocketDebuggerUrl); channels.push(channel);
  await fs.writeFile(path.join(evidence, `official-open-${page.id}.json`), JSON.stringify({ targetId: page.id,
    openedAfter, observedUrl: expectedUrl.origin + expectedUrl.pathname, source: 'Native open_url and exact browser target event' }));
  return { page: channel, targetId: page.id, method: 'existing-cdp-target' };
}
const admitted = new Set();
async function budgetGate({ stage, dataRoot, dshHome, archivePreview }) {
  checkAbort();
  for (const directory of [dataRoot, dshHome]) await ownedDirectory(root, directory);
  let increment = 0;
  if (!admitted.has(stage)) {
    if (stage === 'fresh_official_fetch') increment = 6 * GIB;
    if (['full_runtime_export', 'full_import_and_publication', 'inflight_import_cancel', 'retry_full_import_same_B'].includes(stage)) {
      if (stage === 'full_runtime_export') {
        const [slot, runtime, home, host] = await Promise.all([
          treeFootprint(root, path.join(root, 'business/releases')),
          treeFootprint(root, path.join(app, 'Contents/Resources/runtime')),
          treeFootprint(root, path.join(root, 'dsh')), treeFootprint(root, app),
        ]);
        increment = exportIncrement({ slot, runtime, home, host });
        await fs.writeFile(path.join(evidence, 'export-admission.json'), JSON.stringify({ slot, runtime, home, host, increment }));
      } else {
        const receiver = await treeFootprint(root, dshHome);
        increment = importIncrement(archivePreview, receiver);
        await fs.writeFile(path.join(evidence, stage + '-admission.json'), JSON.stringify({ archivePreview, receiver, increment }));
      }
    }
    await gate(stage, increment, true); admitted.add(stage);
  }
  return gate(stage);
}
async function seedDesktopFixture(home) {
  const profile = path.join(home, 'profiles/desktop'), modules = path.join(profile, 'node_modules');
  await ownedDirectory(root, profile);
  const file = path.join(profile, 'package.json'), metadata = JSON.parse(await fs.readFile(file));
  metadata.dependencies = { ...metadata.dependencies, 'qa-round2-plugin': '1.0.0' };
  metadata.dsh.profile.bundles = [...new Set([...metadata.dsh.profile.bundles, 'qa-round2-plugin'])];
  await fs.writeFile(file, JSON.stringify(metadata, null, 2) + '\n');
  await fs.mkdir(modules, { recursive: true }); await ownedDirectory(root, modules);
  const unit = path.join(modules, 'qa-round2-plugin');
  await fs.cp(path.join(home, 'profiles/web/node_modules/qa-round2-plugin'), unit, { recursive: true, errorOnExist: true, force: false });
  await fs.copyFile(path.join(home, 'profiles/web/cordis.patch.yml'), path.join(profile, 'cordis.patch.yml'));
  const plugin = path.join(unit, 'plugin.mjs');
  let text = await fs.readFile(plugin, 'utf8');
  assert.ok(text.includes('let handle;') && text.includes('const loaded=await handle.read();') && text.includes('process.stdout.write('));
  text = "import fs from 'node:fs/promises';import path from 'node:path';\n" + text
    .replace('let handle;', 'let receiptFacts;let handle;')
    .replace('const loaded=await handle.read();', "const loaded=await handle.read();receiptFacts={sessionId:'qa-valid-session',cwd:match[0].header.cwd,eventCount:loaded.events.length,kind:loaded.events[0]?.data?.source?.kind,text:loaded.events[0]?.data?.content?.[0]?.text,dependency:leaf,marker:config.marker,pid:process.pid};")
    .replace('process.stdout.write(', "await fs.writeFile(path.join(process.env.DSH_HOME,'storages/qa-round2/desktop-receipt.json'),JSON.stringify(receiptFacts));process.stdout.write(");
  await fs.writeFile(plugin, text);
  return { pluginSha256: createHash('sha256').update(text).digest('hex') };
}
async function desktopChecks(instance = initial, home = path.join(root, 'dsh'), userData = path.join(root, 'electron'), seed = false) {
  const invoke = (command, args = {}) => { checkAbort(); return instance.desktop.evaluate(`window.nexusDesktop.invoke(${JSON.stringify(command)},${JSON.stringify(args)})`); };
  const waitReady = timeout => until(instance.proc, async () => {
    const state = await invoke('harness_desktop_status');
    if (state.phase === 'failed' || state.audit?.state === 'failed') throw new Error('Official Desktop failure: ' + JSON.stringify(state));
    return state.phase === 'launched' && state.audit?.state === 'ready' && state;
  }, timeout);
  const before = await invoke('harness_desktop_capability'); assert.equal(before.supported, true);
  await gate('official_desktop_preparation', 6 * GIB, true);
  const started = await invoke('harness_desktop_start');
  const preparing = await invoke('harness_desktop_status');
  if (preparing.phase !== 'preparing') throw Object.assign(new Error('Desktop preparation cancellation window was not captured'), { code: 'INTERRUPTION_NOT_CAPTURED' });
  const stopped = await invoke('harness_desktop_stop'); assert.equal(stopped.phase, 'stopped');
  report.desktopCancel = { started, captured: preparing, stopped };
  await invoke('harness_desktop_start');
  let ready = await waitReady(8 * 60 * 1000);
  if (seed) {
    assert.equal((await invoke('harness_desktop_stop')).phase, 'stopped');
    report.desktopFixture = await seedDesktopFixture(home);
  }
  const receiptFile = path.join(home, 'storages/qa-round2/desktop-receipt.json');
  await fs.rm(receiptFile, { force: true });
  await invoke('harness_desktop_restart'); ready = await waitReady(5 * 60 * 1000);
  const receipt = await until(instance.proc, async () => {
    try { return JSON.parse(await fs.readFile(receiptFile)); } catch (error) { if (error.code === 'ENOENT') return null; throw error; }
  });
  assert.deepEqual({ sessionId: receipt.sessionId, cwd: receipt.cwd, eventCount: receipt.eventCount,
    kind: receipt.kind, text: receipt.text, dependency: receipt.dependency, marker: receipt.marker }, {
    sessionId: 'qa-valid-session', cwd: path.join(root, 'fixture-workspace'), eventCount: 6, kind: 'user',
    text: SESSION_TEXT, dependency: 'qa-round2-transitive-ok', marker: 'qa-round2-config',
  });
  assert.ok(Number.isInteger(receipt.pid) && receipt.pid > 0);
  report.desktopBusiness = { status: 'PASS actual Desktop producer session read and Cordis transitive plugin apply', receipt,
    visualSessionResume: 'NOT RUN', thirdPartyPlugins: 'Synthetic QA plugin only' };
  assert.ok(Number.isInteger(ready.childPid) && ready.childPid > 0);
  // The original producer's structured startup audit is required, never only a live PID.
  report.officialDesktop = ready;
  const recipe = JSON.parse(await fs.readFile(path.join(userData, 'harness-desktop/launch.json')));
  await ownedDirectory(root, recipe.source); await ownedDirectory(root, recipe.kit);
  const officialPackage = JSON.parse(await fs.readFile(path.join(recipe.source, 'apps/desktop/node_modules/electron/package.json')));
  const kit = JSON.parse(await fs.readFile(path.join(recipe.kit, 'manifest.json')));
  const actualCommand = execFileSync('/bin/ps', ['-p', String(ready.childPid), '-o', 'command='], { encoding: 'utf8', timeout: 10000 }).trim();
  const shared = recipe.electronVersion === officialPackage.version;
  const selectedHost = shared ? recipe.electronExecutable : kit.schema === 3
    ? path.join(recipe.userData, 'runtime', `host-${kit.hostArchiveSha256}`, 'Nexus Launcher.app/Contents/MacOS/Nexus Launcher')
    : null;
  assert.ok(selectedHost && actualCommand.includes(selectedHost), 'Actual official Desktop PID must use the normally selected verified host');
  report.desktopHostSelection = { actualCommand: redact(actualCommand), shared,
    launcherElectron: recipe.electronVersion, officialElectron: officialPackage.version, kitElectron: kit.electronVersion,
    status: 'PASS actual normal launch and offline preparation',
    portableFallback: shared ? 'NOT APPLICABLE: exact shared host selected' : 'PASS required different-version host launched' };
  await assert.rejects(() => invoke('proxy_request', { method: 'POST', path: '/v1/harness', body: { action: 'start' } }), /Close Harness Desktop|请先关闭 Harness Desktop/);
  await invoke('harness_desktop_restart');
  const restarted = await waitReady(5 * 60 * 1000);
  assert.notEqual(restarted.operationId, ready.operationId);
  report.desktopRestart = restarted;
  assert.equal((await invoke('harness_desktop_stop')).phase, 'stopped');
  report.checks.push('Official Desktop real readiness audit, preparation cancellation, mutation lock and normal restart');
}
try {
  await gate('native_gui', 128 * 1024 ** 2, true);
  // Temporary runner-only native LaunchServices setting; restore exact old handlers in finally.
  oldHandlers = JSON.parse(jxa("ObjC.import('CoreServices'); JSON.stringify(['http','https'].map(s=>[s,ObjC.unwrap($.LSCopyDefaultHandlerForURLScheme(s))]));"));
  for (const [scheme] of oldHandlers) assert.equal(Number(jxa(`ObjC.import('CoreServices'); $.LSSetDefaultHandlerForURLScheme(${JSON.stringify(scheme)},'com.google.Chrome');`)), 0);
  const fixture = path.join(root, 'browser-fixture.html');
  await fs.writeFile(fixture, '<!doctype html><title>Nexus QA ordinary browser</title><p>nexus-qa-browser-preflight</p>');
  browser = start('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', [
    `--user-data-dir=${root}/browser`, '--remote-debugging-port=0', '--no-first-run', '--no-default-browser-check', `file://${fixture}`,
  ], { TMPDIR: path.join(root, 'tmp') });
  web = await connect(browser, path.join(root, 'browser'), item => item.type === 'page' && item.url.includes('browser-fixture.html'));
  channels.push(web);
  web.host.ws.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    if (['Target.targetCreated', 'Target.targetInfoChanged'].includes(message.method)) {
      const item = message.params.targetInfo; targetEvents.push({ time: Date.now(), id: item.targetId, url: item.url });
      if (targetEvents.length > 1000) targetEvents.shift();
    }
  });
  await web.host.cdp('Target.setDiscoverTargets', { discover: true });
  initial = await launchInstance({ executable, dataRoot: path.join(root, 'business'), home: path.join(root, 'home'),
    dshHome: path.join(root, 'dsh'), userDataDir: path.join(root, 'electron') });
  const screen = await initial.desktop.cdp('Page.captureScreenshot', { format: 'png' });
  await fs.writeFile(path.join(evidence, 'installed-native.png'), Buffer.from(screen.data, 'base64'));
  report.checks.push('Original native app GUI, private Agent bridge, no Node globals in renderer');
  await runBusinessQA({ phase, root, evidence, report, desktop: initial.desktop, executable,
      signal: abort.signal,
      expectedIdentity, workspace: path.join(root, 'fixture-workspace'),
      migrationProfiles: ['web', 'desktop'],
      verifyDesktopMigration: ({ instance, spec, home }) => desktopChecks(instance, home, spec.userDataDir),
      deadlineAt: Date.now() + (phase === 'final' ? 65 : 25) * 60 * 1000,
      budgetGate, findOfficialPage, launchInstance, restartInstance });
    assert.equal(report.businessQA.status, 'PASS');
  if (phase === 'fresh') await desktopChecks(initial, path.join(root, 'dsh'), path.join(root, 'electron'), true);
  report.result = 'PASS BUSINESS ' + phase;
} catch (error) {
  report.error = { code: error.code, message: redact(error.message) }; process.exitCode = 1;
} finally {
  try { await closeInstance(initial); } catch (error) { report.cleanupError = redact(error.message); }
  for (const channel of channels) {
    try { await channel.host?.cdp('Browser.close'); } catch { /* close event can precede response */ }
    channel.ws.close(); channel.host?.ws.close();
  }
  for (const child of children) {
    await Promise.race([child.qaClose, delay(5000)]);
    if (!child.qaClosed) { child.kill('SIGTERM'); await Promise.race([child.qaClose, delay(10000)]); }
    report.children.push({ pid: child.pid, program: child.qaProgram, closed: child.qaClosed,
      exitCode: child.exitCode, signalCode: child.signalCode, tail: child.qaLog });
    if (!child.qaClosed) { report.cleanupError = 'Owned child did not confirm close'; releaseOwnedChildHandles(child); }
  }
  if (oldHandlers) try {
    for (const [scheme, handler] of oldHandlers) {
      assert.equal(typeof handler, 'string');
      assert.equal(Number(jxa(`ObjC.import('CoreServices'); $.LSSetDefaultHandlerForURLScheme(${JSON.stringify(scheme)},${JSON.stringify(handler)});`)), 0);
    }
    report.defaultBrowserRestored = true;
  } catch (error) { report.cleanupError = 'Temporary default browser restoration failed: ' + error.message; }
  if (report.cleanupError) { report.result = 'FAIL'; process.exitCode = 1; }
  await fs.writeFile(path.join(evidence, 'native-result.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ phase, result: report.result, error: report.error, cleanupError: report.cleanupError }));
}
