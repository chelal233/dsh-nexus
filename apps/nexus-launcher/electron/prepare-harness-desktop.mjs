// Run the unmodified official preparation routines only when inputs changed.
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { verifyDesktopKit, prepareDesktopElectron, digest, desktopKitMatchesSource } from './desktop-runtime.mjs';
import { preparePrimaryPayload, preparePortableHost, runtimeInventory } from './desktop-runtime-cache.mjs';
import { desktopSourceView } from './desktop-paths.mjs';

globalThis.fetch = async () => { throw Error('desktop_runtime_missing: network is disabled during Desktop preparation'); };
process.env.npm_config_offline = 'true';
process.env.PYTHONDONTWRITEBYTECODE = '1';
const stage = value => console.log(`NEXUS_DESKTOP_STAGE:${value}`);
const root = fs.realpathSync.native(process.argv[2]), app = path.join(root, 'apps/desktop');
const load = file => import(pathToFileURL(path.join(app, file)).href);
const require = createRequire(path.join(app, 'package.json'));
const metadata = JSON.parse(fs.readFileSync(path.join(app, 'package.json'), 'utf8'));
const kitRoot = fs.realpathSync.native(process.argv[3]);
const cache = process.argv[4] || path.join(desktopSourceView(root), 'apps/desktop/.desktop-build/nexus-runtime');
stage('verify');
const kit = verifyDesktopKit(kitRoot);
if (process.argv[6] === 'portable-host') await preparePortableHost(kit, cache);
const pnpm = JSON.parse(fs.readFileSync(path.join(app, 'node_modules/pnpm/package.json'), 'utf8'));
if (!desktopKitMatchesSource(root, kitRoot) ||
    kit.electronVersion !== require('electron/package.json').version || kit.pnpmVersion !== pnpm.version) throw Error('desktop_runtime_incompatible');
