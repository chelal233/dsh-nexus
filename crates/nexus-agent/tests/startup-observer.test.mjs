import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { observeSkippedBundles } from '../src/startup-observer.mjs';

test('startup observation preserves write receiver, callbacks, bytes and backpressure', () => {
  const writes = [];
  const stream = { write(...args) { assert.equal(this, stream); writes.push(args); return false; } };
  const original = stream.write, seal = observeSkippedBundles([stream]), callback = () => {};
  const line = Buffer.from('dsh: skipping profile bundle "旧插件": 接口不存在\n');
  const split = line.indexOf(Buffer.from('旧')) + 1;
  assert.equal(stream.write(line.subarray(0, split), callback), false);
  stream.write(line.subarray(split));
  stream.write(line);
  stream.write('ordinary background output\n', 'utf8', callback);
  assert.deepEqual(writes[0][0], line.subarray(0, split));
  assert.equal(writes[0][1], callback);
  assert.equal(writes[3][1], 'utf8');
  assert.equal(writes[3][2], callback);
  assert.deepEqual(seal(), { entries: [{ package: '旧插件', reason: '接口不存在' }], truncated: true });
  assert.equal(stream.write, original);
  stream.write('dsh: skipping profile bundle "later": after readiness\n');
  assert.equal(seal().entries.length, 1);
});

test('multiline reasons flag omitted text across chunks without attributing another stream', () => {
  const stderr = { write() { return true; } }, stdout = { write() { return true; } };
  const seal = observeSkippedBundles([stderr, stdout]);
  stderr.write('dsh: skipping profile bundle "addon": first\r');
  stderr.write('\n\nsec');
  stdout.write('separate stdout log\n');
  stderr.write('ond\n');
  assert.deepEqual(seal(), { entries: [{ package: 'addon', reason: 'first' }], truncated: true });

  const second = observeSkippedBundles([stderr, stdout]);
  stderr.write('dsh: skipping profile bundle "addon": one line\n');
  stdout.write('unrelated stdout\n');
  assert.deepEqual(second(), { entries: [{ package: 'addon', reason: 'one line' }], truncated: false });
});

test('startup evidence bounds lines, rows and multilingual bytes', () => {
  const stream = { write() { return true; } }, seal = observeSkippedBundles([stream]);
  stream.write('unrelated'.repeat(4000));
  stream.write('\ndsh: skipping profile bundle "first": real warning\r\n');
  for (let i = 0; i < 60; i++) stream.write(`dsh: skipping profile bundle "p${i}": ${'界'.repeat(400)}\n`);
  stream.write(`dsh: skipping profile bundle "oversized": ${'x'.repeat(20000)}\n`);
  const result = seal();
  assert.equal(result.entries[0].package, 'first');
  assert.equal(result.truncated, true);
  assert.ok(result.entries.length <= 48);
  assert.ok(Buffer.byteLength(JSON.stringify(result.entries)) <= 8000);
});

test('sealing does not remove a later wrapper installed by the host', () => {
  let writes = 0;
  const stream = { write() { writes++; return true; } }, seal = observeSkippedBundles([stream]);
  const observed = stream.write;
  const later = function (...args) { return Reflect.apply(observed, this, args); };
  stream.write = later;
  seal();
  assert.equal(stream.write, later);
  assert.equal(stream.write('dsh: skipping profile bundle "late": ignored\n'), true);
  assert.equal(writes, 1);
  assert.equal(seal().entries.length, 0);
});

test('real Node preload and official ready callback retain evidence beyond the log tail', t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-observer-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const file = path.join(root, 'host-startup.json');
  const bridge = new URL('../../../plugins/nexus-desktop-bridge/index.mjs', import.meta.url).href;
  const script = `
    import { observeHostStartup } from ${JSON.stringify(bridge)};
    const warning = Buffer.from('dsh: skipping profile bundle "示例": export removed\\n');
    process.stderr.write(warning.subarray(0, 35)); process.stderr.write(warning.subarray(35));
    let commit;
    observeHostStartup({inject:(_names, apply)=>apply({effect:fn=>fn(),appReady:{onReady:fn=>{commit=fn;return()=>{};}}})},
      process.env.NEXUS_HOST_STARTUP_FILE, process.env.NEXUS_BROWSER_HEALTH_RUN);
    commit();
    process.stderr.write('dsh: skipping profile bundle "after-ready": background\\n');
    process.stdout.write('normal log output\\n'.repeat(20000));
  `;
  const result = spawnSync(process.execPath, ['--import', new URL('../src/startup-observer.mjs', import.meta.url).href,
    '--input-type=module', '--eval', script], { encoding: 'utf8', maxBuffer: 1024 * 1024,
    env: { ...process.env, NEXUS_HOST_STARTUP_FILE: file, NEXUS_BROWSER_HEALTH_RUN: 'current-run' } });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  const ready = JSON.parse(fs.readFileSync(file, 'utf8'));
  assert.equal(ready.run, 'current-run'); assert.equal(ready.state, 'ready');
  assert.ok(ready.pid > 1);
  assert.deepEqual(ready.skipped_bundles, { entries: [{ package: '示例', reason: 'export removed' }], truncated: false });
  assert.ok(result.stdout.length > 256000);
});
