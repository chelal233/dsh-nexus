import assert from 'node:assert/strict';

export const GIB = 2 ** 30;

export function verifyNodeIdentity(actual, version, arch) {
  assert.match(version, /^v\d+\.\d+\.\d+$/);
  assert.deepEqual(actual, { platform: 'darwin', arch, version });
}

const archiveBound = ({ bytes, entries }) => {
  for (const value of [bytes, entries]) assert.ok(Number.isSafeInteger(value) && value >= 0);
  return Math.ceil((bytes + (entries + 2) * 8192) * 1.01) + 1024 ** 2;
};
export function exportIncrement({ slot, runtime, home, host, git = runtime, desktop = { bytes: 0, entries: 0 } }) {
  // Runtime may need a separate Git copy; the complete Mac app also becomes
  // host.tar.gz inside staging, while the final compressed archive coexists.
  const parts = [slot, runtime, git, desktop, home];
  const hostArchive = archiveBound(host);
  const staged = { bytes: parts.reduce((n, p) => n + p.bytes, hostArchive),
    entries: parts.reduce((n, p) => n + p.entries, 2) };
  return archiveBound(staged) * 2 + GIB / 2;
}
export function importIncrement(preview, receiver) {
  assert.ok(preview && Number.isSafeInteger(preview.bytes) && Number.isSafeInteger(preview.files));
  // Actual archive manifest bounds extraction plus all incoming environment
  // during the merge; existing receiver contents must coexist with both.
  return archiveBound({ bytes: preview.bytes, entries: preview.files }) * 2
    + archiveBound(receiver) + GIB / 2;
}

export function selectOriginalAssets(release, arch) {
  assert.ok(['x64', 'arm64'].includes(arch));
  assert.match(release.tag_name, /^v\d+\.\d+\.\d+$/);
  assert.equal(release.draft, false);
  assert.equal(release.prerelease, false);
  const version = release.tag_name.slice(1);
  const names = ['dmg', 'portable.zip', 'build.json', 'SHA256SUMS.txt']
    .map(suffix => `dsh-nexus_${version}_macos_${arch}${suffix === 'dmg' ? '.' : '_'}${suffix}`);
  return names.map(name => {
    const matching = release.assets.filter(asset => asset.name === name);
    assert.equal(matching.length, 1, `Missing or duplicated original asset: ${name}`);
    const item = matching[0];
    assert.ok(Number.isSafeInteger(item.size) && item.size > 0);
    assert.ok(Number.isSafeInteger(item.id) && item.id > 0);
    assert.match(item.digest, /^sha256:[a-f0-9]{64}$/);
    return { id: item.id, name, bytes: item.size, sha256: item.digest.slice(7) };
  });
}

export function capacityVerdict({ free, total, downloadedBytes, limit = 2 * GIB }) {
  for (const value of [free, total, downloadedBytes, limit]) {
    assert.ok(Number.isSafeInteger(value) && value >= 0);
  }
  assert.ok(total > 0 && free <= total);
  const floor = Math.max(20 * GIB, Math.ceil(total / 10));
  // This lower bound excludes expanded apps, Harness, export, receiver and staging.
  // A passing lower bound alone must never authorize the full business suite.
  const reasons = [];
  if (downloadedBytes > limit) reasons.push('original_downloads_exceed_temp_budget');
  if (free - downloadedBytes < floor) reasons.push('original_downloads_cross_free_space_floor');
  return { status: reasons.length ? 'BLOCKED' : 'LOWER_BOUND_ONLY', free, total, floor,
    limit, downloadedBytes, reasons, fullAcceptance: 'NOT RUN',
    excludes: ['expanded app', 'Harness dependencies', 'Desktop runtime preparation',
      'complete offline export', 'receiver and import staging'] };
}
