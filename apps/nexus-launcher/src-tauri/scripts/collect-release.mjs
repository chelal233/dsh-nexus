import { createHash } from 'node:crypto';
import { copyFile, mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { bundleFormats, selectPlatform } from './release-platform.mjs';

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const root = path.resolve(app, '../..');
const spec = selectPlatform(process.env.CARGO_BUILD_TARGET);
const identity = JSON.parse(await readFile(path.join(app, 'src-tauri/resources/release-identity.json'), 'utf8'));
if (identity.dirty) throw new Error('Release artifacts must come from a clean committed checkout');
const tag = process.env.GITHUB_REF_TYPE === 'tag' ? process.env.GITHUB_REF_NAME : undefined;
if (tag && tag !== `v${identity.version}`) throw new Error('Tag does not match the package version');
const source = path.join(process.env.CARGO_TARGET_DIR || path.join(app, 'src-tauri/target'), spec.target, 'release/bundle');
const destination = path.join(root, 'target/release-assets', spec.target);
await mkdir(destination, { recursive: true });
if ((await readdir(destination)).length) throw new Error('Release output must be empty; use a new build directory');
const files = [];
for (const kind of spec.bundles) {
  const { extension, count } = bundleFormats[kind];
  const directory = path.join(source, kind);
  const entries = (await readdir(directory, { withFileTypes: true })).filter(e => e.isFile() && e.name.endsWith(extension));
  if (entries.length !== count) throw new Error(`Expected ${count} ${kind} packages; found ${entries.length}`);
  for (const entry of entries) {
    const name = `${spec.target}_${identity.buildId}_${entry.name.replaceAll(' ', '')}`;
    const bytes = await readFile(path.join(directory, entry.name));
    files.push({ name, sha256: createHash('sha256').update(bytes).digest('hex') });
    await copyFile(path.join(directory, entry.name), path.join(destination, name));
  }
}
await writeFile(path.join(destination, `${spec.target}_SHA256SUMS.txt`), files.map(f => `${f.sha256}  ${f.name}\n`).join(''));
await writeFile(path.join(destination, `${spec.target}_build.json`), JSON.stringify({
  version: identity.version, buildId: identity.buildId, commit: identity.commit,
  target: spec.target, node: identity.node, npm: identity.npm, pnpm: identity.pnpm,
  automatedChecks: 'passed', installedPackageSmoke: 'passed-on-ci-runner', machineAcceptance: 'not-performed-by-ci',
  signing: spec.platform === 'darwin' ? 'ad-hoc; not notarized' : 'unsigned', files,
}, null, 2) + '\n');
