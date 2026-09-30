import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { apply } from '../src/index.mjs';

function fixture(jobs, sessions = new Map()) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-job-notifications-'));
  const file = path.join(root, 'events.json');
  const oldFile = process.env.NEXUS_NOTIFICATION_FILE, oldRun = process.env.NEXUS_NOTIFICATION_RUN;
  process.env.NEXUS_NOTIFICATION_FILE = file; process.env.NEXUS_NOTIFICATION_RUN = 'job-contract-test';
  const disposers = [];
  const ctx = {
    get: key => key === 'sessions' ? sessions : undefined,
    effect: setup => { const disposer = setup(); if (disposer) disposers.push(disposer); },
    inject: (names, setup) => { if (names[0] === 'jobs') setup({ ...ctx, jobs }); },
  };
  apply(ctx);
  return {
    read: () => JSON.parse(fs.readFileSync(file)),
    close: () => {
      for (const dispose of disposers.reverse()) dispose();
      if (oldFile === undefined) delete process.env.NEXUS_NOTIFICATION_FILE; else process.env.NEXUS_NOTIFICATION_FILE = oldFile;
      if (oldRun === undefined) delete process.env.NEXUS_NOTIFICATION_RUN; else process.env.NEXUS_NOTIFICATION_RUN = oldRun;
      fs.rmSync(root, { recursive: true, force: true });
    },
  };
}

test('new job event stream reports producer completions with the owning session and removes its listener', () => {
  let listener, removed = 0;
  const job = { id: 'bash-1', owner: 'user', status: 'completed', label: 'Build project' };
  const state = fixture({ events: { subscribe(filter, callback) {
    assert.deepEqual(filter, { owners: 'scope' }); listener = callback; return () => { removed++; };
  } }, onJobDone() { assert.fail('modern API must be used when both are present'); } },
    new Map([['user', { header: { id: 'user' } }], ['child', { header: { id: 'child', origin: 'subagent' } }]]));
  try {
    assert.ok(state.read().capabilities.includes('jobs'));
    listener({ type: 'registered', job });
    listener({ type: 'settled', job, cause: 'producer', awaited: true });
    listener({ type: 'settled', job, cause: 'teardown', awaited: false });
    listener({ type: 'settled', job, cause: 'kill', awaited: false });
    listener({ type: 'settled', job: { ...job, owner: 'child' }, cause: 'producer', awaited: false });
    assert.deepEqual(state.read().events, []);
    listener({ type: 'settled', job, cause: 'producer', awaited: false });
    listener({ type: 'settled', job, cause: 'producer', awaited: false });
    listener({ type: 'settled', job: { ...job, id: 'bash-2', status: 'failed', detail: 'exit code: 3' }, cause: 'producer', awaited: false });
    const events = state.read().events;
    assert.deepEqual(events.map(e => [e.kind, e.session]), [['job-completed', 'user'], ['job-failed', 'user']]);
    assert.equal(events[1].body, 'Build project — exit code: 3');
  } finally { state.close(); }
  assert.equal(removed, 1);
});

test('old job completion API still resolves the exact owner and unknown APIs stay unverified', () => {
  let listener;
  const state = fixture({ onJobDone(callback) { listener = callback; return () => {}; } }, new Map([['old', { header: { id: 'old' } }]]));
  try {
    listener({ id: 'job-1', status: 'completed' }, { id: 'old' });
    assert.equal(state.read().events[0].session, 'old');
    assert.ok(state.read().capabilities.includes('jobs'));
  } finally { state.close(); }
  const unsupported = fixture({});
  try { assert.ok(!unsupported.read().capabilities.includes('jobs')); } finally { unsupported.close(); }
});
