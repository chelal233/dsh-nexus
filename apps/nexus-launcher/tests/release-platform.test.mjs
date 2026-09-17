import { test } from 'node:test';
import assert from 'node:assert/strict';
import { releaseBasename, selectPlatform, targets } from '../desktop/scripts/release-platform.mjs';

test('all products select their native Node archive and packaging format', () => {
  for (const [target, spec] of Object.entries(targets)) {
    assert.equal(selectPlatform(target, spec.platform, spec.arch).target, target);
    assert.match(spec.sha256, /^[a-f0-9]{64}$/);
  }
  assert.throws(() => selectPlatform('i686-pc-windows-msvc', 'win32', 'x64'), /Unsupported/);
  for (const spec of Object.values(targets)) {
    assert.deepEqual(spec.bundles, spec.platform === 'win32' ? ['nsis', 'zip'] : ['dmg', 'zip']);
  }
});

test('unsupported or mismatched targets fail before staging host binaries', () => {
  assert.throws(() => selectPlatform('aarch64-apple-darwin', 'darwin', 'x64'), /native runner/);
  assert.throws(() => selectPlatform('aarch64-pc-windows-msvc', 'win32', 'x64'), /native runner/);
  assert.throws(() => selectPlatform('i686-apple-darwin', 'darwin', 'x64'), /Unsupported/);
  assert.throws(() => selectPlatform(undefined, 'linux', 'arm'), /Unsupported/);
});

test('public download names identify product, version, system and architecture', () => {
  const expected = ['windows_x64', 'windows_arm64', 'macos_x64', 'macos_arm64'];
  assert.deepEqual(Object.keys(targets).map(target => releaseBasename(target, '0.1.3')),
    expected.map(suffix => 'dsh-nexus_0.1.3_' + suffix));
  assert.equal(releaseBasename('x86_64-pc-windows-msvc', '0.1.3-rc.1'), 'dsh-nexus_0.1.3-rc.1_windows_x64');
  assert.throws(() => releaseBasename('unsupported', '0.1.3'));
  assert.throws(() => releaseBasename('x86_64-pc-windows-msvc', '../0.1.3'));
});
