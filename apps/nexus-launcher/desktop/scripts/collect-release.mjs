import { createHash } from 'node:crypto';
import { copyFile, mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { bundleFormats, releaseBasename, selectPlatform } from './release-platform.mjs';

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const root = path.resolve(app, '../..');
const spec = selectPlatform(process.env.CARGO_BUILD_TARGET);
const identity = JSON.parse(await readFile(path.join(app, 'desktop/resources/release-identity.json'), 'utf8'));
const basename = releaseBasename(spec.target, identity.version);
if (identity.dirty) throw new Error('Release artifacts must come from a clean committed checkout');
const tag = process.env.GITHUB_REF_TYPE === 'tag' ? process.env.GITHUB_REF_NAME : undefined;
if (tag && tag !== `v${identity.version}`) throw new Error('Tag does not match the package version');
const source = path.join(app, 'electron-dist');
const destination = path.join(root, 'target/release-assets', spec.target);
await mkdir(destination, { recursive: true });
if ((await readdir(destination)).length) throw new Error('Release output must be empty; use a new build directory');
const files = [];
for (const kind of spec.bundles) {
  const { extension, count } = bundleFormats[kind];
  const directory = source;
  const entries = (await readdir(directory, { withFileTypes: true })).filter(e => e.isFile() && e.name.endsWith(extension));
  if (entries.length !== count) throw new Error(`Expected ${count} ${kind} packages; found ${entries.length}`);
  for (const entry of entries) {
    const name = `${basename}${extension}`;
    const bytes = await readFile(path.join(directory, entry.name));
    files.push({ name, sha256: createHash('sha256').update(bytes).digest('hex') });
    await copyFile(path.join(directory, entry.name), path.join(destination, name));
  }
}
const channelName = `latest-${spec.arch}${spec.platform === 'darwin' ? '-mac' : ''}.yml`;
const channelBytes = await readFile(path.join(source, channelName));
files.push({ name: channelName, sha256: createHash('sha256').update(channelBytes).digest('hex') });
await copyFile(path.join(source, channelName), path.join(destination, channelName));
if (process.env.NEXUS_PACKAGE_SMOKE_PASSED !== '1') throw new Error('Installed package smoke must pass before collection');
await writeFile(path.join(destination, `${basename}_SHA256SUMS.txt`), files.map(f => `${f.sha256}  ${f.name}\n`).join(''));
await writeFile(path.join(destination, `${basename}_build.json`), JSON.stringify({
  version: identity.version, buildId: identity.buildId, commit: identity.commit,
  target: spec.target, node: identity.node, npm: identity.npm, pnpm: identity.pnpm,
  automatedChecks: 'passed', installedPackageSmoke: 'passed-on-ci-runner', machineAcceptance: 'not-performed-by-ci',
  signing: spec.platform === 'darwin' ? 'ad-hoc; not notarized' : 'unsigned', files,
}, null, 2) + '\n');
