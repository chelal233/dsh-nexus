import test from 'node:test';
import assert from 'node:assert/strict';
import { ensureSpaceBudget, modules, writeProgress } from './offline-package.mjs';
import fsSync from 'node:fs';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

test('a locked progress snapshot does not abort packaging and later updates recover', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'nexus-progress-'));
  const file = path.join(root, 'progress.json');
  try {
    assert.equal(writeProgress(file, { completed: 1 }), true);
    const busy = { ...fsSync, renameSync() { throw Object.assign(new Error('sharing violation'), { code: 'EPERM' }); } };
    assert.equal(writeProgress(file, { completed: 2 }, busy), false);
    assert.deepEqual(JSON.parse(await fs.readFile(file, 'utf8')), { completed: 1 });
    assert.equal(writeProgress(file, { completed: 3 }), true);
    assert.deepEqual(JSON.parse(await fs.readFile(file, 'utf8')), { completed: 3 });
    const unavailable = { ...fsSync, writeFileSync() { throw Object.assign(new Error('read only'), { code: 'EACCES' }); } };
    assert.equal(writeProgress(file, { completed: 4 }, unavailable), false);
    assert.deepEqual(JSON.parse(await fs.readFile(file, 'utf8')), { completed: 3 });
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});

test('Windows canonical runtime paths load nested packaging modules', { skip: process.platform !== 'win32' }, async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'nexus offline modules '));
  try {
    const npm = path.join(root, 'node/node_modules/npm');
    await fs.mkdir(npm, { recursive: true });
    await fs.writeFile(path.join(npm, 'package.json'), '{}');
    for (const name of ['tar', 'read-cmd-shim', 'cmd-shim']) {
      const directory = path.join(npm, 'node_modules', name);
      await fs.mkdir(directory, { recursive: true });
      await fs.writeFile(path.join(directory, 'index.js'), "module.exports = require('./nested.js');");
      await fs.writeFile(path.join(directory, 'nested.js'), `module.exports = ${JSON.stringify(name)};`);
    }
    assert.deepEqual(modules(path.toNamespacedPath(root)), { tar: 'tar', readShim: 'read-cmd-shim', shim: 'cmd-shim' });
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});

test('export sums staging and archive when they share the target volume', async () => {
  const entries = [{ path: 'stage', bytes: 60 }, { path: 'archive', bytes: 50 }];
  await assert.rejects(ensureSpaceBudget(entries, async () => ({ key: 'same', available: 100n, blockSize: 1n })), /Insufficient target-volume space/);
  await ensureSpaceBudget(entries, async path => ({ key: path, available: 60n, blockSize: 1n }));
});
test('caller quota and allocation units govern import budget without charging source bytes', async () => {
  await assert.rejects(ensureSpaceBudget([{ path: 'payload', bytes: 60, entries: 2 }], async () => ({ key: 'target', available: 65n, blockSize: 4n })), /68 additional/);
  await ensureSpaceBudget([{ path: 'payload', bytes: 60, entries: 2 }], async () => ({ key: 'target', available: 68n, blockSize: 4n }));
});
