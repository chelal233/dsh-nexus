import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { verifyReleaseAssets } from './verify-release-assets.mjs';

test('release gate requires five tested targets, exact provenance and unmodified files', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'nexus-release-assets-'));
  const commit = 'a'.repeat(40);
  const targets = {
    'x86_64-pc-windows-msvc': ['.exe'],
    'i686-pc-windows-msvc': ['.exe'],
    'aarch64-pc-windows-msvc': ['.exe'],
    'x86_64-apple-darwin': ['.dmg'],
    'aarch64-apple-darwin': ['.dmg'],
  };
  try {
    for (const [target, extensions] of Object.entries(targets)) {
      const files = extensions.map((ext, i) => {
        const name = `${target}_test_${i}${ext}`;
        writeFileSync(path.join(dir, name), 'test installer');
        return { name, sha256: createHash('sha256').update('test installer').digest('hex') };
      });
      writeFileSync(path.join(dir, `${target}_build.json`), JSON.stringify({ target, version: '0.1.2', commit,
        buildId: 'test', automatedChecks: 'passed', installedPackageSmoke: 'passed-on-ci-runner', files }));
      writeFileSync(path.join(dir, `${target}_SHA256SUMS.txt`), files.map(f => `${f.sha256}  ${f.name}\n`).join(''));
    }
    assert.equal(verifyReleaseAssets(dir, 'v0.1.2', commit).length, 15);
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.3', commit));
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', 'b'.repeat(40)));
    const metadata = path.join(dir, 'x86_64-pc-windows-msvc_build.json');
    const original = readFileSync(metadata);
    rmSync(metadata);
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Missing or repeated release asset/);
    const untested = JSON.parse(original);
    untested.installedPackageSmoke = 'not-performed';
    writeFileSync(metadata, JSON.stringify(untested));
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit));
    writeFileSync(metadata, original);
    const extra = path.join(dir, 'private-debug.log');
    writeFileSync(extra, 'not a release file');
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Unexpected release assets/);
    rmSync(extra);
    writeFileSync(path.join(dir, 'x86_64-pc-windows-msvc_test_0.exe'), 'corrupted');
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Hash mismatch/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
