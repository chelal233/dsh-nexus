import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export function verifyReleaseAssets(directory, tag, commit) {
  const expected = new Map([
    ['x86_64-pc-windows-msvc', ['.exe', '.msi', '.msi']],
    ['i686-pc-windows-msvc', ['.exe', '.msi', '.msi']],
    ['aarch64-pc-windows-msvc', ['.exe']],
    ['x86_64-apple-darwin', ['.dmg']],
    ['aarch64-apple-darwin', ['.dmg']],
  ]);
  assert.match(commit, /^[a-f0-9]{40}$/);
  const inventory = new Map();
  function walk(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const file = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(file);
      else {
        assert.ok(entry.isFile(), `Not an ordinary file: ${file}`);
        assert.ok(!inventory.has(entry.name), `Duplicate asset: ${entry.name}`);
        inventory.set(entry.name, file);
      }
    }
  }
  walk(directory);
  const allowed = new Set();
  for (const [target, extensions] of expected) {
    const metadata = `${target}_build.json`;
    const checksums = `${target}_SHA256SUMS.txt`;
    const build = JSON.parse(readFileSync(inventory.get(metadata), 'utf8'));
    assert.equal(build.target, target);
    assert.equal(`v${build.version}`, tag);
    assert.equal(build.commit, commit);
    assert.equal(build.automatedChecks, 'passed');
    assert.equal(build.installedPackageSmoke, 'passed-on-ci-runner');
    assert.deepEqual(build.files.map(f => path.extname(f.name)).sort(), extensions.sort());
    allowed.add(metadata);
    allowed.add(checksums);
    for (const file of build.files) {
      assert.ok(file.name.startsWith(`${target}_${build.buildId}_`));
      assert.ok(!/[\\/\r\n]/.test(file.name));
      assert.ok(!allowed.has(file.name), `Duplicate file: ${file.name}`);
      assert.match(file.sha256, /^[a-f0-9]{64}$/);
      const actual = createHash('sha256').update(readFileSync(inventory.get(file.name))).digest('hex');
      assert.equal(actual, file.sha256, `Hash mismatch: ${file.name}`);
      allowed.add(file.name);
    }
    assert.equal(readFileSync(inventory.get(checksums), 'utf8').replaceAll('\r\n', '\n'),
      build.files.map(f => `${f.sha256}  ${f.name}\n`).join(''));
  }
  assert.deepEqual([...inventory.keys()].sort(), [...allowed].sort(), 'Unexpected release assets');
  return [...inventory.values()];
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const files = verifyReleaseAssets(...process.argv.slice(2));
  console.log(`Verified ${files.length} release assets across all five architectures`);
}
