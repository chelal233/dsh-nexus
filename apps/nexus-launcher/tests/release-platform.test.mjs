import { test } from 'node:test';
import assert from 'node:assert/strict';
import { collectedUpdateChannel, releaseBasename, selectPlatform, targets, updateChannelFile } from '../desktop/scripts/release-platform.mjs';

test('collection rewrites every Linux channel reference to the delivered package name', () => {
  const source = `version: 1.0.3\nfiles:\n  - url: old_aarch64.rpm\n    sha512: SAME-RPM-HASH\n    size: 175193761\n  - url: 'old_arm64.AppImage'\n    sha512: SAME-APP-HASH\npath: old_arm64.AppImage\nsha512: SAME-APP-HASH\n`;
  const names = new Map([['old_aarch64.rpm', 'dsh-nexus_1.0.3_linux_arm64.rpm'], ['old_arm64.AppImage', 'dsh-nexus_1.0.3_linux_arm64.AppImage']]);
  const rewritten = collectedUpdateChannel(source, names);
  assert.equal(rewritten, source.replaceAll('old_aarch64.rpm', names.get('old_aarch64.rpm')).replaceAll("'old_arm64.AppImage'", names.get('old_arm64.AppImage')).replaceAll('old_arm64.AppImage', names.get('old_arm64.AppImage')));
  assert.throws(() => collectedUpdateChannel(source, new Map()), /Unknown update asset/);
  assert.throws(() => collectedUpdateChannel('files:\n  - url: https://elsewhere.invalid/pkg.rpm\n', names), /Unknown update asset/);
  assert.throws(() => collectedUpdateChannel('version: 1.0.3\n', names), /no package references/);
});

test('all products select their native Node archive and packaging format', () => {
  for (const [target, spec] of Object.entries(targets)) {
    assert.equal(selectPlatform(target, spec.platform, spec.arch).target, target);
    assert.match(spec.sha256, /^[a-f0-9]{64}$/);
  }
  assert.throws(() => selectPlatform('i686-pc-windows-msvc', 'win32', 'x64'), /Unsupported/);
  for (const spec of Object.values(targets)) {
    assert.deepEqual(spec.bundles, spec.platform === 'win32' ? ['nsis', 'zip'] : spec.platform === 'darwin' ? ['dmg', 'zip'] : ['AppImage', 'deb', 'rpm']);
  }
});

test('unsupported or mismatched targets fail before staging host binaries', () => {
  assert.throws(() => selectPlatform('aarch64-apple-darwin', 'darwin', 'x64'), /native runner/);
  assert.throws(() => selectPlatform('aarch64-pc-windows-msvc', 'win32', 'x64'), /native runner/);
  assert.throws(() => selectPlatform('i686-apple-darwin', 'darwin', 'x64'), /Unsupported/);
  assert.throws(() => selectPlatform(undefined, 'linux', 'arm'), /Unsupported/);
  assert.throws(() => selectPlatform('x86_64-unknown-linux-gnu', 'linux', 'arm64'), /native runner/);
  assert.throws(() => selectPlatform('aarch64-unknown-linux-gnu', 'linux', 'x64'), /native runner/);
  assert.equal(updateChannelFile(selectPlatform(undefined, 'linux', 'x64')), 'latest-x64-linux.yml');
  assert.equal(updateChannelFile(selectPlatform(undefined, 'linux', 'arm64')), 'latest-arm64-linux-arm64.yml');
});

test('public download names identify product, version, system and architecture', () => {
  const expected = ['windows_x64', 'windows_arm64', 'macos_x64', 'macos_arm64', 'linux_arm64', 'linux_x64'];
  assert.deepEqual(Object.keys(targets).map(target => releaseBasename(target, '0.1.3')),
    expected.map(suffix => 'dsh-nexus_0.1.3_' + suffix));
  assert.equal(releaseBasename('x86_64-pc-windows-msvc', '0.1.3-rc.1'), 'dsh-nexus_0.1.3-rc.1_windows_x64');
  assert.throws(() => releaseBasename('unsupported', '0.1.3'));
  assert.throws(() => releaseBasename('x86_64-pc-windows-msvc', '../0.1.3'));
});
