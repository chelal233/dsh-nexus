import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import helperModule from 'electron-updater/out/DownloadedUpdateHelper.js';

const { DownloadedUpdateHelper } = helperModule;
const logger = { info() {}, warn() {} };
const digest = bytes => createHash('sha512').update(bytes).digest('base64');
function fixture(t) {
  const root = mkdtempSync(path.join(tmpdir(), 'nexus-update-cache-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const pending = path.join(root, 'pending'); mkdirSync(pending);
  return { root, pending, helper: new DownloadedUpdateHelper(root) };
}
test('Killed v1.1 download is never accepted after restart; v1.2 must pass its own checksum', async t => {
  const { root, pending, helper } = fixture(t);
  const partial = path.join(pending, 'temp-v1.1.exe');
  const child = spawnSync(process.execPath, ['-e', `const fs=require('fs');const fd=fs.openSync(process.argv[1],'w');fs.writeSync(fd,'partial v1.1');fs.fsyncSync(fd);process.kill(process.pid,'SIGKILL')`, partial]);
  assert.notEqual(child.status, 0); assert.equal(readFileSync(partial, 'utf8'), 'partial v1.1');
  const bytes = Buffer.from('complete v1.2 installer');
  const info = { info: { sha512: digest(bytes) } };
  const target = path.join(pending, 'v1.2.exe');
  assert.equal(await helper.validateDownloadedPath(target, { version: '1.2.0' }, info, logger), null);
  assert.equal(helper.file, null);
  writeFileSync(target, bytes);
  await helper.setDownloadedFile(target, null, { version: '1.2.0' }, info, 'v1.2.exe', true);
  const restarted = new DownloadedUpdateHelper(root);
  assert.equal(await restarted.validateDownloadedPath(target, { version: '1.2.0' }, info, logger), target);
  assert.notEqual(restarted.file, partial);
});
test('A complete older cache is discarded when the newly published version has another checksum', async t => {
  const { root, pending, helper } = fixture(t);
  const old = path.join(pending, 'v1.1.exe'); writeFileSync(old, 'old version');
  await helper.setDownloadedFile(old, null, { version: '1.1.0' }, { info: { sha512: digest('old version') } }, 'v1.1.exe', true);
  const restarted = new DownloadedUpdateHelper(root);
  assert.equal(await restarted.validateDownloadedPath(path.join(pending, 'v1.2.exe'), { version: '1.2.0' }, { info: { sha512: digest('new version') } }, logger), null);
  assert.equal(existsSync(old), false); assert.equal(restarted.file, null);
});
test('Truncated installer or interrupted cache metadata is never treated as a verified download', async t => {
  const { root, pending, helper } = fixture(t);
  const target = path.join(pending, 'v1.2.exe');
  const info = { info: { sha512: digest('complete') } };
  writeFileSync(target, 'incomplete');
  await helper.setDownloadedFile(target, null, { version: '1.2.0' }, info, 'v1.2.exe', true);
  assert.equal(await new DownloadedUpdateHelper(root).validateDownloadedPath(target, { version: '1.2.0' }, info, logger), null);
  writeFileSync(path.join(pending, 'update-info.json'), '{"fileName":');
  assert.equal(await new DownloadedUpdateHelper(root).validateDownloadedPath(target, { version: '1.2.0' }, info, logger), null);
});
