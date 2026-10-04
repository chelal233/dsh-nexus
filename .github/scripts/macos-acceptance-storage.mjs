import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { GIB } from './macos-acceptance-contract.mjs';

export async function ownedDirectory(root, directory) {
  const realRoot = await fs.realpath(root), real = await fs.realpath(directory);
  const relative = path.relative(realRoot, real);
  assert.ok(!relative.startsWith('..') && !path.isAbsolute(relative), 'QA directory escaped');
  assert.equal((await fs.lstat(directory)).isSymbolicLink(), false);
  assert.equal((await fs.stat(directory)).isDirectory(), true);
  assert.equal((await fs.stat(realRoot)).dev, (await fs.stat(real)).dev, 'QA path crossed target volume');
  return real;
}

export async function allocatedBytes(directory, hooks = {}) {
  const run = hooks.run || (() => execFileSync('/usr/bin/du', ['-sk', directory], {
    encoding: 'utf8', timeout: 30000, maxBuffer: 256 * 1024, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, LC_ALL: 'C' },
  }));
  for (let attempt = 1; attempt <= 3; attempt++) {
    let output;
    try { output = run(); }
    catch (error) {
      const lines = String(error.stderr || '').trim().split('\n');
      // A normal cold promotion renames a subtree while du is walking it. Only
      // retry that explicit race, never accept a partial count or denied probe.
      const movedSubtree = error.status === 1 && lines.length > 0 && lines.every(line =>
        line.startsWith('du: ' + directory + '/') && line.endsWith(': No such file or directory'));
      if (!movedSubtree || attempt === 3) throw error;
      await hooks.onRetry?.({ attempt, reason: 'subtree-disappeared-during-du', stderr: String(error.stderr).slice(0, 4096) });
      await (hooks.delay || delay)(100);
      continue;
    }
    const value = Number(output.trim().split(/\s/)[0]) * 1024;
    assert.ok(Number.isSafeInteger(value) && value >= 0);
    return value;
  }
}

export async function treeFootprint(root, directory) {
  await ownedDirectory(root, directory);
  let bytes = 0, entries = 0;
  const visited = new Set(), device = (await fs.stat(root)).dev;
  async function visit(file) {
    let stat = await fs.lstat(file);
    if (stat.isSymbolicLink()) {
      const target = await fs.realpath(file), relative = path.relative(await fs.realpath(root), target);
      assert.ok(!relative.startsWith('..') && !path.isAbsolute(relative), 'Unknown dependency link leaves measured task roots');
      return visit(target);
    }
    assert.equal(stat.dev, device, 'Measured source crossed target volume');
    const real = await fs.realpath(file); if (visited.has(real)) return; visited.add(real); entries++;
    assert.ok(entries <= 250000, 'QA footprint entry bound');
    if (stat.isDirectory()) for (const name of await fs.readdir(file)) await visit(path.join(file, name));
    else { assert.ok(stat.isFile()); bytes += stat.size; }
    assert.ok(Number.isSafeInteger(bytes));
  }
  await visit(directory); return { bytes, entries };
}

export function watchStorage(gate, onViolation, intervalMs = 2000) {
  let active = true, pending = null;
  const sample = () => {
    if (!active || pending) return;
    pending = Promise.resolve().then(() => gate('periodic-sample', 0, true)).catch(async error => {
      active = false; clearInterval(timer); await onViolation(error);
    }).finally(() => { pending = null; });
  };
  const timer = setInterval(sample, intervalMs); sample();
  return async () => { active = false; clearInterval(timer); await pending; };
}

export function storageGate(root, evidence, hooks = {}) {
  const limitGiB = Number(process.env.QA_TEMP_BUDGET_GIB || 2);
  const armFloor = Number(process.env.QA_ARM_FREE_GIB || 0);
  assert.ok([2, 32].includes(limitGiB), 'Only default or specifically approved Mac envelope');
  assert.ok([0, 8].includes(armFloor), 'Only default or specifically approved ARM floor');
  const limit = limitGiB * GIB; let peak = 0, last = null, measuredAt = 0;
  const measure = hooks.measure || (async () => {
    await ownedDirectory(root, root);
    const occupied = await allocatedBytes(root, { onRetry: snapshot =>
      fs.appendFile(path.join(evidence, 'storage-retries.jsonl'), JSON.stringify({ measuredAt: new Date().toISOString(), ...snapshot }) + '\n') });
    const info = await fs.statfs(root);
    return { occupied, free: info.bavail * info.bsize, total: info.blocks * info.bsize };
  });
  const append = hooks.append || (snapshot => fs.appendFile(path.join(evidence, 'storage.jsonl'), JSON.stringify(snapshot) + '\n'));
  return async (stage, increment = 0, force = false) => {
    assert.ok(Number.isSafeInteger(increment) && increment >= 0);
    let snapshot = last;
    if (force || !last || Date.now() - measuredAt > 3000 || increment) {
      const { occupied, free, total } = await measure();
      for (const n of [occupied, free, total]) assert.ok(Number.isSafeInteger(n) && n >= 0);
      const ordinaryFloor = Math.max(20 * GIB, Math.ceil(total / 10));
      const floor = process.arch === 'arm64' && armFloor === 8 ? 8 * GIB : ordinaryFloor;
      peak = Math.max(peak, occupied);
      snapshot = Object.freeze({ measuredAt: new Date().toISOString(), stage, occupied, increment, sampledPeak: peak, free, total, floor, ordinaryFloor, limit,
        approvedArmException: process.arch === 'arm64' && armFloor === 8,
        ok: occupied + increment <= limit - GIB && free - increment >= floor + GIB });
      last = snapshot;
      measuredAt = Date.now();
      await append(snapshot);
    }
    if (!snapshot.ok) throw Object.assign(new Error('Measured Mac QA capacity exceeded; no next write started'), { code: 'BUDGET_BLOCKED' });
    return snapshot;
  };
}
