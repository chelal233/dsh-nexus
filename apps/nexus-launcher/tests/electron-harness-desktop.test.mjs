import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readDesktopState, managedDesktopRoot, HarnessDesktop, probeDesktopSupport, desktopCapability, desktopPreferenceEnvironment } from '../electron/harness-desktop.mjs';
import { verifyDesktopKit, digest, selectDesktopKit } from '../electron/desktop-runtime.mjs';
import { desktopRuntimeForExport } from '../../../crates/nexus-agent/scripts/offline-package.mjs';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-desktop-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const put = (relative, content) => { const file = path.join(root, relative); fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, content); return file; };
  return { root, put };
}
test('Desktop status distinguishes dead workers, live orphans, and finished native clients', t => {
  const { put } = fixture(t);
  const file = put('state.json', JSON.stringify({ phase: 'launched', pid: 10, childPid: 20 }));
  assert.equal(readDesktopState(file, pid => pid === 20).phase, 'launched');
  assert.equal(readDesktopState(file, () => false).error, 'desktop_interrupted');
  fs.writeFileSync(file, JSON.stringify({ phase: 'stopped', pid: 10 }));
  assert.equal(readDesktopState(file, () => false).phase, 'stopped');
});
test('managed source rejects traversal and source redirects outside release storage', t => {
  const { root, put } = fixture(t);
  put('releases/selected/package.json', '{}'); put('elsewhere/package.json', '{}');
  assert.equal(managedDesktopRoot(root, 'selected'), fs.realpathSync(path.join(root, 'releases/selected')));
  for (const value of ['../elsewhere', 'a/b', '', '..', 'C:\\elsewhere']) assert.throws(() => managedDesktopRoot(root, value));
  fs.symlinkSync(path.join(root, 'elsewhere'), path.join(root, 'releases/escape'), process.platform === 'win32' ? 'junction' : 'dir');
  assert.throws(() => managedDesktopRoot(root, 'escape'), /invalid_source/);
});
test('Desktop launch inspects status without scheduling Web and rejects a running Web process', async t => {
  const { root } = fixture(t), calls = [];
  const bridge = { request: async (command, args) => {
    calls.push(command);
    if (command === 'desktop_launch_context') return { available: true, data_root: root };
    if (args.path === '/v1/harness') return { harness: { state: 'running', pid: 50 } };
    return {};
  } };
  await assert.rejects(new HarnessDesktop({ bridge, userData: root, resources: root }).start(), /desktop_stop_web/);
  assert.ok(!calls.includes('startup_status'));
});
test('offline kit rejects missing and corrupted payloads; imported runtime is preferred', { skip: process.platform === 'linux' }, async t => {
  const { root, put } = fixture(t);
  const payload = put('runtime/desktop/assets/temp', 'payload'), assetHash = digest(payload);
  fs.renameSync(payload, path.join(root, 'runtime/desktop/assets', assetHash));
  const target = `${process.platform === 'win32' ? 'win' : 'mac'}-${process.arch}`;
  const lock = { targets: { [target]: { nodeSha256: assetHash, pythonSha256: assetHash, wheels: [] } }, wheels: [] };
  const lockFile = put('runtime/desktop/lock.json', JSON.stringify(lock));
  const electron = process.platform === 'win32' ? 'electron/electron.exe' : 'electron/Electron.app/Contents/MacOS/Electron';
  put(`runtime/desktop/${electron}`, 'fixture');
  const kit = path.join(root, 'runtime/desktop');
  const manifest = { schema: 1, platform: process.platform, arch: process.arch, lockSha256: digest(lockFile), electronVersion: '44.0.0', pnpmVersion: '11.7.0',
    files: ['lock.json', electron, `assets/${assetHash}`].map(name => ({ path: name, sha256: digest(path.join(kit, name)) })) };
  const file = put('runtime/desktop/manifest.json', JSON.stringify(manifest));
  verifyDesktopKit(kit);
  assert.equal(selectDesktopKit('missing', { runtime: { node: { ownership: 'nexus', path: path.join(root, 'runtime/node/node.exe') } } }), kit);
  put('slot/apps/desktop/scripts/primary-runtime-lock.json', JSON.stringify(lock));
  put('slot/apps/desktop/node_modules/electron/package.json', JSON.stringify({ version: '44.0.0' }));
  put('slot/apps/desktop/node_modules/pnpm/package.json', JSON.stringify({ version: '11.7.0' }));
  if (process.platform === 'win32') assert.equal(await desktopRuntimeForExport(path.join(root, 'slot'), path.join(root, 'old-runtime'), kit), kit);
  fs.writeFileSync(path.join(kit, electron), 'corrupt');
  assert.throws(() => verifyDesktopKit(kit), /invalid/);
  if (process.platform === 'win32') await assert.rejects(desktopRuntimeForExport(path.join(root, 'slot'), path.join(root, 'old-runtime'), kit), /integrity/);
  fs.writeFileSync(path.join(kit, electron), 'fixture');
  manifest.files = manifest.files.filter(item => !item.path.startsWith('assets/'));
  fs.writeFileSync(file, JSON.stringify(manifest));
  assert.throws(() => verifyDesktopKit(kit), /missing/);
  assert.throws(() => selectDesktopKit(path.join(root, 'missing'), {}), /missing/);
  if (process.platform === 'win32') {
    put('slot/apps/desktop/node_modules/electron/package.json', JSON.stringify({ version: '45.0.0' }));
    await assert.rejects(desktopRuntimeForExport(path.join(root, 'slot'), path.join(root, 'old-runtime'), kit), /No matching/);
  }
});


