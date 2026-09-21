import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { selectDesktopKit } from './desktop-runtime.mjs';

export const desktopActive = state => ['preparing', 'launched', 'stopping'].includes(state?.phase);
const alive = pid => {
  if (!Number.isSafeInteger(pid) || pid < 1) return false;
  try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; }
};

export function readDesktopState(file, isAlive = alive) {
  let state;
  try { state = JSON.parse(fs.readFileSync(file, 'utf8')); }
  catch (error) { if (error.code === 'ENOENT') return { phase: 'idle' }; throw error; }
  if (['launched', 'failed', 'stopped'].includes(state.phase) && /^[a-f0-9-]{36}$/.test(state.operationId ?? '')) {
    const evidence = path.join(path.dirname(file), `startup-${state.operationId}.json`);
    state.audit = { state: 'checking' };
    try {
      const meta = fs.lstatSync(evidence);
      if (meta.isFile() && meta.size <= 16384) {
        const audit = JSON.parse(fs.readFileSync(evidence, 'utf8'));
        if (audit.operationId === state.operationId && (audit.pid === state.childPid || (state.phase !== 'launched' && !state.childPid)) && ['checking','ready','failed','unverified'].includes(audit.state)) state.audit = audit;
      }
    } catch { /* Missing structured evidence must not hide the official diagnostic. */ }
    if (state.audit.state !== 'ready') {
      try {
        const diagnostic = `${evidence}.error`, errorMeta = fs.lstatSync(diagnostic);
        if (errorMeta.isFile() && errorMeta.size <= 65536) state.audit = {state:'failed', error:fs.readFileSync(diagnostic,'utf8').slice(0,6000).replace(/([?&]token=)[^\s&]+/gi,'$1[redacted]')};
      } catch { /* No official diagnostic is available yet. */ }
    }
    if (state.phase === 'launched' && state.audit.state === 'checking' && Date.now() - state.stageStartedAt > 95000) state.audit = { state:'unverified' };
  }
  if (desktopActive(state) && !isAlive(state.pid)) {
    if (isAlive(state.childPid)) return state; // Do not rebuild under an orphaned native process.
    return { ...state, phase: 'failed', error: 'desktop_interrupted' };
  }
  return state;
}

export function managedDesktopRoot(dataRoot, release) {
  if (!path.isAbsolute(dataRoot ?? '') || typeof release !== 'string' ||
      !/^[a-zA-Z0-9][a-zA-Z0-9._-]{0,199}$/.test(release) || release === '..') {
    throw new Error('desktop_no_release');
  }
  const parent = fs.realpathSync(path.join(dataRoot, 'releases'));
  const root = fs.realpathSync(path.join(parent, release));
  const relative = path.relative(parent, root);
  if (relative.startsWith('..') || path.isAbsolute(relative) || !relative) throw new Error('desktop_invalid_source');
  return root;
}

// Feature detection is separate from launch readiness: a missing runtime is repairable.
export function probeDesktopSupport(root, { platform = process.platform, arch = process.arch } = {}) {
  const file = path.join(root, 'apps/desktop/package.json');
  if (!fs.existsSync(file)) return { supported: false };
  const metadata = JSON.parse(fs.readFileSync(file, 'utf8'));
  const lockFile = path.join(root, 'apps/desktop/scripts/primary-runtime-lock.json');
  if (!fs.existsSync(lockFile)) return { supported: false };
  const lock = JSON.parse(fs.readFileSync(lockFile, 'utf8'));
  if (!lock.targets?.[`${({ win32: 'win', darwin: 'mac', linux: 'linux' })[platform]}-${arch}`]) return { supported: false };
  return { supported: metadata.name === '@deepseek-ai/dsh-desktop' && metadata.main === 'lib/main.js', version: metadata.version };
}

export function desktopCapability(root) {
  if (!probeDesktopSupport(root).supported) throw new Error('desktop_unsupported');
  const app = path.join(root, 'apps', 'desktop');
  const required = ['package.json', 'lib/main.js', 'scripts/development-project.ts',
    'scripts/prepare-primary-runtime.ts', 'src/host-protocol.ts', 'node_modules/electron/package.json'];
  if (required.some(file => !fs.existsSync(path.join(app, file))) ||
      !fs.existsSync(path.join(root, 'apps/desktop-host/lib/index.js')) ||
      !fs.existsSync(path.join(root, 'node_modules/tsx/dist/loader.mjs'))) throw new Error('desktop_install_incomplete');
  const metadata = JSON.parse(fs.readFileSync(path.join(app, 'package.json'), 'utf8'));
  if (metadata.name !== '@deepseek-ai/dsh-desktop' || metadata.main !== 'lib/main.js') throw new Error('desktop_unsupported');
  return { app, version: metadata.version };
}

