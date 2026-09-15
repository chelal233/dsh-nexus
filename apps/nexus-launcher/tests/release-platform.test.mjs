import { test } from 'node:test';
import assert from 'node:assert/strict';
import { selectPlatform, targets } from '../src-tauri/scripts/release-platform.mjs';

test('all products select their native Node archive and packaging format', () => {
  for (const [target, spec] of Object.entries(targets)) {
    assert.equal(selectPlatform(target, spec.platform, spec.arch).target, target);
    assert.match(spec.sha256, /^[a-f0-9]{64}$/);
  }
  const x86 = selectPlatform('i686-pc-windows-msvc', 'win32', 'x64');
  assert.equal(x86.nodeVersion, '22.23.2');
  assert.equal(x86.archive, 'win-x86.zip');
  assert.deepEqual(targets['aarch64-pc-windows-msvc'].bundles, ['nsis']);
});

test('unsupported or mismatched targets fail before staging host binaries', () => {
  assert.throws(() => selectPlatform('aarch64-apple-darwin', 'darwin', 'x64'), /native runner/);
  assert.throws(() => selectPlatform('aarch64-pc-windows-msvc', 'win32', 'x64'), /native runner/);
  assert.throws(() => selectPlatform('i686-apple-darwin', 'darwin', 'x64'), /Unsupported/);
  assert.throws(() => selectPlatform(undefined, 'linux', 'arm'), /Unsupported/);
});
