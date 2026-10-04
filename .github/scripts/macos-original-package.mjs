import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { createReadStream, createWriteStream } from 'node:fs';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { pipeline } from 'node:stream/promises';
import { GIB } from './macos-acceptance-contract.mjs';
import { ownedDirectory, allocatedBytes, storageGate } from './macos-acceptance-storage.mjs';
import { verifyInventory } from '../../apps/nexus-launcher/desktop/scripts/prepare-release.mjs';

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
let processesClosed = true;
const command = (program, args, options = {}) => execFileSync(program, args, {
  encoding: 'utf8', timeout: 120000, maxBuffer: 1024 * 1024,
  env: { ...process.env, TMPDIR: path.join(root, 'tmp'), TEMP: path.join(root, 'tmp'), TMP: path.join(root, 'tmp') }, ...options,
}).trim();
async function digest(file) {
  const hash = createHash('sha256'); let bytes = 0;
  for await (const chunk of createReadStream(file)) { hash.update(chunk); bytes += chunk.length; }
  return { bytes, sha256: hash.digest('hex') };
}
async function download(asset) {
  await gate('download:' + asset.name, asset.bytes + GIB / 8, true);
  const target = path.join(root, 'downloads', asset.name);
  const child = spawn('gh', ['api', `repos/chelal233/dsh-nexus/releases/assets/${asset.id}`, '-H', 'Accept: application/octet-stream'], {
    stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, TMPDIR: path.join(root, 'tmp') },
  });
  let errorTail = '', received = 0;
  child.stderr.on('data', chunk => { errorTail = (errorTail + chunk).slice(-4096); });
  const exited = new Promise((resolve, reject) => {
    child.once('error', reject); child.once('close', code => code === 0 ? resolve() : reject(new Error('Original asset transfer failed: ' + errorTail)));
  });
  child.stdout.on('data', chunk => { received += chunk.length; if (received > asset.bytes) child.kill('SIGTERM'); });
  try { await Promise.all([pipeline(child.stdout, createWriteStream(target, { flags: 'wx' })), exited]); }
  catch (error) { child.kill('SIGTERM'); await exited.catch(() => {}); throw error; }
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
  assert.deepEqual(JSON.parse(command(node, ['-p', 'JSON.stringify({platform:process.platform,arch:process.arch,version:process.version})'])),
    { platform: 'darwin', arch: process.arch, version: 'v' + manifest.runtime.node.version });
  const result = { app, build, manifestSha256: createHash('sha256').update(raw).digest('hex'),
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
  command('/usr/bin/hdiutil', ['attach', file, '-readonly', '-nobrowse', '-mountpoint', mount]); mounts.add(mount);
  try {
    const source = path.join(mount, 'Nexus Launcher.app');
    assert.equal((await fs.lstat(source)).isSymbolicLink(), false);
    command('/usr/bin/ditto', [source, destination]);
  } finally { command('/usr/bin/hdiutil', ['detach', mount]); mounts.delete(mount); }
  await fs.unlink(file); return identity(destination, release, build);
}
async function runPhase(phase, app, proof, phaseRoot = root) {
  const phaseEvidence = path.join(evidence, phase); await fs.mkdir(phaseEvidence);
  await gate('phase:' + phase, GIB / 4, true);
  const proofFile = path.join(phaseEvidence, 'package-proof.json'); await fs.writeFile(proofFile, JSON.stringify(proof));
  const child = spawn(proof.node, [path.join(workspace, '.github/scripts/macos-native-business.mjs')], {
    env: { ...process.env, QA_ROOT: phaseRoot, QA_INSTALLED_APP: app, QA_PHASE: phase,
      QA_PHASE_EVIDENCE: phaseEvidence, QA_PACKAGE_PROOF: proofFile,
      TMPDIR: path.join(root, 'tmp'), TEMP: path.join(root, 'tmp'), TMP: path.join(root, 'tmp') },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
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
  } finally { clearTimeout(timer); log.end(); }
  const result = JSON.parse(await fs.readFile(path.join(phaseEvidence, 'native-result.json'), 'utf8'));
  assert.ok(result.result.startsWith('PASS')); assert.equal(result.cleanupError, undefined);
  assert.ok(result.children.every(item => item.closed)); report.phases.push({ phase, result: result.result });
  return result;
}
async function prepareData(baseRoot) {
  for (const name of ['tmp', 'downloads', 'business', 'home', 'dsh', 'electron', 'browser']) {
    await fs.mkdir(path.join(baseRoot, name), { recursive: true });
    await ownedDirectory(root, path.join(baseRoot, name));
  }
}
try {
  await prepareData(root); await gate('before-downloads', preflight.capacity.downloadedBytes + 2 * GIB, true);
  const build = await receiveMetadata(preflight.candidate), oldBuild = await receiveMetadata(preflight.baseline);
  const app = path.join(root, 'Nexus Launcher.app');
  const proof = await installDmg(preflight.candidate, app, build);
  await runPhase('fresh', app, proof);
  const saved = path.join(root, 'candidate.app'); await fs.rename(app, saved);
  const oldProof = await installDmg(preflight.baseline, app, oldBuild);
  await runPhase('old', app, oldProof);
  await ownedDirectory(root, app); await fs.rm(app, { recursive: true });
  await fs.rename(saved, app);
  await identity(app, preflight.candidate, build);
  await runPhase('final', app, proof);
  // The portable package is independently received, checked and actually launched.
  const zip = await download(preflight.candidate.assets.find(item => item.name.endsWith('.zip')));
  const portableRoot = path.join(root, 'portable'); await fs.mkdir(portableRoot); await prepareData(portableRoot);
  await gate('portable-extraction', allocatedBytes(app) + GIB / 4, true);
  command('/usr/bin/ditto', ['-x', '-k', zip, portableRoot]); await fs.unlink(zip);
  const portableApp = path.join(portableRoot, 'Nexus Launcher.app');
  const portableProof = await identity(portableApp, preflight.candidate, build);
  await runPhase('portable', portableApp, portableProof, portableRoot);
  assert.deepEqual(report.phases.map(item => item.phase), ['fresh', 'old', 'final', 'portable']);
  report.result = 'PASS FULL MAC CLOUD ACCEPTANCE';
} catch (error) {
  report.error = { code: error.code, message: error.message }; process.exitCode = 1;
} finally {
  for (const mount of mounts) try { command('/usr/bin/hdiutil', ['detach', mount]); mounts.delete(mount); }
  catch (error) { report.cleanupError = error.message; }
  const output = command('/bin/ps', ['-axo', 'pid=,ppid=,command=']);
  const leftovers = output.split('\n').filter(line => line.includes(root)).map(line => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(.+)$/);
    return match && { pid: Number(match[1]), ppid: Number(match[2]), command: match[3] };
  }).filter(item => item && item.pid !== process.pid && /Nexus Launcher|nexus-agent|nexus-desktop-bridge|Google Chrome|macos-native-business/.test(item.command));
  report.ownedProcessLeftovers = leftovers; processesClosed = leftovers.length === 0;
  report.cleanup = { ownedChildrenAbsent: processesClosed, imagesDetached: mounts.size === 0, removedTaskTemporaryRoot: false };
  if (!processesClosed || mounts.size || report.cleanupError) { report.result = 'FAIL'; process.exitCode = 1; }
  try { report.finalStorage = await gate('finish', 0, true); } catch (error) { report.storageError = error.message; }
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
