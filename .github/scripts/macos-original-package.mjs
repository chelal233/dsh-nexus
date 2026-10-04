import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { createReadStream, createWriteStream } from 'node:fs';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { pipeline } from 'node:stream/promises';
import { GIB, verifyNodeIdentity } from './macos-acceptance-contract.mjs';
import { ownedDirectory, storageGate, watchStorage, treeFootprint } from './macos-acceptance-storage.mjs';
import { verifyInventory } from '../../apps/nexus-launcher/desktop/scripts/prepare-release.mjs';
import { browserLease } from './macos-acceptance-browser.mjs';

assert.equal(process.platform, 'darwin'); assert.ok(process.getuid() > 0);
const workspace = await fs.realpath(process.env.GITHUB_WORKSPACE);
const base = path.join(workspace, '.codex-temp/macos-acceptance');
const evidence = path.join(workspace, '.codex-artifacts/macos-acceptance');
await ownedDirectory(workspace, evidence);
assert.match(process.env.GITHUB_RUN_ID, /^\d+$/);
for (const part of ['.codex-temp', '.codex-temp/macos-acceptance']) {
  const directory = path.join(workspace, part);
  try { assert.equal((await fs.lstat(directory)).isSymbolicLink(), false); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  await fs.mkdir(directory, { recursive: true }); await ownedDirectory(workspace, directory);
}
const root = path.join(base, 'run-' + process.env.GITHUB_RUN_ID);
await fs.mkdir(root); await ownedDirectory(base, root);
await fs.writeFile(path.join(root, 'owner.json'), JSON.stringify({ run: process.env.GITHUB_RUN_ID, arch: process.arch }));
const preflight = JSON.parse(await fs.readFile(path.join(evidence, 'preflight.json'), 'utf8'));
assert.equal(preflight.result, 'PASS PREFLIGHT ONLY'); assert.equal(preflight.machine.arch, process.arch);
assert.equal(Number(process.env.QA_TEMP_BUDGET_GIB), 32, 'Full Mac acceptance requires its explicit reviewed capacity envelope');
const report = { schema: 1, result: 'FAIL', arch: process.arch, candidate: preflight.candidate,
  baseline: preflight.baseline, originalBytes: [], installed: [], phases: [],
  publicationChanged: false, gatekeeperFirstOpen: 'NOT RUN', realDeviceAcceptance: false };
const gate = storageGate(root, evidence), mounts = new Set();
const activeChildren = new Set(); let storageError, stopWatch;
const own = child => { activeChildren.add(child); child.once('close', () => activeChildren.delete(child)); return child; };
const checkStorage = () => { if (storageError) throw storageError; };
let processesClosed = true;
const command = (program, args, options = {}) => execFileSync(program, args, {
  encoding: 'utf8', timeout: 120000, maxBuffer: 1024 * 1024,
  env: { ...process.env, TMPDIR: path.join(root, 'tmp'), TEMP: path.join(root, 'tmp'), TMP: path.join(root, 'tmp') }, ...options,
}).trim();
async function longCommand(program, args) {
  checkStorage();
  const child = own(spawn(program, args, { env: { ...process.env, TMPDIR: path.join(root, 'tmp') }, stdio: ['ignore', 'pipe', 'pipe'] }));
  let tail = '';
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => { tail = (tail + chunk).slice(-8192); });
  const timer = setTimeout(() => child.kill('SIGTERM'), 120000);
  try {
    await new Promise((resolve, reject) => {
      child.once('error', reject); child.once('close', code => code === 0 ? resolve() : reject(new Error('Owned command failed: ' + tail)));
    });
    checkStorage();
  } finally { clearTimeout(timer); }
}
async function digest(file) {
  const hash = createHash('sha256'); let bytes = 0;
  for await (const chunk of createReadStream(file)) { hash.update(chunk); bytes += chunk.length; }
  return { bytes, sha256: hash.digest('hex') };
}
async function download(asset) {
  checkStorage();
  await gate('download:' + asset.name, asset.bytes + GIB / 8, true);
  const target = path.join(root, 'downloads', asset.name);
  const child = own(spawn('gh', ['api', `repos/chelal233/dsh-nexus/releases/assets/${asset.id}`, '-H', 'Accept: application/octet-stream'], {
    stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, TMPDIR: path.join(root, 'tmp') },
  }));
  let errorTail = '', received = 0;
  child.stderr.on('data', chunk => { errorTail = (errorTail + chunk).slice(-4096); });
  const exited = new Promise((resolve, reject) => {
    child.once('error', reject); child.once('close', code => code === 0 ? resolve() : reject(new Error('Original asset transfer failed: ' + errorTail)));
  });
  child.stdout.on('data', chunk => { received += chunk.length; if (received > asset.bytes) child.kill('SIGTERM'); });
  try { await Promise.all([pipeline(child.stdout, createWriteStream(target, { flags: 'wx' })), exited]); }
  catch (error) { child.kill('SIGTERM'); await exited.catch(() => {}); throw error; }
  checkStorage();
  const actual = await digest(target); assert.deepEqual(actual, { bytes: asset.bytes, sha256: asset.sha256 });
  report.originalBytes.push({ ...asset, status: 'PASS actual downloaded bytes' });
  return target;
}
async function identity(app, release, build) {
  const resources = path.join(app, 'Contents/Resources');
  await ownedDirectory(root, app); await ownedDirectory(app, resources);
  const raw = await fs.readFile(path.join(resources, 'release-manifest.json'));
  const manifest = JSON.parse(raw), target = `${process.arch === 'x64' ? 'x86_64' : 'aarch64'}-apple-darwin`;
  assert.equal(manifest.version, release.tag.slice(1)); assert.equal(manifest.commit, release.commit);
  for (const key of ['version', 'commit', 'buildId']) assert.equal(manifest[key], build[key]);
  assert.equal(manifest.runtime.target, target); assert.equal(manifest.dirty, false);
  await verifyInventory(resources, manifest.files);
  const executable = path.join(app, 'Contents/MacOS/Nexus Launcher');
  for (const file of [executable, path.join(resources, 'nexus-agent'), path.join(resources, 'nexus-desktop-bridge'),
    path.join(resources, 'runtime/node/bin/node')]) {
    const archs = command('/usr/bin/lipo', ['-archs', file]).split(/\s+/);
    assert.ok(archs.includes(process.arch === 'x64' ? 'x86_64' : 'arm64'), 'Original Mach-O architecture differs');
  }
  command('/usr/bin/codesign', ['--verify', '--deep', '--strict', app]);
  const update = await fs.readFile(path.join(resources, 'app-update.yml'), 'utf8');
  assert.match(update, new RegExp(`^channel: latest-${process.arch}\\r?$`, 'm'));
  const node = path.join(resources, 'runtime/node/bin/node');
  verifyNodeIdentity(JSON.parse(command(node, ['-p', 'JSON.stringify({platform:process.platform,arch:process.arch,version:process.version})'])),
    manifest.runtime.node.version, process.arch);
  const result = { app, build, manifestSha256: createHash('sha256').update(raw).digest('hex'),
    expandedApp: await treeFootprint(root, app),
    resourceFilesVerified: manifest.files.length, node, architecture: target, appSignatureIntegrity: 'PASS',
    productionSigning: false, updateChannel: `latest-${process.arch}`, updateYaml: `latest-${process.arch}-mac.yml` };
  report.installed.push(result); return result;
}
async function receiveMetadata(release) {
  const asset = release.assets.find(item => item.name.endsWith('_build.json'));
  const bytes = await download(asset), build = JSON.parse(await fs.readFile(bytes, 'utf8'));
  assert.equal(build.commit, release.commit); assert.equal(build.version, release.tag.slice(1));
  const listAsset = release.assets.find(item => item.name.endsWith('_SHA256SUMS.txt'));
  const list = await fs.readFile(await download(listAsset), 'utf8');
  for (const item of release.assets.filter(item => item.name.endsWith('.dmg') || item.name.endsWith('.zip'))) {
    assert.ok(list.split(/\r?\n/).some(line => line === `${item.sha256}  ${item.name}`));
  }
  return build;
}
async function installDmg(release, destination, build) {
  const file = await download(release.assets.find(item => item.name.endsWith('.dmg')));
  const mount = path.join(root, 'mount'); await fs.mkdir(mount, { recursive: true });
  const xml = command('/usr/bin/hdiutil', ['imageinfo', '-plist', file]);
  await fs.writeFile(path.join(evidence, release.tag + '-imageinfo.plist'), xml);
  // Image size is a conservative pre-copy admission; never overwrite a user app.
  const info = JSON.parse(command('/usr/bin/plutil', ['-convert', 'json', '-o', '-', '-'], { input: xml }));
  const size = Number(info['Size Information']?.['Total Bytes']);
  assert.ok(Number.isSafeInteger(size) && size > 0);
  await gate('mount-and-copy:' + release.tag, size + GIB / 4, true);
  mounts.add(mount);
  await longCommand('/usr/bin/hdiutil', ['attach', file, '-readonly', '-nobrowse', '-mountpoint', mount]);
  try {
    const source = path.join(mount, 'Nexus Launcher.app');
    assert.equal((await fs.lstat(source)).isSymbolicLink(), false);
    await longCommand('/usr/bin/ditto', [source, destination]);
  } finally { command('/usr/bin/hdiutil', ['detach', mount]); mounts.delete(mount); }
  await fs.unlink(file); return identity(destination, release, build);
}
async function runPhase(phase, app, proof, phaseRoot, format) {
  checkStorage();
  const phaseEvidence = path.join(evidence, format + '-' + phase); await fs.mkdir(phaseEvidence);
  await gate('phase:' + phase, GIB / 4, true);
  const proofFile = path.join(phaseEvidence, 'package-proof.json'); await fs.writeFile(proofFile, JSON.stringify(proof));
  const child = own(spawn(proof.node, [path.join(workspace, '.github/scripts/macos-native-business.mjs')], {
    env: { ...process.env, QA_ROOT: phaseRoot, QA_INSTALLED_APP: app, QA_PHASE: phase,
      QA_PHASE_EVIDENCE: phaseEvidence, QA_PACKAGE_PROOF: proofFile, QA_PACKAGE_FORMAT: format, QA_BUDGET_ROOT: root,
      QA_BROWSER_BRIDGE: path.join(root, 'launchservices-browser'),
      TMPDIR: path.join(root, 'tmp'), TEMP: path.join(root, 'tmp'), TMP: path.join(root, 'tmp') },
    stdio: ['ignore', 'pipe', 'pipe'],
  }));
  const log = createWriteStream(path.join(phaseEvidence, 'worker.log'), { flags: 'wx' });
  let logBytes = 0;
  for (const stream of [child.stdout, child.stderr]) stream.on('data', chunk => {
    logBytes += chunk.length; if (logBytes <= 2 * 1024 ** 2) log.write(chunk);
  });
  const close = new Promise((resolve, reject) => { child.once('error', reject); child.once('close', resolve); });
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => {
    child.kill('SIGTERM'); reject(new Error('Mac business phase deadline')); }, (phase === 'final' ? 75 : 40) * 60 * 1000); });
  try {
    assert.equal(await Promise.race([close, timeout]), 0, 'Actual native business phase failed; inspect worker receipt');
    checkStorage();
  } finally {
    clearTimeout(timer);
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGTERM');
      let cleanupTimer;
      await Promise.race([close.catch(() => {}), new Promise(resolve => { cleanupTimer = setTimeout(resolve, 90000); })]);
      clearTimeout(cleanupTimer);
    }
    log.end();
  }
  const result = JSON.parse(await fs.readFile(path.join(phaseEvidence, 'native-result.json'), 'utf8'));
  assert.ok(result.result.startsWith('PASS')); assert.equal(result.cleanupError, undefined);
  assert.ok(result.children.every(item => item.closed)); report.phases.push({ format, phase, result: result.result });
  return result;
}
async function prepareData(baseRoot) {
  for (const name of ['tmp', 'downloads', 'business', 'home', 'dsh', 'electron', 'browser']) {
    await fs.mkdir(path.join(baseRoot, name), { recursive: true });
    await ownedDirectory(root, path.join(baseRoot, name));
  }
}
async function installZip(release, destination, build) {
  const zip = await download(release.assets.find(item => item.name.endsWith('.zip')));
  const footprint = report.installed.find(item => item.build.commit === release.commit)?.expandedApp;
  assert.ok(footprint, 'ZIP extraction needs the independently measured original DMG app');
  await gate('portable-extraction', footprint.bytes + footprint.entries * 8192 + GIB / 4, true);
  await longCommand('/usr/bin/ditto', ['-x', '-k', zip, destination]); await fs.unlink(zip);
  return identity(path.join(destination, 'Nexus Launcher.app'), release, build);
}
function ownedProcesses(directory) {
  return command('/bin/ps', ['-axo', 'pid=,ppid=,command=']).split('\n').filter(line => line.includes(directory)).map(line => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(.+)$/);
    return match && { pid: Number(match[1]), ppid: Number(match[2]), command: match[3] };
  }).filter(item => item && item.pid !== process.pid);
}
try {
  await prepareData(root);
  stopWatch = watchStorage(gate, error => {
    storageError = error;
    for (const child of activeChildren) child.kill('SIGTERM');
  });
  await gate('before-downloads', preflight.capacity.downloadedBytes + 2 * GIB, true);
  const browserBridge = path.join(root, 'launchservices-browser');
  await gate('compile-native-browser-bridge', GIB / 8, true);
  command('/usr/bin/clang', ['-fobjc-arc', '-fno-modules', '-framework', 'Foundation', '-framework', 'CoreServices',
    path.join(workspace, '.github/scripts/macos-acceptance-browser.m'), '-o', browserBridge]);
  const browser = browserLease(args => command(browserBridge, args));
  report.browserPreflight = { result: 'PENDING existing native browser observation', original: browser.before, preferenceWrites: 0 };
  // Stop the denied setting operation; only the already configured native browser can satisfy this contract.
  try { browser.requireExistingChrome(); } catch (error) {
    report.browserPreflight.result = 'BLOCKED normal browser configuration required';
    throw error;
  }
  report.browserPreflight.result = 'PASS actual existing browser readback; no preference changed';
  const build = await receiveMetadata(preflight.candidate), oldBuild = await receiveMetadata(preflight.baseline);
  for (const format of ['dmg', 'zip']) {
    const familyRoot = path.join(root, format); await fs.mkdir(familyRoot); await prepareData(familyRoot);
    const app = path.join(familyRoot, 'Nexus Launcher.app');
    const install = (release, packageBuild) => format === 'dmg'
      ? installDmg(release, app, packageBuild) : installZip(release, familyRoot, packageBuild);
    const proof = await install(preflight.candidate, build);
    await runPhase('fresh', app, proof, familyRoot, format);
    const saved = path.join(familyRoot, 'candidate.app'); await fs.rename(app, saved);
    const oldProof = await install(preflight.baseline, oldBuild);
    await runPhase('old', app, oldProof, familyRoot, format);
    await ownedDirectory(root, app); await fs.rm(app, { recursive: true });
    await fs.rename(saved, app); await identity(app, preflight.candidate, build);
    await runPhase('final', app, proof, familyRoot, format);
    const leftovers = ownedProcesses(familyRoot);
    assert.equal(leftovers.length, 0, 'Format transaction still owns live processes');
    await ownedDirectory(root, familyRoot); await fs.rm(familyRoot, { recursive: true });
  }
  assert.deepEqual(report.phases.map(item => item.format + ':' + item.phase), ['dmg:fresh', 'dmg:old', 'dmg:final', 'zip:fresh', 'zip:old', 'zip:final']);
  report.result = 'PASS FULL MAC CLOUD ACCEPTANCE';
} catch (error) {
  report.error = { code: error.code, message: error.message }; process.exitCode = 1;
} finally {
  await stopWatch?.();
  for (const mount of mounts) try { command('/usr/bin/hdiutil', ['detach', mount]); mounts.delete(mount); }
  catch (error) { report.cleanupError = error.message; }
  const leftovers = ownedProcesses(root);
  report.ownedProcessLeftovers = leftovers;
  report.unclosedOwnedCommandPids = [...activeChildren].map(child => child.pid);
  processesClosed = leftovers.length === 0 && activeChildren.size === 0;
  report.cleanup = { ownedChildrenAbsent: processesClosed, imagesDetached: mounts.size === 0, removedTaskTemporaryRoot: false };
  if (!processesClosed || mounts.size || report.cleanupError) { report.result = 'FAIL'; process.exitCode = 1; }
  try { checkStorage(); report.finalStorage = await gate('finish', 0, true); }
  catch (error) { report.storageError = error.message; report.result = 'FAIL'; process.exitCode = 1; }
  if (processesClosed && mounts.size === 0) {
    await ownedDirectory(base, root);
    const owner = JSON.parse(await fs.readFile(path.join(root, 'owner.json')));
    assert.deepEqual(owner, { run: process.env.GITHUB_RUN_ID, arch: process.arch });
    await fs.rm(root, { recursive: true });
    report.cleanup.removedTaskTemporaryRoot = true;
  }
  await fs.writeFile(path.join(evidence, 'macos-original-package-result.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ arch: process.arch, result: report.result, error: report.error, cleanup: report.cleanup }));
}
