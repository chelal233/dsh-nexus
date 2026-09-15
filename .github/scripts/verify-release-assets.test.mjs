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
  const names = {
    'x86_64-pc-windows-msvc': 'windows_x64',
    'i686-pc-windows-msvc': 'windows_x86',
    'aarch64-pc-windows-msvc': 'windows_arm64',
    'x86_64-apple-darwin': 'macos_x64',
    'aarch64-apple-darwin': 'macos_arm64',
  };
  try {
    for (const [target, extensions] of Object.entries(targets)) {
      const basename = `dsh-nexus_0.1.2_${names[target]}`;
      const files = extensions.map(ext => {
        const name = `${basename}${ext}`;
        writeFileSync(path.join(dir, name), 'test installer');
        return { name, sha256: createHash('sha256').update('test installer').digest('hex') };
      });
      writeFileSync(path.join(dir, `${basename}_build.json`), JSON.stringify({ target, version: '0.1.2', commit,
        buildId: 'test', automatedChecks: 'passed', installedPackageSmoke: 'passed-on-ci-runner', files }));
      writeFileSync(path.join(dir, `${basename}_SHA256SUMS.txt`), files.map(f => `${f.sha256}  ${f.name}\n`).join(''));
    }
    assert.equal(verifyReleaseAssets(dir, 'v0.1.2', commit).length, 15);
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.3', commit));
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', 'b'.repeat(40)));
    const metadata = path.join(dir, 'dsh-nexus_0.1.2_windows_x64_build.json');
    const original = readFileSync(metadata);
    rmSync(metadata);
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Missing or repeated release asset/);
    const untested = JSON.parse(original);
    untested.installedPackageSmoke = 'not-performed';
    writeFileSync(metadata, JSON.stringify(untested));
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit));
    const wrongName = JSON.parse(original);
    wrongName.files[0].name = 'dsh-nexus_0.1.2_windows_arm64.exe';
    writeFileSync(metadata, JSON.stringify(wrongName));
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit));
    const unidentified = JSON.parse(original);
    delete unidentified.buildId;
    writeFileSync(metadata, JSON.stringify(unidentified));
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Missing build identity/);
    writeFileSync(metadata, original);
    const extra = path.join(dir, 'private-debug.log');
    writeFileSync(extra, 'not a release file');
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Unexpected release assets/);
    rmSync(extra);
    writeFileSync(path.join(dir, 'dsh-nexus_0.1.2_windows_x64.exe'), 'corrupted');
    assert.throws(() => verifyReleaseAssets(dir, 'v0.1.2', commit), /Hash mismatch/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
