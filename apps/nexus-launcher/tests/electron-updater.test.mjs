import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { DesktopUpdater, saveUpdateSettings } from '../electron/updater.mjs';

test('Killed settings writer preserves accepted settings and a later save ignores its residue', t => {
  const directory = mkdtempSync(path.join(tmpdir(), 'nexus-update-settings-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const settingsPath = path.join(directory, 'desktop-update.json');
  saveUpdateSettings(settingsPath, { enabled: true });
  const source = `
    import fs from 'node:fs';
    import { syncBuiltinESMExports } from 'node:module';
    import { saveUpdateSettings } from ${JSON.stringify(new URL('../electron/updater.mjs', import.meta.url).href)};
    fs.fsyncSync = () => process.kill(process.pid, 'SIGKILL');
    syncBuiltinESMExports();
    saveUpdateSettings(process.argv[1], { enabled: false });
  `;
  const child = spawnSync(process.execPath, ['--input-type=module', '-e', source, settingsPath], { timeout: 10000 });
  assert.ifError(child.error);
  assert.notEqual(child.status, 0);
  assert.deepEqual(JSON.parse(readFileSync(settingsPath)), { enabled: true });
  const residue = readdirSync(directory).filter(name => name.startsWith('.desktop-update-'));
  assert.equal(residue.length, 1);
  saveUpdateSettings(settingsPath, { enabled: false });
  assert.deepEqual(JSON.parse(readFileSync(settingsPath)), { enabled: false });
  assert.deepEqual(readdirSync(directory).filter(name => name.startsWith('.desktop-update-')), residue);
});

test('Failed settings serialization preserves accepted settings and removes only its own temporary directory', t => {
  const directory = mkdtempSync(path.join(tmpdir(), 'nexus-update-settings-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const settingsPath = path.join(directory, 'desktop-update.json');
  saveUpdateSettings(settingsPath, { enabled: true });
  const cyclic = {}; cyclic.self = cyclic;
  assert.throws(() => saveUpdateSettings(settingsPath, cyclic));
  assert.deepEqual(JSON.parse(readFileSync(settingsPath)), { enabled: true });
  assert.deepEqual(readdirSync(directory), ['desktop-update.json']);
});

function fixture(coordinate = async () => {}) {
  const native = new EventEmitter(); let installs = 0; let checks = 0;
  native.quitAndInstall = (silent, restart) => { assert.equal(silent, true); assert.equal(restart, true); installs++; };
  native.checkForUpdates = async () => { checks++; native.emit('update-not-available'); };
  const updater = new DesktopUpdater(native, { coordinate, changed() {}, settings: {}, save() {}, packaged: true });
  return { native, updater, installs: () => installs, checks: () => checks };
}
test('Full downloads and coordinated install are mandatory', async () => {
  const f = fixture();
  assert.equal(f.native.disableDifferentialDownload, true);
  assert.equal(f.native.autoInstallOnAppQuit, false);
  assert.equal(f.native.allowDowngrade, false);
  await assert.rejects(f.updater.install(), /No verified/);
  f.native.emit('update-downloaded', { version: '0.2.0' });
  await f.updater.install(); assert.equal(f.installs(), 1);
});
test('Active work or shell exit failure defers installation without losing downloaded update', async () => {
  const f = fixture(async () => { throw new Error('Harness is busy'); });
  f.native.emit('update-downloaded', { version: '0.2.0' });
  await assert.rejects(f.updater.install(), /busy/);
  assert.equal(f.installs(), 0); assert.equal(f.updater.state.phase, 'ready');
});
test('Disabled automatic updates persist and stop automatic download', () => {
  const f = fixture(); f.updater.setEnabled(false);
  assert.equal(f.updater.state.enabled, false); assert.equal(f.native.autoDownload, false);
  assert.throws(() => f.updater.setEnabled('yes'));
});
test('Installation is single-flight while process coordination is pending', async () => {
  let release; const f = fixture(() => new Promise(resolve => { release = resolve; }));
  f.native.emit('update-downloaded', { version: '0.2.0' });
  const first = f.updater.install(); await assert.rejects(f.updater.install());
  release(); await first; assert.equal(f.installs(), 1);
});

test('Automatic checks run once at startup and then every two hours', async t => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const f = fixture(); t.after(() => f.updater.stop());
  f.updater.start(); f.updater.start();
  assert.equal(f.checks(), 1);
  await Promise.resolve();
  t.mock.timers.tick(2 * 60 * 60 * 1000 - 1);
  assert.equal(f.checks(), 1);
  t.mock.timers.tick(1);
  assert.equal(f.checks(), 2);
  await Promise.resolve();
  t.mock.timers.tick(2 * 60 * 60 * 1000);
  assert.equal(f.checks(), 3);
  f.updater.stop();
  t.mock.timers.tick(4 * 60 * 60 * 1000);
  assert.equal(f.checks(), 3);
});

test('Disabled automatic updates perform neither startup nor periodic checks', t => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const f = fixture(); t.after(() => f.updater.stop());
  f.updater.setEnabled(false); f.updater.start();
  t.mock.timers.tick(6 * 60 * 60 * 1000);
  assert.equal(f.checks(), 0);
  f.updater.setEnabled(true);
  t.mock.timers.tick(2 * 60 * 60 * 1000);
  assert.equal(f.checks(), 1);
  f.updater.setEnabled(false);
  t.mock.timers.tick(2 * 60 * 60 * 1000);
  assert.equal(f.checks(), 1);
});

test('Manual checks wait for confirmation; requested downloads work with automation disabled', async () => {
  const f = fixture(); f.updater.setEnabled(false);
  let downloads = 0;
  f.native.checkForUpdates = async () => f.native.emit('update-available', { version: '0.2.0' });
  f.native.downloadUpdate = async () => {
    downloads++; f.native.emit('download-progress', { percent: 42.5 });
    assert.equal(f.updater.state.percent, 42.5);
    f.native.emit('update-downloaded', { version: '0.2.0' });
  };
  await f.updater.check(); assert.equal(downloads, 0);
  await f.updater.check({ manual: true });
  assert.equal(downloads, 0); assert.equal(f.updater.state.phase, 'available');
  await f.updater.download('0.2.0');
  assert.equal(downloads, 1); assert.equal(f.updater.state.enabled, false);
  assert.equal(f.updater.state.phase, 'ready'); assert.equal(f.installs(), 0);
});

test('A ready download waits for the user even across automatic check ticks', async t => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const f = fixture(); t.after(() => f.updater.stop());
  f.native.emit('update-downloaded', { version: '0.2.0' });
  f.updater.start(); t.mock.timers.tick(4 * 60 * 60 * 1000);
  assert.equal(f.installs(), 0); assert.equal(f.checks(), 0);
});

test('Automatic and repeated manual checks never download without confirmation', async () => {
  const f = fixture(); let release; let downloads = 0;
  f.native.checkForUpdates = async () => {
    await new Promise(resolve => { release = resolve; });
    f.native.emit('update-available', { version: '0.2.0' });
  };
  f.native.downloadUpdate = async () => { downloads++; f.native.emit('update-downloaded', { version: '0.2.0' }); };
  const automatic = f.updater.check(); f.updater.setEnabled(false); release(); await automatic;
  assert.equal(downloads, 0); assert.equal(f.updater.state.phase, 'available');
  const manual = f.updater.check({ manual: true }); release(); await manual;
  assert.equal(downloads, 0);
  await f.updater.download('0.2.0');
  assert.equal(downloads, 1); assert.equal(f.updater.state.enabled, false);
});

test('Installer error events and throws release coordination before allowing retry', async () => {
  for (const throws of [false, true]) {
    const native = new EventEmitter(); let locked = false; let cancelled = 0;
    const updater = new DesktopUpdater(native, {
      coordinate: async () => { locked = true; },
      cancel: async () => { cancelled++; locked = false; },
      changed() {}, settings: {}, save() {}, packaged: true,
    });
    native.quitAndInstall = () => {
      const error = new Error('installer failed');
      native.emit('error', error);
      if (throws) throw error;
    };
    native.emit('update-downloaded', { version: '0.2.0' });
    if (throws) await assert.rejects(updater.install(), /installer failed/);
    else await updater.install();
    await updater.recovery;
    assert.equal(locked, false); assert.equal(cancelled, 1);
    assert.equal(updater.state.phase, 'ready');
    assert.match(updater.state.error, /installer failed/);
  }
});


test('confirmation rejects stale versions and coalesces download clicks; failures require a new check', async () => {
  const f=fixture();let downloads=0,release;
  f.native.checkForUpdates=async()=>f.native.emit('update-available',{version:'0.2.0'});
  f.native.downloadUpdate=()=>{downloads++;return new Promise((resolve,reject)=>{release=reject;});};
  await f.updater.check(); assert.equal(downloads,0);
  await assert.rejects(f.updater.download('0.1.9'),/selection changed/);
  const first=f.updater.download('0.2.0');
  await f.updater.download('0.2.0');assert.equal(downloads,1);
  await assert.rejects(f.updater.install(),/No verified/);
  release(new Error('network interrupted'));await assert.rejects(first,/network interrupted/);
  assert.equal(f.updater.state.phase,'error');
  await assert.rejects(f.updater.download('0.2.0'),/Check for an available/);
  await f.updater.check({manual:true});assert.equal(f.updater.state.phase,'available');
});
