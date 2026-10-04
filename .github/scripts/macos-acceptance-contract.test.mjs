import assert from 'node:assert/strict';
import { test } from 'node:test';
import { GIB, selectOriginalAssets, capacityVerdict } from './macos-acceptance-contract.mjs';

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