test('Desktop support probe reads the selected release without requiring runtime dependencies', async t => {
  const { root, put } = fixture(t);
  put('releases/web-only/package.json', '{}');
  put('releases/native/apps/desktop/package.json', JSON.stringify({name:'@deepseek-ai/dsh-desktop',main:'lib/main.js',version:'test'}));
  assert.equal(probeDesktopSupport(path.join(root,'releases/native'), { platform: 'linux', arch: 'arm64' }).supported, false);
  assert.equal(probeDesktopSupport(path.join(root,'releases/native')).supported, false);
  put('releases/native/apps/desktop/scripts/primary-runtime-lock.json', JSON.stringify({ targets: { [`${({ win32: 'win', darwin: 'mac', linux: 'linux' })[process.platform]}-${process.arch}`]: {} } }));
  put('releases/native/apps/desktop/scripts/desktop-build-paths.mjs', `const SUPPORTED_TARGETS = new Set(['${({ win32: 'win', darwin: 'mac', linux: 'linux' })[process.platform]}-${process.arch}'])`);
  assert.equal(probeDesktopSupport(path.join(root,'releases/web-only')).supported,false);
  assert.equal(probeDesktopSupport(path.join(root,'releases/native')).supported,true);
  assert.throws(()=>desktopCapability(path.join(root,'releases/native')), /desktop_install_incomplete/);
  let release='native'; const calls=[];
  const bridge={request:async(command,args)=>{
    calls.push([command,args]);
    if(command==='desktop_launch_context') return {available:true,data_root:root};
    if(args.path==='/v1/releases') return {current_release:release};
    if(args.path==='/v1/config') return {};
    throw new Error('Unexpected probe side effect');
  }};
  const desktop=new HarnessDesktop({bridge,userData:root,resources:root});
  assert.deepEqual(await desktop.capability(),{supported:true,version:'test',release:'native'});
  release='web-only'; assert.deepEqual(await desktop.capability(),{supported:false,release:'web-only'});
  assert.equal(fs.existsSync(desktop.file),false);
  assert.ok(calls.every(([command,args])=>command==='desktop_launch_context'||args.method==='GET'));
  put('releases/native/apps/desktop/package.json','invalid');
  assert.throws(()=>probeDesktopSupport(path.join(root,'releases/native')));
});

// Exercise OS child environment serialization, including an explicit empty opt-in.
test('Desktop forwards common settings to a real child without applying Web or SDK flags', async () => {
  const { spawnSync } = await import('node:child_process');
  const preferences = { deepseek_base_url: 'https://model.example', search_base_url: 'https://search.example',
    search_provider: 'test-search', fetch_provider: 'test-fetch', agents_home: '/test/agents',
    bundled_skill_dir: '/test/skills', permission_mode: 'read-only', telemetry_disabled: false,
    port: 0, open_browser: false, tools_mode: 'ptc', context_window: 123, system_prompt: 'not-desktop' };
  const overrides = desktopPreferenceEnvironment(preferences);
  const child = spawnSync(process.execPath, ['-e', 'process.stdout.write(JSON.stringify(Object.fromEntries(JSON.parse(process.argv[1]).map(k=>[k,process.env[k]]))))', JSON.stringify(Object.keys(overrides))],
    { env: { ...process.env, DSH_TELEMETRY_DISABLED: '1', ...overrides }, encoding: 'utf8' });
  assert.equal(child.status, 0, child.stderr);
  assert.deepEqual(JSON.parse(child.stdout), {
    DEEPSEEK_BASE_URL: preferences.deepseek_base_url, DEEPSEEK_SEARCH_BASE_URL: preferences.search_base_url,
    DSH_WEB_SEARCH_PROVIDER: 'test-search', DSH_WEB_FETCH_PROVIDER: 'test-fetch', DSH_AGENTS_HOME: '/test/agents',
    DSH_BUNDLED_SKILL_DIR: '/test/skills', DSH_PERMISSION_MODE: 'read-only', DSH_TELEMETRY_DISABLED: '',
  });
  assert.equal(desktopPreferenceEnvironment({telemetry_disabled:true}).DSH_TELEMETRY_DISABLED, '1');
  assert.deepEqual(desktopPreferenceEnvironment(), {});
  assert.deepEqual(desktopPreferenceEnvironment(null), {});
});