// Preparation is independent of the launcher window. The worker owns the child
// until it exits and publishes process state, never an invented readiness signal.
export class HarnessDesktop {
  busy = false;
  restarting = false;
  stopGeneration = 0;
  constructor({ bridge, userData, resources, executable = process.execPath, electronExecutable = process.execPath, electronApp }) {
    Object.assign(this, { bridge, userData, resources, executable, electronExecutable, electronApp });
    this.directory = path.join(userData, 'harness-desktop');
    this.file = path.join(this.directory, 'state.json');
  }
  status() {
    const state = readDesktopState(this.file);
    return this.busy && !desktopActive(state) ? { ...state, phase: 'preparing', stage: 'context' } : state;
  }
  stop() {
    this.stopGeneration++;
    if (!this.stopPromise) this.stopPromise = this.stopOperation().finally(() => { this.stopPromise = undefined; });
    return this.stopPromise;
  }
  async restart() {
    if (this.restarting) throw new Error('desktop_already_active');
    this.restarting = true;
    try {
      const stopped = this.stop();
      const generation = this.stopGeneration;
      await stopped;
      // A later Stop cancels the pending restart, including while stop is shared.
      if (generation !== this.stopGeneration) return this.status();
      return await this.startOperation();
    } finally { this.restarting = false; }
  }
  async stopOperation() {
    this.cancelRequested = true;
    const deadline = Date.now() + 20000;
    while (this.busy && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 100));
    if (this.busy) throw new Error('desktop_stop_timeout');
    const state = this.status();
    if (!desktopActive(state)) return state;
    if (!/^[a-f0-9-]{36}$/.test(state.operationId ?? '')) throw new Error('desktop_stop_unsupported');
    const requestId = randomUUID();
    const requestFile = path.join(this.directory, `stop-${state.operationId}.json`);
    const temporary = `${requestFile}.${requestId}.tmp`;
    try {
      fs.writeFileSync(temporary, JSON.stringify({ requestId }), { mode: 0o600 });
      fs.renameSync(temporary, requestFile);
    } finally { fs.rmSync(temporary, { force: true }); }
    while (Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 100));
      const next = this.status();
      if (next.operationId !== state.operationId) throw new Error('desktop_stop_changed');
      if (next.stopRequestId === requestId && next.stopError) throw new Error(next.stopError);
      if (!desktopActive(next)) return next;
    }
    throw new Error('desktop_stop_timeout');
  }
  async capability() {
    const startup = await this.bridge.request('desktop_launch_context');
    if (!startup.available) throw new Error('Desktop support check unavailable');
    const [releases, config] = await Promise.all([
      this.bridge.request('proxy_request', { method: 'GET', path: '/v1/releases' }),
      this.bridge.request('proxy_request', { method: 'GET', path: '/v1/config' }),
    ]);
    if (config.external_harness || !releases.current_release) return { supported: false, release: releases.current_release };
    return { ...probeDesktopSupport(managedDesktopRoot(startup.data_root, releases.current_release)), release: releases.current_release };
  }
  async start() {
    if (this.restarting || this.stopPromise) throw new Error('desktop_already_active');
    return this.startOperation();
  }
  async startOperation() {
    if (this.busy || desktopActive(this.status())) throw new Error('desktop_already_active');
    this.busy = true;
    this.cancelRequested = false;
    try {
      const startup = await this.bridge.request('desktop_launch_context');
      const [releases, config, runtime] = await Promise.all([
        this.bridge.request('proxy_request', { method: 'GET', path: '/v1/releases' }),
        this.bridge.request('proxy_request', { method: 'GET', path: '/v1/config' }),
        this.bridge.request('proxy_request', { method: 'GET', path: '/v1/harness' }),
      ]);
      if (this.cancelRequested) throw new Error('desktop_start_cancelled');
      if (!startup.available || config.external_harness) throw new Error('desktop_no_release');
      if (['running', 'starting', 'stopping'].includes(runtime.harness?.state) || runtime.harness?.pid) throw new Error('desktop_stop_web');
      const release = releases.current_release;
      const source = managedDesktopRoot(startup.data_root, release);
      const { version } = desktopCapability(source);
      const kit = selectDesktopKit(this.resources, config);
      const home = config.harness_preferences?.home || process.env.DSH_HOME || path.join(os.homedir(), '.dsh');
      if (!path.isAbsolute(home)) throw new Error('desktop_invalid_home');
      fs.mkdirSync(this.directory, { recursive: true });
      const operationId = randomUUID();
      const stopFile = path.join(this.directory, `stop-${operationId}.json`);
      const recipe = path.join(this.directory, 'launch.json');
      fs.writeFileSync(recipe, JSON.stringify({ source, home, release, version, kit, operationId, stopFile, stateFile: this.file,
        electronExecutable: this.electronExecutable, electronApp: this.electronApp, electronVersion: process.versions.electron, electronNodeVersion: process.versions.node,
        userData: path.join(this.directory, 'user-data') }), { mode: 0o600 });
      fs.writeFileSync(this.file, JSON.stringify({ phase: 'preparing', stage: 'verify', startedAt: Date.now(), release, version, operationId, pid: process.pid }));
      const worker = fileURLToPath(new URL('./harness-desktop-worker.mjs', import.meta.url)).replace(/app\.asar([\\/])/, 'app.asar.unpacked$1');
      const child = spawn(this.executable, [worker, recipe], {
        detached: true, stdio: 'ignore', windowsHide: true,
        env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
      });
      await new Promise((resolve, reject) => { child.once('spawn', resolve); child.once('error', reject); });
      child.unref();
      return this.status();
    } catch (error) {
      if (fs.existsSync(this.file) && this.status().pid === process.pid) {
        fs.writeFileSync(this.file, JSON.stringify({ phase: 'failed', error: error.message }));
      }
      throw error;
    } finally { this.busy = false; }
  }
}
