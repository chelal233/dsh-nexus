import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { desktopCapability } from './harness-desktop.mjs';
import { digest, legacyElectronEntry, portableHostEntry } from './desktop-runtime.mjs';
import { stopDesktopChild } from './desktop-process.mjs';
import { desktopSourceView } from './desktop-paths.mjs';

const recipe = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
let state = { phase: 'preparing', stage: 'verify', startedAt: Date.now(), pid: process.pid, release: recipe.release, version: recipe.version, operationId: recipe.operationId };
function report(update) {
  if (update.stage && update.stage !== state.stage) update.stageStartedAt = Date.now();
  state = { ...state, ...update };
  const temporary = `${recipe.stateFile}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, JSON.stringify(state), { mode: 0o600 });
  fs.renameSync(temporary, recipe.stateFile);
}
let tail = '';
let preparationPid;
let stopFailure;
let activeChild, stopRequested = false, stopBusy = false, stopCompletion = Promise.resolve();
const stopTimer = setInterval(() => {
  if (stopBusy || !recipe.stopFile || !fs.existsSync(recipe.stopFile)) return;
  const request = JSON.parse(fs.readFileSync(recipe.stopFile, 'utf8'));
  fs.unlinkSync(recipe.stopFile);
  stopRequested = true; stopBusy = true;
  const previousPhase = state.phase;
  stopFailure = undefined;
  report({ phase: 'stopping', stopRequestId: request.requestId, stopError: undefined });
  stopCompletion = stopDesktopChild(activeChild).catch(error => {
    stopFailure = error;
    stopRequested = false;
    report({ phase: previousPhase, stopError: error.message });
  }).finally(() => { stopBusy = false; });
}, 200);
const ensureNotStopped = () => { if (stopRequested) throw new Error('desktop_start_cancelled'); };
async function waitForStop() {
  await stopCompletion;
  // A failed group stop must retain ownership and allow a fresh stop request.
  // Never overwrite the error with stopped or remove a live child's files.
  while (stopFailure) {
    await new Promise(resolve => setTimeout(resolve, 100));
    await stopCompletion;
  }
}

const env = { ...process.env, ELECTRON_RUN_AS_NODE: '1' };
delete env.NODE_OPTIONS;
delete env.DSH_DESKTOP_HOST_INSPECT_PORT;
function run(program, args, options = {}) {
  return new Promise((resolve, reject) => {
    ensureNotStopped();
    const child = activeChild = spawn(program, args, { cwd: recipe.source, env, detached: process.platform !== 'win32', windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'], ...options });
    child.once('spawn', () => { preparationPid = child.pid; report({ childPid: child.pid }); });
    const append = chunk => { tail = (tail + chunk.toString()).slice(-12000); report({ detail: tail }); };
    let pending = '';
    child.stdout?.on('data', chunk => {
      pending += chunk.toString();
      const lines = pending.split('\n'); pending = lines.pop();
      for (const line of lines) {
        const stage = line.trim().replace(/^NEXUS_DESKTOP_STAGE:/, '');
        if (line.startsWith('NEXUS_DESKTOP_STAGE:') && ['verify', 'runtime', 'project', 'check', 'cleanup'].includes(stage)) report({ stage });
        else append(line + '\n');
      }
    }); child.stderr?.on('data', append);
    child.once('error', reject);
    child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`Process exited (${code ?? signal})`)));
  });
}
try {
  report({});
  const { app } = desktopCapability(recipe.source);
  const kit = JSON.parse(fs.readFileSync(path.join(recipe.kit, 'manifest.json'), 'utf8'));
  const electronVersion = JSON.parse(fs.readFileSync(path.join(app, 'node_modules/electron/package.json'), 'utf8')).version;
  const shared = recipe.electronVersion === electronVersion;
  const portable = !shared && kit.schema === 3 && /^[a-f0-9]{64}$/.test(kit.hostArchiveSha256 ?? '');
  if (kit.electronVersion !== electronVersion || (!shared && kit.schema === 3 && !portable) || kit.lockSha256 !== digest(path.join(app, 'scripts/primary-runtime-lock.json'))) throw new Error('desktop_runtime_incompatible');
  report({ detail: '' }); tail = '';
  await run(process.execPath, ['--import', pathToFileURL(path.join(recipe.source, 'node_modules/tsx/dist/loader.mjs')).href,
    fileURLToPath(new URL('./prepare-harness-desktop.mjs', import.meta.url)), recipe.source, recipe.kit, path.join(recipe.userData, 'runtime'), ...(shared ? [recipe.electronNodeVersion] : portable ? [kit.electronNodeVersion, 'portable-host'] : [])]);
  await waitForStop();
  ensureNotStopped();
  // Legacy offline exports retain their own Electron when the host differs.
  const electron = shared ? recipe.electronExecutable : portable ? path.join(recipe.userData, 'runtime', `host-${kit.hostArchiveSha256}`, portableHostEntry(kit.platform))
    : kit.schema === 1 ? path.join(recipe.kit, 'electron', legacyElectronEntry()) : path.join(recipe.userData, 'runtime', `electron-${kit.electronArchiveSha256}`, legacyElectronEntry());
  const desktopEnv = { ...env, DSH_HOME: recipe.home, DSH_DESKTOP_OPEN_DEVTOOLS: '0' };
  delete desktopEnv.ELECTRON_RUN_AS_NODE;
  tail = ''; report({ stage: 'launch', childPid: undefined, detail: '' });
  const launchApp = path.join(desktopSourceView(recipe.source), 'apps/desktop');
  ensureNotStopped();
  const args = shared || portable ? [...(shared && recipe.electronApp ? [recipe.electronApp] : []), `--user-data-dir=${recipe.userData}`, `--nexus-official-desktop=${path.resolve(process.argv[2])}`]
    : [`--user-data-dir=${recipe.userData}`, launchApp];
  const child = activeChild = spawn(electron, args, {
    cwd: launchApp, env: desktopEnv, detached: process.platform !== 'win32', stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true,
  });
  const append = chunk => { tail = (tail + chunk.toString()).slice(-12000).replace(/([?&]token=)[^\s&]+/g, '$1[redacted]'); };
  child.stdout.on('data', append); child.stderr.on('data', append);
  await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('spawn', () => report({ phase: 'launched', preparedMs: Date.now() - state.startedAt, childPid: child.pid, detail: undefined }));
    child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`Desktop exited (${code ?? signal})`)));
  });
  await waitForStop();
  report({ phase: 'stopped', childPid: undefined });
} catch (error) {
  await waitForStop();
  if (stopRequested) report({ phase: 'stopped', childPid: undefined, error: undefined, detail: undefined });
  else { report({ phase: 'failed', error: error.message, detail: tail || undefined }); process.exitCode = 1; }
}

finally {
  clearInterval(stopTimer);
  // A terminated extraction cannot run its finally block. Remove only that
  // owned child's temporary trees after its process tree has exited.
  const cache = path.resolve(recipe.userData, 'runtime');
  if (preparationPid && fs.existsSync(cache)) {
    for (const entry of fs.readdirSync(cache, { withFileTypes: true })) {
      if (entry.isDirectory() && !entry.isSymbolicLink() && ['primary', 'host', 'electron'].some(kind => entry.name.startsWith(`.${kind}-${preparationPid}-`))) {
        const target = path.resolve(cache, entry.name);
        if (path.dirname(target) === cache) fs.rmSync(target, { recursive: true, force: true });
      }
    }
  }
}
