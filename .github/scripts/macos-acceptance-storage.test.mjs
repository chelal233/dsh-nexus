import assert from 'node:assert/strict';
import { test } from 'node:test';
import { watchStorage, storageGate } from './macos-acceptance-storage.mjs';
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
