import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { bundleFormats, targets } from '../../apps/nexus-launcher/src-tauri/scripts/release-platform.mjs';

export function verifyReleaseAssets(directory, tag, commit) {
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
  const files = [...inventory.values()];
  function take(name) {
    assert.ok(inventory.has(name), `Missing or repeated release asset: ${name}`);
    const file = inventory.get(name);
    inventory.delete(name);
    return readFileSync(file);
  }

  for (const [target, spec] of Object.entries(targets)) {
    const metadata = `${target}_build.json`;
    const checksums = `${target}_SHA256SUMS.txt`;
    const build = JSON.parse(take(metadata));
    assert.equal(build.target, target);
    assert.equal(`v${build.version}`, tag);
    assert.equal(build.commit, commit);
    assert.equal(build.automatedChecks, 'passed');
    assert.equal(build.installedPackageSmoke, 'passed-on-ci-runner');
    const extensions = spec.bundles.flatMap(kind => {
      const { extension, count } = bundleFormats[kind];
      return Array(count).fill(extension);
    });
    assert.deepEqual(build.files.map(f => path.extname(f.name)).sort(), extensions.sort());
    for (const file of build.files) {
      assert.ok(file.name.startsWith(`${target}_${build.buildId}_`));
      assert.ok(!/[\\/\r\n]/.test(file.name));
      assert.match(file.sha256, /^[a-f0-9]{64}$/);
      const actual = createHash('sha256').update(take(file.name)).digest('hex');
      assert.equal(actual, file.sha256, `Hash mismatch: ${file.name}`);
    }
    assert.equal(take(checksums).toString('utf8').replaceAll('\r\n', '\n'),
      build.files.map(f => `${f.sha256}  ${f.name}\n`).join(''));
  }
  assert.equal(inventory.size, 0, `Unexpected release assets: ${[...inventory.keys()].join(', ')}`);
  return files;
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const files = verifyReleaseAssets(...process.argv.slice(2));
  console.log(`Verified ${files.length} release assets across all five architectures`);
}
