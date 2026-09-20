import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readDesktopState, managedDesktopRoot, HarnessDesktop, probeDesktopSupport, desktopCapability } from '../electron/harness-desktop.mjs';
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