const { resolveDesktopTargetBuildPaths } = await load('scripts/desktop-build-paths.mjs');
const paths = resolveDesktopTargetBuildPaths();
fs.mkdirSync(paths.runtime, { recursive: true });
stage('runtime');
if (kit.schema === 3) {
  const payload = await preparePrimaryPayload(kit, cache);
  const primary = JSON.parse(fs.readFileSync(path.join(payload, 'primary-runtime/runtime.json')));
  const lock = JSON.parse(fs.readFileSync(path.join(kitRoot, 'lock.json')));
  const versions = primary.components ?? primary;
  if (primary.platform !== process.platform || primary.arch !== process.arch ||
      versions.node !== lock.nodeVersion || versions.python !== lock.pythonVersion || versions.pnpm !== pnpm.version) throw Error('desktop_runtime_incompatible');
  for (const name of ['primary-runtime', 'office-skills']) {
    const target = path.join(payload, name), destination = path.join(paths.runtime, name);
    let correct = false;
    try { correct = fs.lstatSync(destination).isSymbolicLink() && fs.realpathSync.native(destination) === fs.realpathSync.native(target); } catch {}
    if (!correct) {
      // Only replace the upstream build-owned generated path, never user data.
      fs.rmSync(destination, { recursive: true, force: true });
      fs.symlinkSync(target, destination, process.platform === 'win32' ? 'junction' : 'dir');
    }
  }
} else {
  // Old offline exports remain supported; cancellation leaves no valid stamp.
  prepareDesktopElectron(kitRoot, cache);
  const primary = path.join(desktopSourceView(root), path.relative(root, paths.runtime), 'primary-runtime');
  const stamp = path.join(paths.runtime, 'nexus-legacy-primary.json');
  const identity = digest(path.join(kitRoot, 'manifest.json')) + digest(path.join(app, 'scripts/prepare-primary-runtime.ts'));
  let valid = false;
  try { const saved = JSON.parse(fs.readFileSync(stamp)); valid = saved.identity === identity && JSON.stringify(saved.files) === JSON.stringify(await runtimeInventory(primary)); } catch {}
  if (!valid) {
    fs.mkdirSync(paths.downloads, { recursive: true });
    for (const entry of kit.files.filter(file => file.path.startsWith('assets/'))) fs.copyFileSync(path.join(kitRoot, entry.path), path.join(paths.downloads, path.basename(entry.path)));
    const { preparePrimaryRuntime, smokePrimaryRuntime } = await load('scripts/prepare-primary-runtime.ts');
    await preparePrimaryRuntime({ deferSmoke: true });
    stage('check'); smokePrimaryRuntime(primary);
    fs.writeFileSync(stamp, JSON.stringify({ identity, files: await runtimeInventory(primary) }));
  }
}
stage('project');
const { prepareDevelopmentProject } = await load('scripts/development-project.ts');
const { DESKTOP_HOST_PROTOCOL_VERSION } = await load('src/host-protocol.ts');
const projectDir = path.join(app, '.desktop-build/development/project');
const dependencyDir = path.join(root, 'node_modules/.pnpm/node_modules');
const hash = createHash('sha256');
hash.update(root); hash.update(digest(path.join(kitRoot, 'manifest.json')));
for (const file of ['package.json', 'scripts/development-project.ts', 'src/project-manager.ts', 'src/runtime-tree.ts', 'src/host-protocol.ts']) hash.update(fs.readFileSync(path.join(app, file)));
for (const name of fs.readdirSync(dependencyDir).filter(name => name !== '.bin').sort()) {
  const names = name.startsWith('@') ? fs.readdirSync(path.join(dependencyDir, name)).sort().map(child => `${name}/${child}`) : [name];
  for (const entry of names) {
    const target = fs.realpathSync.native(path.join(dependencyDir, entry));
    if (!fs.statSync(target).isDirectory()) continue;
    hash.update(JSON.stringify([entry, target]));
    hash.update(fs.readFileSync(path.join(target, 'package.json')));
  }
}
for (const entry of ['apps/cli/package.json', 'apps/desktop-host/package.json']) hash.update(fs.readFileSync(path.join(root, entry)));
const nodeVersion = process.argv[5] || kit.electronNodeVersion || execFileSync(prepareDesktopElectron(kitRoot, cache).electron,
  ['-p', 'process.versions.node'], { encoding: 'utf8', windowsHide: true, env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' } }).trim();
hash.update(nodeVersion);
const fingerprint = hash.digest('hex'), projectStamp = path.join(paths.runtime, 'nexus-project.json');
// Check the small project view without traversing any linked package trees.
function projectIdentity() {
  const result = [];
  function visit(relative) {
    const file = path.join(projectDir, relative), stat = fs.lstatSync(file);
    if (stat.isSymbolicLink()) result.push([relative, fs.realpathSync.native(file)]);
    else if (stat.isDirectory()) for (const name of fs.readdirSync(file).sort()) visit(path.join(relative, name));
    else result.push([relative, digest(file)]);
  }
  visit(''); return result;
}
let cached = false;
try { const saved = JSON.parse(fs.readFileSync(projectStamp)); cached = saved.fingerprint === fingerprint && JSON.stringify(saved.files) === JSON.stringify(projectIdentity()); } catch {}
if (!cached) {
  prepareDevelopmentProject({ projectDir, cliDir: path.join(root, 'apps/cli'), hostDir: path.join(root, 'apps/desktop-host'), dependencyDir,
    release: { schemaVersion: 1, version: metadata.version, hostProtocolVersion: DESKTOP_HOST_PROTOCOL_VERSION, nodeVersion, pnpmVersion: pnpm.version } });
  fs.writeFileSync(projectStamp, JSON.stringify({ fingerprint, files: projectIdentity() }));
}
// Successful shared preparation supersedes exactly the old Nexus-owned cache
// for this pinned Electron. Keep other versions and portable fallback hosts.
if (kit.schema === 3 && process.argv[6] !== 'portable-host' && /^[a-f0-9]{64}$/.test(kit.retiredElectronArchiveSha256 ?? '')) {
  const retired = path.join(cache, `electron-${kit.retiredElectronArchiveSha256}`), stamp = `${retired}.json`;
  try {
    if (fs.existsSync(retired) && fs.existsSync(stamp) && !fs.lstatSync(retired).isSymbolicLink() &&
        fs.lstatSync(retired).isDirectory() && fs.lstatSync(stamp).isFile() && fs.lstatSync(stamp).size < 1024 * 1024 &&
        Array.isArray(JSON.parse(fs.readFileSync(stamp)))) {
      stage('cleanup'); fs.rmSync(retired, { recursive: true }); fs.rmSync(stamp);
    }
  } catch { console.warn('Unused Electron cache retained; shared runtime is ready.'); }
}
console.log(`Official Harness Desktop ${metadata.version} is prepared (${cached ? 'reused project' : 'prepared project'}).`);
