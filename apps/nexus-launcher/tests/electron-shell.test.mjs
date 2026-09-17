import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { ShellController } from '../electron/shell-controller.mjs';
import { NoticeCoordinator } from '../electron/notice-coordinator.mjs';

test('shell recovers from missing service and follows a replaced port/token without reload loops', async () => {
  let info = { url: 'http://127.0.0.1:1234/?token=a' }, fail = false;
  const loaded = []; let recovered = 0;
  const controller = new ShellController({ discover: async () => { if (fail) throw Error('offline'); return info; }, load: async url => loaded.push(url), recover: async () => recovered++ });
  await controller.tick(); await controller.tick(); assert.equal(loaded.length, 1);
  fail = true; await controller.tick(); await controller.tick(); assert.equal(recovered, 1);
  fail = false; info = { url: 'http://127.0.0.1:5678/?token=b' }; await controller.tick();
  assert.equal(loaded.length, 2); assert.equal(controller.current.port, '5678');
  await controller.broken(); await controller.tick(); assert.equal(loaded.length, 2);
  await controller.retry(); assert.equal(loaded.length, 3);
  controller.stop(); await controller.retry(); assert.equal(loaded.length, 3);
});

test('shell never loads a remote or malformed URL and serializes pending discovery', async () => {
  let finish, count = 0;
  const controller = new ShellController({ discover: () => new Promise(resolve => { count++; finish = resolve; }), load: () => assert.fail('untrusted navigation'), recover: async () => {} });
  const first = controller.tick(); await controller.tick(); assert.equal(count, 1);
  finish({ url: 'https://example.com' }); await first; assert.equal(controller.failed, true);
});

test('renderer failure during navigation remains failed and a restarted same-URL service can recover', async () => {
  let finish, online = true;
  const controller = new ShellController({ discover: async () => ({ url: 'http://127.0.0.1:1234/', available: online }),
    load: () => new Promise(resolve => { finish = resolve; }), recover: async () => {} });
  const loading = controller.tick(); await Promise.resolve(); await Promise.resolve();
  await controller.broken(); finish(); await loading; assert.equal(controller.failed, true);
  online = false; await controller.tick(); online = true;
  const restarted = controller.tick(); await Promise.resolve(); await Promise.resolve(); finish(); await restarted;
  assert.equal(controller.failed, false);
});

test('two hosts combine focus, expire crashed hosts and deliver each event only once', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-notice-'));
  let now = 10000;
  const launcher = new NoticeCoordinator(root, 'launcher', () => now);
  const shell = new NoticeCoordinator(root, 'shell', () => now);
  try {
    launcher.heartbeat(false); shell.heartbeat(true); assert.equal(launcher.focused(), true);
    const event = { sequence: 1, kind: 'completed', id: 'task' };
    assert.equal(shell.claim('a', event), true); assert.equal(launcher.claim('a', event), false);
    launcher.close(); assert.equal(shell.claim('a', { ...event, sequence: 2 }), true);
    now += 6000; launcher.heartbeat(false); assert.equal(launcher.focused(), false);
    assert.equal(launcher.claim('b', event), true);
  } finally { launcher.close(); shell.close(); fs.rmSync(root, { recursive: true, force: true }); }
});
