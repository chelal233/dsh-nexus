import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { releaseBasename, targets } from '../src-tauri/scripts/release-platform.mjs';

assert.ok(process.argv[2], 'Usage: node scripts/ab-release-names.mjs <saved-script-directory>');
const before = path.resolve(process.argv[2]);
const after = fileURLToPath(new URL('../src-tauri/scripts/', import.meta.url));
const cases = mkdtempSync(path.join(before, 'cases-'));
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const inputs = ['collect-release.mjs', 'release-platform.mjs'];
const sourceHashes = directory => Object.fromEntries(inputs.map(name => [name, sha(readFileSync(path.join(directory, name)))]));

function collect(source, target, spec, side) {
  const root = path.join(cases, `${side}-${target}`);
  const app = path.join(root, 'apps/nexus-launcher');
  const scripts = path.join(app, 'src-tauri/scripts');
  const resources = path.join(app, 'src-tauri/resources');
  const targetDirectory = path.join(root, 'bundle-input');
  mkdirSync(scripts, { recursive: true });
  mkdirSync(resources, { recursive: true });
  for (const name of inputs) {
    let code = readFileSync(path.join(source, name), 'utf8');
    // Only emulate the host selector: these fixtures copy bytes, never run installers.
    if (name === 'release-platform.mjs') {
      code = `const process = ${JSON.stringify({ platform: spec.platform, arch: spec.arch })};\n${code}`;
    }
    writeFileSync(path.join(scripts, name), code);
  }
  const identity = { version: '0.1.3', buildId: 'ci-fixture-1', commit: 'a'.repeat(40), dirty: false,
    node: spec.nodeVersion, npm: 'fixture', pnpm: 'fixture' };
  writeFileSync(path.join(resources, 'release-identity.json'), JSON.stringify(identity));
  const extension = spec.platform === 'win32' ? '.exe' : '.dmg';
  const kind = spec.bundles[0];
  const directory = path.join(targetDirectory, target, 'release/bundle', kind);
  mkdirSync(directory, { recursive: true });
  const installer = Buffer.from(`unchanged installer fixture: ${target}\0\xff`, 'utf8');
  writeFileSync(path.join(directory, `Nexus Launcher_0.1.3_fixture${extension}`), installer);
  execFileSync(process.execPath, [path.join(scripts, 'collect-release.mjs')], {
    env: { ...process.env, CARGO_BUILD_TARGET: target, CARGO_TARGET_DIR: targetDirectory,
      GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v0.1.3' }, windowsHide: true,
  });
  const output = path.join(root, 'target/release-assets', target);
  const inventory = readdirSync(output);
  assert.equal(inventory.length, 3);
  const metadata = inventory.find(name => name.endsWith('_build.json'));
  const checksums = inventory.find(name => name.endsWith('_SHA256SUMS.txt'));
  const build = JSON.parse(readFileSync(path.join(output, metadata), 'utf8'));
  assert.equal(build.files.length, 1);
  const file = build.files[0];
  assert.deepEqual(readFileSync(path.join(output, file.name)), installer);
  assert.equal(file.sha256, sha(installer));
  assert.equal(readFileSync(path.join(output, checksums), 'utf8'), `${file.sha256}  ${file.name}\n`);
  if (side === 'B') {
    const basename = releaseBasename(target, identity.version);
    assert.equal(file.name, basename + extension);
    assert.equal(metadata, basename + '_build.json');
    assert.equal(checksums, basename + '_SHA256SUMS.txt');
  }
  const filename = file.name;
  file.name = '<installer filename>';
  return { filename, build };
}

const report = { status: 'passed', baseline: sourceHashes(before), candidate: sourceHashes(after), targets: [] };
for (const [target, spec] of Object.entries(targets)) {
  const a = collect(before, target, spec, 'A');
  const b = collect(after, target, spec, 'B');
  assert.deepEqual(b.build, a.build, 'Only the filename may change in build metadata');
  report.targets.push({ target, before: a.filename, after: b.filename, bytesAndProvenanceIdentical: true });
}
writeFileSync(path.join(before, 'comparison.json'), JSON.stringify(report, null, 2));
console.log(JSON.stringify(report, null, 2));
