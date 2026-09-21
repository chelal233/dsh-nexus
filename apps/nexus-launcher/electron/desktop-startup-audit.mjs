import fs from 'node:fs';
import path from 'node:path';

// Windows readers or scanners can briefly deny replacement. Keep atomicity:
// never delete the destination, and surface persistent or unrelated errors.
export function renameDesktopState(temporary, file, {
  rename = fs.renameSync, platform = process.platform,
  wait = () => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50),
} = {}) {
  for (let attempt = 0; ; attempt++) {
    try { rename(temporary, file); return; }
    catch (error) {
      if (platform !== 'win32' || !['EPERM','EACCES','EBUSY'].includes(error.code) || attempt >= 10) throw error;
      wait();
    }
  }
}

export const startupEvidenceFile = recipe => path.join(path.dirname(recipe.stateFile), `startup-${recipe.operationId}.json`);
const clean = value => String(value?.message ?? value).slice(-6000).replace(/([?&]token=)[^\s&]+/gi, '$1[redacted]');

// New profiles are created by upstream. Existing profiles must remain intact,
// including malformed manifests which need an actionable repair, not a reset.
export function checkDesktopProfile(home) {
  const file = path.join(home, 'profiles/desktop/package.json');
  let stat;
  try { stat = fs.statSync(file); } catch (error) { if (error.code === 'ENOENT') return; throw error; }
  if (!stat.isFile() || stat.size > 1024 * 1024) throw Error(`Desktop profile manifest is invalid: ${file}`);
  let manifest;
  try { manifest = JSON.parse(fs.readFileSync(file, 'utf8')); }
  catch { throw Error(`Desktop profile JSON is invalid. Repair this file and retry: ${file}`); }
  if (!Array.isArray(manifest.dsh?.profile?.bundles) || !manifest.dsh.profile.bundles.every(name => typeof name === 'string' && name.length > 0))
    throw Error(`Desktop profile bundles are invalid. Repair this file and retry: ${file}`);
}

export function desktopAuditSupported(source) {
  try {
    const read = file => fs.readFileSync(path.join(source, file), 'utf8');
    return read('apps/desktop/src/ipc.ts').includes("'dsh-desktop:boot-failed'") &&
      read('packages/client/web/src/boot-client.ts').includes('assertEntriesActive(ctx)') &&
      read('packages/client/web/src/boot.ts').includes('await mountClient(ctx, this.container)') &&
      read('packages/client/web/src/boot-page.ts').includes('this.root.dataset.dshBoot');
  } catch { return false; }
}

// Observe upstream's validated boot IPC and mounted document, without changing
// its profile, plugin tree, recovery UI, or startup outcome. Stops after startup.
export function installDesktopStartupAudit({ app, ipcMain, recipe, setTimer = setInterval, clearTimer = clearInterval, now = Date.now }) {
  const file = startupEvidenceFile(recipe), began = now();
  let state = 'checking', hostReady = false, timer, checking = false, contents;
  const publish = (next, error) => {
    if (state !== 'checking') return;
    state = next;
    const temporary = `${file}.${process.pid}.tmp`;
    fs.writeFileSync(temporary, JSON.stringify({ operationId: recipe.operationId, pid: process.pid, state, elapsedMs: now() - began, error: error && clean(error) }), { mode: 0o600 });
    renameDesktopState(temporary, file);
    if (state !== 'checking') clearTimer(timer);
  };
  if (!desktopAuditSupported(recipe.source)) { publish('unverified', 'Desktop startup observation is unsupported by this Harness version.'); return; }
  publish('checking');
  const trusted = event => event.senderFrame === event.sender?.mainFrame && event.sender.getURL().startsWith('dsh-app://app/');
  const handle = ipcMain.handle.bind(ipcMain);
  ipcMain.handle = (channel, listener) => handle(channel, !['dsh-desktop:boot', 'dsh-desktop:boot-failed'].includes(channel) ? listener : async (event, ...args) => {
    try {
      const result = await listener(event, ...args);
      if (trusted(event)) {
        contents = event.sender;
        if (channel === 'dsh-desktop:boot-failed') publish('failed', args[0]);
        else hostReady = true;
      }
      return result;
    } catch (error) {
      if (channel === 'dsh-desktop:boot' && trusted(event)) publish('failed', error);
      throw error;
    }
  });
  app.on('web-contents-created', (_event, web) => {
    web.on('render-process-gone', () => { if (contents === web) publish('failed', 'Desktop renderer exited during startup.'); });
    web.on('did-fail-load', (_event, code, description, url, mainFrame) => {
      if (mainFrame && url.startsWith('dsh-app://app/') && code !== -3) publish('failed', description);
    });
  });
  timer = setTimer(async () => {
    if (state !== 'checking') return;
    if (now() - began > 90000) { publish('unverified', 'Desktop startup verification timed out. The process remains available for inspection or stopping.'); return; }
    if (checking) return;
    if (!hostReady || !contents || contents.isDestroyed()) return;
    checking = true;
    try {
      // Upstream mounts the UI only after assertEntriesActive() succeeds.
      // A boot placeholder or bare web address is never accepted as readiness.
      const mounted = await contents.executeJavaScript("Boolean(location.href.startsWith('dsh-app://app/') && document.querySelector('#root')?.childElementCount && !document.querySelector('[data-dsh-boot]'))");
      if (mounted) publish('ready');
    } catch { /* Navigation may still be settling. */ }
    finally { checking = false; }
  }, 500);
  timer?.unref?.();
  app.once('will-quit', () => clearTimer(timer));
}
