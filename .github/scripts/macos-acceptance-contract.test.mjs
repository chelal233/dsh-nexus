import assert from 'node:assert/strict';
import { test } from 'node:test';
import { GIB, selectOriginalAssets, capacityVerdict, verifyNodeIdentity, exportIncrement, importIncrement } from './macos-acceptance-contract.mjs';

const fixture = () => ({ tag_name: 'v1.0.4', draft: false, prerelease: false,
  assets: ['dsh-nexus_1.0.4_macos_x64.dmg', 'dsh-nexus_1.0.4_macos_x64_portable.zip',
    'dsh-nexus_1.0.4_macos_x64_build.json', 'dsh-nexus_1.0.4_macos_x64_SHA256SUMS.txt']
    .map((name, i) => ({ name, id: i + 1, size: 100, digest: 'sha256:' + 'a'.repeat(64) })) });

test('asset identity rejects a missing architecture, duplicate or malformed hash', () => {
  assert.equal(selectOriginalAssets(fixture(), 'x64').length, 4);
  assert.throws(() => selectOriginalAssets(fixture(), 'arm64'));
  const duplicate = fixture(); duplicate.assets.push(duplicate.assets[0]);
  assert.throws(() => selectOriginalAssets(duplicate, 'x64'));
  const malformed = fixture(); malformed.assets[0].digest = 'sha256:invalid';
  assert.throws(() => selectOriginalAssets(malformed, 'x64'));
});

test('14 GiB free cannot satisfy the 20 GiB floor even before expansion', () => {
  const result = capacityVerdict({ free: 14 * GIB, total: 100 * GIB, downloadedBytes: GIB });
  assert.equal(result.status, 'BLOCKED');
  assert.deepEqual(result.reasons, ['original_downloads_cross_free_space_floor']);
  assert.equal(result.fullAcceptance, 'NOT RUN');
});

test('free space uses ten percent when greater and does not waive the task budget', () => {
  const result = capacityVerdict({ free: 100 * GIB, total: 400 * GIB, downloadedBytes: 3 * GIB });
  assert.equal(result.floor, 40 * GIB);
  assert.deepEqual(result.reasons, ['original_downloads_exceed_temp_budget']);
});

test('a passing download lower bound never means full migration was admitted', () => {
  const result = capacityVerdict({ free: 80 * GIB, total: 300 * GIB, downloadedBytes: GIB });
  assert.equal(result.status, 'LOWER_BOUND_ONLY');
  assert.equal(result.fullAcceptance, 'NOT RUN');
  assert.ok(result.excludes.includes('receiver and import staging'));
});

test('original runtime version already includes v and is compared without rewriting', () => {
  verifyNodeIdentity({ platform: 'darwin', arch: 'arm64', version: 'v24.20.0' }, 'v24.20.0', 'arm64');
  assert.throws(() => verifyNodeIdentity({ platform: 'darwin', arch: 'arm64', version: 'v24.20.0' }, 'vv24.20.0', 'arm64'));
});

test('complete Mac host and simultaneous archives contribute to export admission', () => {
  const part = { bytes: GIB, entries: 100 };
  const estimate = exportIncrement({ slot: part, runtime: part, home: part, host: { bytes: 3 * GIB, entries: 500 } });
  assert.ok(estimate > 14 * GIB);
});

test('import admission requires actual manifest totals and retains receiver allocation', () => {
  const receiver = { bytes: GIB, entries: 100 };
  assert.ok(importIncrement({ bytes: 5 * GIB, files: 1000 }, receiver) > 11 * GIB);
  assert.throws(() => importIncrement(undefined, receiver));
  assert.throws(() => importIncrement({ bytes: -1, files: 1 }, receiver));
});
