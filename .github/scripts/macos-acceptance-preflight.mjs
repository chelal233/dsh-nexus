import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { execFileSync } from 'node:child_process';
import { selectOriginalAssets, capacityVerdict } from './macos-acceptance-contract.mjs';

const workspace = await fs.realpath(process.env.GITHUB_WORKSPACE || process.cwd());
const out = path.resolve(process.env.QA_EVIDENCE || path.join(workspace, '.codex-artifacts/macos-acceptance'));
assert.equal(out, path.join(workspace, '.codex-artifacts/macos-acceptance'));
for (const part of ['.codex-artifacts', '.codex-artifacts/macos-acceptance']) {
  const target = path.join(workspace, part);
  try { assert.equal((await fs.lstat(target)).isSymbolicLink(), false); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  await fs.mkdir(target, { recursive: true });
  assert.equal(await fs.realpath(target), target);
}
const report = { schema: 1, result: 'FAIL', createdAt: new Date().toISOString(),
  scope: 'Read-only original-asset, GUI and storage preflight; no package downloaded or installed',
  fullAcceptance: 'NOT RUN', checks: [] };
const command = (program, args) => execFileSync(program, args, { encoding: 'utf8',
  timeout: 30000, maxBuffer: 1024 * 1024, stdio: ['ignore', 'pipe', 'pipe'] }).trim();
const probe = (program, args) => {
  try { return { ok: true, output: command(program, args).slice(0, 4096) }; }
  catch (error) { return { ok: false, exitCode: error.status, message: String(error.message).slice(0, 1000) }; }
};
try {
  assert.equal(process.platform, 'darwin');
  assert.equal(process.arch, process.env.QA_ARCH);
  assert.ok(process.getuid() > 0, 'QA requires the ordinary non-root runner user');
  report.machine = { platform: process.platform, arch: process.arch, uid: process.getuid(),
    uname: command('/usr/bin/uname', ['-m']), version: command('/usr/bin/sw_vers', ['-productVersion']),
    node: process.version, virtualized: true, realDeviceAcceptance: false };
  report.gui = { launchServices: probe('/bin/launchctl', ['print', `gui/${process.getuid()}`]),
    consoleOwnerUid: probe('/usr/bin/stat', ['-f', '%u', '/dev/console']),
    chrome: probe('/usr/bin/mdls', ['-name', 'kMDItemVersion', '/Applications/Google Chrome.app']),
    uiAutomation: 'NOT RUN', gatekeeperFirstOpen: 'NOT RUN', tcc: 'NOT RUN' };
  const repository = process.env.GITHUB_REPOSITORY;
  assert.equal(repository, 'chelal233/dsh-nexus');
  const api = resource => JSON.parse(command('gh', ['api', `repos/${repository}/${resource}`]));
  const receive = tag => {
    assert.match(tag, /^v\d+\.\d+\.\d+$/);
    const release = api(`releases/tags/${tag}`);
    assert.equal(release.tag_name, tag);
    const ref = api(`git/ref/tags/${tag}`);
    const annotation = ref.object.type === 'tag' ? api(`git/tags/${ref.object.sha}`) : undefined;
    const commit = annotation ? annotation.object.sha : ref.object.sha;
    assert.match(commit, /^[a-f0-9]{40}$/);
    return { tag, releaseId: release.id, tagObject: ref.object.sha, commit,
      assets: selectOriginalAssets(release, process.arch) };
  };
  report.candidate = receive(process.env.QA_CANDIDATE_TAG);
  report.baseline = receive(process.env.QA_BASELINE_TAG);
  assert.notEqual(report.candidate.tag, report.baseline.tag);
  if (report.candidate.tag === 'v1.0.4') assert.equal(report.candidate.commit,
    'a02ef46494ac5000060e2de8dbdf64d73cc5d932');
  const metadata = await fs.statfs(workspace);
  const downloadedBytes = report.candidate.assets.reduce((n, item) => n + item.bytes, 0)
    + report.baseline.assets.find(item => item.name.endsWith('.dmg')).bytes;
  report.capacity = capacityVerdict({ free: metadata.bavail * metadata.bsize,
    total: metadata.blocks * metadata.bsize, downloadedBytes });
  report.memoryBytes = os.totalmem();
  report.checks.push('Native architecture and non-root user match',
    'Exact published candidate and old-version release/tag metadata',
    'Unique nonzero original DMG/ZIP/build/checksum asset identities',
    'Actual target-volume lower-bound capacity measured');
  report.result = 'PASS PREFLIGHT ONLY';
} catch (error) {
  report.error = { message: error.message, code: error.code };
  process.exitCode = 1;
} finally {
  report.cleanup = { downloads: 0, applications: 0, childProcesses: 0, userDataChanged: false };
  const text = JSON.stringify(report, null, 2) + '\n';
  assert.ok(Buffer.byteLength(text) < 128 * 1024);
  await fs.writeFile(path.join(out, 'preflight.json'), text, { flag: 'wx' });
  console.log(JSON.stringify({ result: report.result, arch: process.arch, capacity: report.capacity,
    fullAcceptance: report.fullAcceptance }));
}