test('Desktop does not run dependency repair before its first launch', async t => {
  const { root, put } = fixture(t);
  put('releases/test-slot/package.json', '{}');
  let checked = false;
  const bridge = { request: async (command, args) => {
    if (command === 'desktop_launch_context') return { available: true, data_root: root };
    if (args.path === '/v1/releases') return { current_release: 'test-slot' };
    if (args.path === '/v1/config') return {};
    if (args.path === '/v1/harness') return { harness: { state: 'stopped' } };
    if (args.path === '/v1/dependencies') {
      assert.deepEqual(args, { method: 'POST', path: '/v1/dependencies', body: { startup: true } });
      checked = true; throw new Error('Startup dependency unavailable: cordis');
    }
    throw new Error('Unexpected call');
  } };
  const desktop = new HarnessDesktop({ bridge, userData: root, resources: root });
  await assert.rejects(desktop.start(), /desktop_/);
  assert.equal(checked, false);
});

test('Desktop retries only a failed missing dependency once, and Stop cancels the retry', async t => {
  const {root}=fixture(t);
  for (const cancel of [false,true]) {
    const calls=[];let launches=0;
    const desktop=new HarnessDesktop({userData:root,resources:root,bridge:{request:async (_command,args)=>{
      calls.push(args.path);
      if(args.path==='/v1/config')return {};
      if(args.path==='/v1/releases')return {current_release:'test'};
      if(args.path==='/v1/dependencies'){if(cancel)desktop.stopGeneration++;return {phase:'repaired'};}
      throw Error('unexpected call');
    }}});
    desktop.status=()=>({operationId:'owned',phase:'failed',detail:'ERR_MODULE_NOT_FOUND'});
    desktop.startOperation=async retry=>{assert.equal(retry,true);launches++;};
    await desktop.observeStartupFailure('owned','test',{},0);
    assert.deepEqual(calls,cancel ? ['/v1/config','/v1/releases','/v1/dependencies'] : ['/v1/config','/v1/releases','/v1/dependencies','/v1/config','/v1/releases']);
    assert.equal(launches,cancel?0:1);
  }
});
test('Desktop never scans dependencies for ready, unrelated failure, or stale operation', async t => {
  const {root}=fixture(t);
  for (const state of [
    {operationId:'owned',phase:'launched',audit:{state:'ready'}},
    {operationId:'owned',phase:'failed',detail:'plugin configuration invalid'},
    {operationId:'owned',phase:'failed',detail:'failed to import: SyntaxError'},
    {operationId:'other',phase:'failed',detail:'ERR_MODULE_NOT_FOUND'},
  ]) {
    const desktop=new HarnessDesktop({userData:root,resources:root,bridge:{request:async()=>{throw Error('must not scan');}}});
    desktop.status=()=>state;
    await desktop.observeStartupFailure('owned','test',{},0);
  }
});

test('Desktop recovery cancels when its final launch reads a different configuration or release', async t => {
  const {root}=fixture(t);
  for (const change of ['config','release']) {
    const desktop=new HarnessDesktop({userData:root,resources:root,bridge:{request:async(command,args)=>{
      if(command==='desktop_launch_context')return {available:true,data_root:root};
      if(args.path==='/v1/config')return change==='config'?{harness_preferences:{home:root}}:{};
      if(args.path==='/v1/releases')return {current_release:change==='release'?'new':'original'};
      if(args.path==='/v1/harness')return {harness:{state:'stopped'}};
      throw Error('unexpected request');
    }}});
    await assert.rejects(desktop.startOperation(true,{release:'original',config:{}}),/desktop_start_cancelled/);
    assert.equal(fs.existsSync(desktop.file),false);
    assert.equal(fs.existsSync(path.join(desktop.directory,'launch.json')),false);
    assert.equal(desktop.busy,false);
  }
});