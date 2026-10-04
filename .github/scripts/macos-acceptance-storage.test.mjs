import assert from 'node:assert/strict';
import { test } from 'node:test';
import { watchStorage, storageGate, allocatedBytes } from './macos-acceptance-storage.mjs';
import { GIB } from './macos-acceptance-contract.mjs';

test('zero-increment periodic sample cannot overwrite a rejected concurrent admission', async () => {
  let release, entered;
  const held = new Promise(resolve => { release = resolve; });
  const started = new Promise(resolve => { entered = resolve; });
  const limit = Number(process.env.QA_TEMP_BUDGET_GIB || 2) * GIB;
  const gate = storageGate('memory', 'memory', {
    measure: async () => ({ occupied: limit - GIB * 1.5, free: 100 * GIB, total: 300 * GIB }),
    append: async snapshot => { if (snapshot.stage === 'foreground') { entered(); await held; } },
  });
  const blocked = assert.rejects(gate('foreground', GIB, true), { code: 'BUDGET_BLOCKED' });
  await started;
  const periodic = await gate('periodic', 0, true); assert.equal(periodic.ok, true);
  release(); await blocked;
});

test('periodic measurement failure stops sampling and reaches the owned cancellation path once', async () => {
  const error = new Error('measurement unavailable');
  let probes = 0, failures = 0, resolve;
  const failed = new Promise(done => { resolve = done; });
  const stop = watchStorage(async () => { probes++; throw error; }, actual => {
    assert.equal(actual, error); failures++; resolve();
  }, 10);
  await failed; await stop();
  assert.equal(probes, 1); assert.equal(failures, 1);
});

test('periodic sampling serializes slow measurements and stops without leaving a timer', async () => {
  let active = 0, maximum = 0, probes = 0, resolve;
  const sampled = new Promise(done => { resolve = done; });
  const stop = watchStorage(async () => {
    active++; maximum = Math.max(maximum, active); probes++;
    await new Promise(done => setTimeout(done, 25)); active--; resolve();
  }, () => assert.fail('No failure expected'), 5);
  await sampled; await stop();
  assert.equal(maximum, 1); assert.equal(probes, 1); assert.equal(active, 0);
});

test('renamed cold subtree needs a complete successful du recount, never its partial output', async () => {
  let calls = 0; const retries = [];
  const error = Object.assign(new Error('du race'), { status: 1, stdout: '1\t/qa\n', stderr: 'du: /qa/cold/packages: No such file or directory\n' });
  const bytes = await allocatedBytes('/qa', { run: () => { if (++calls === 1) throw error; return '4096\t/qa\n'; },
    delay: async () => {}, onRetry: event => retries.push(event) });
  assert.equal(bytes, 4096 * 1024); assert.equal(calls, 2); assert.equal(retries.length, 1);
});

test('a denied probe or a missing task root is not a renamed-subtree retry', async () => {
  for (const stderr of ['du: /qa/cold: Operation not permitted\n', 'du: /qa: No such file or directory\n']) {
    let calls = 0; const error = Object.assign(new Error('du failure'), { status: 1, stderr });
    await assert.rejects(allocatedBytes('/qa', { run: () => { calls++; throw error; }, delay: async () => {} }), actual => actual === error);
    assert.equal(calls, 1);
  }
});

test('persistent subtree disappearance remains a failure after three probes', async () => {
  let calls = 0; const error = Object.assign(new Error('du race persists'), { status: 1, stderr: 'du: /qa/cold: No such file or directory\n' });
  await assert.rejects(allocatedBytes('/qa', { run: () => { calls++; throw error; }, delay: async () => {} }), actual => actual === error);
  assert.equal(calls, 3);
});

test('stopping a sampler waits for its unfinished probe before a tree can be removed', async () => {
  let enter, finish, calls = 0, drained = false;
  const entered = new Promise(resolve => { enter = resolve; });
  const pending = new Promise(resolve => { finish = resolve; });
  const stop = watchStorage(async () => { calls++; enter(); await pending; }, () => assert.fail('Unexpected violation'), 5);
  await entered;
  const stopped = stop().then(() => { drained = true; });
  await Promise.resolve(); assert.equal(drained, false);
  finish(); await stopped;
  assert.equal(drained, true); assert.equal(calls, 1);
});

test('a truncated du diagnostic remains a failure and records its bounded metadata', async () => {
  let calls = 0; const failures = [];
  const error = Object.assign(new Error('buffer exceeded'), { code: 'ENOBUFS', status: 1,
    stderr: 'du: /qa/cold: No such file or directory\n' });
  await assert.rejects(allocatedBytes('/qa', { run: () => { calls++; throw error; },
    onFailure: value => failures.push(value), delay: async () => {} }), actual => actual === error);
  assert.equal(calls, 1); assert.equal(failures[0].code, 'ENOBUFS');
  assert.equal(failures[0].stderrBytes, Buffer.byteLength(error.stderr));
});
