import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { GIB } from './macos-acceptance-contract.mjs';

export async function ownedDirectory(root, directory) {
  const realRoot = await fs.realpath(root), real = await fs.realpath(directory);
  const relative = path.relative(realRoot, real);
  assert.ok(!relative.startsWith('..') && !path.isAbsolute(relative), 'QA directory escaped');
  assert.equal((await fs.lstat(directory)).isSymbolicLink(), false);
  assert.equal((await fs.stat(directory)).isDirectory(), true);
  return real;
}

export function allocatedBytes(directory) {
  const output = execFileSync('/usr/bin/du', ['-sk', directory], { encoding: 'utf8', timeout: 30000, maxBuffer: 8192 });
  const value = Number(output.trim().split(/\s/)[0]) * 1024;
  assert.ok(Number.isSafeInteger(value) && value >= 0);
  return value;
}

export function storageGate(root, evidence) {
  const limitGiB = Number(process.env.QA_TEMP_BUDGET_GIB || 2);
  const armFloor = Number(process.env.QA_ARM_FREE_GIB || 0);
  assert.ok([2, 32].includes(limitGiB), 'Only default or specifically approved Mac envelope');
  assert.ok([0, 8].includes(armFloor), 'Only default or specifically approved ARM floor');
  const limit = limitGiB * GIB; let peak = 0, last = null, measuredAt = 0;
  return async (stage, increment = 0, force = false) => {
    assert.ok(Number.isSafeInteger(increment) && increment >= 0);
    if (force || !last || Date.now() - measuredAt > 3000 || increment) {
      await ownedDirectory(root, root);
      const info = await fs.statfs(root), occupied = allocatedBytes(root);
      const free = info.bavail * info.bsize, total = info.blocks * info.bsize;
      const ordinaryFloor = Math.max(20 * GIB, Math.ceil(total / 10));
      const floor = process.arch === 'arm64' && armFloor === 8 ? 8 * GIB : ordinaryFloor;
      peak = Math.max(peak, occupied);
      last = { stage, occupied, increment, peak, free, total, floor, ordinaryFloor, limit,
        approvedArmException: process.arch === 'arm64' && armFloor === 8,
        ok: occupied + increment <= limit - GIB && free - increment >= floor + GIB };
      measuredAt = Date.now();
      await fs.appendFile(path.join(evidence, 'storage.jsonl'), JSON.stringify(last) + '\n');
    }
    if (!last.ok) throw Object.assign(new Error('Measured Mac QA capacity exceeded; no next write started'), { code: 'BUDGET_BLOCKED' });
    return last;
  };
}
