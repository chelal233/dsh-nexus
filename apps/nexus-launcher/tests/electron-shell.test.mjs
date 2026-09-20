import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { NoticeCoordinator } from '../electron/notice-coordinator.mjs';

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
