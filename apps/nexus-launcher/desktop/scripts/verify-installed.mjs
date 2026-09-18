import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { verifyInventory, verifyIdentity, verifyRuntimeVersions, verifyAgentIdentity } from './prepare-release.mjs';

// Takes only an explicitly supplied package resource directory. All live state
// belongs to a fresh temporary data root; no real Harness is configured.
const resources = path.resolve(process.argv[2]);
const gui = process.argv[3];
if (gui && process.env.GITHUB_ACTIONS !== 'true') throw new Error('GUI package smoke runs only on disposable CI runners');
const suffix = process.platform === 'win32' ? '.exe' : '';
// Both installed and ZIP packages must carry the updater's runtime config.
// Agent startup alone does not exercise either automatic or manual checks.
const updateConfig = await readFile(path.join(resources, 'app-update.yml'), 'utf8');
for (const line of [
  'provider: github', 'owner: chelal233', 'repo: dsh-nexus',
  `channel: latest-${process.arch}`, 'updaterCacheDirName: nexus-launcher-updater',
]) {
  assert.ok(updateConfig.split(/\r?\n/).includes(line), `Invalid packaged update configuration: expected ${line}`);
}
const bytes = await readFile(path.join(resources, 'release-manifest.json'));
const manifest = JSON.parse(bytes);
verifyIdentity(manifest, JSON.parse(await readFile(path.join(resources, 'release-identity.json'), 'utf8')),
  createHash('sha256').update(bytes).digest('hex'));
await verifyInventory(resources, manifest.files);
verifyRuntimeVersions(resources, manifest.runtime);
const agent = path.join(resources, `nexus-agent${suffix}`);
verifyAgentIdentity(manifest, JSON.parse(execFileSync(agent, ['--build-identity'], { encoding: 'utf8', timeout: 15000 })));
const temporary = await mkdtemp(path.join(os.tmpdir(), 'nexus-installed-smoke-'));
let backend, frontend;
let logs = '';
function start(program, args, env) {
  const child = spawn(program, args, { cwd: resources, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  child.on('error', error => { logs += error.message; });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { logs = (logs + data).slice(-12000); });
  return child;
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise(resolve => child.once('close', resolve));
  child.kill();
  await exited;
}
try {
  const env = { ...process.env, NEXUS_DATA_DIR: temporary };
  backend = start(agent, ['--data-dir', temporary, '--port', '0'], env);
  let record;
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (backend.exitCode !== null) throw new Error(`Packaged Agent exited: ${logs}`);
    try { record = JSON.parse(await readFile(path.join(temporary, 'run/agent.json'), 'utf8')); } catch {}
    if (record) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.equal(record?.pid, backend.pid, `Packaged Agent did not publish its own discovery record: ${logs}`);
  const cli = path.join(resources, `nexusctl${suffix}`);
  const args = ['--port', String(record.port), 'status', '--json'];
  JSON.parse(execFileSync(cli, args, { env, encoding: 'utf8', timeout: 15000 }));
  if (gui) {
    frontend = start(path.resolve(gui), [], { ...env, NEXUS_AGENT_PORT: String(record.port) });
    await new Promise(resolve => setTimeout(resolve, 8000));
    assert.equal(frontend.exitCode, null, `Packaged GUI exited during startup: ${logs}`);
    assert.equal(frontend.signalCode, null, `Packaged GUI was terminated during startup: ${logs}`);
    assert.ok(frontend.pid, `Packaged GUI could not spawn: ${logs}`);
    JSON.parse(execFileSync(cli, args, { env, encoding: 'utf8', timeout: 15000 }));
  }
  console.log(`[installed-smoke] ${manifest.buildId}: resource hashes, runtimes, Agent/CLI authenticated status${gui ? ', GUI process startup' : ''} passed`);
} finally {
  await stop(frontend);
  await stop(backend);
  await rm(temporary, { recursive: true, force: true });
}
