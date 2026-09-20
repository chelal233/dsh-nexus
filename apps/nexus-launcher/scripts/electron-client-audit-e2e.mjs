// Real Electron + localhost + shipped client/HTTP publisher. The plugin tree is
// a controlled fixture, not acceptance of a particular upstream Harness build.
import { app, BrowserWindow } from 'electron';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'node:http';
import { ClientAudit } from '../electron/client-audit.mjs';
import { apply } from '../../../plugins/nexus-desktop-bridge/index.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
fs.mkdirSync(path.join(root, 'electron-dist'), { recursive: true });
const output = fs.mkdtempSync(path.join(root, 'electron-dist/client-audit-'));
app.setPath('userData', path.join(output, 'desktop'));
app.on('window-all-closed', () => {});

void app.whenReady().then(async () => {
  const file = path.join(output, 'browser-health.json');
  const source = fs.readFileSync(path.join(root, '../../plugins/nexus-desktop-bridge/client.js'), 'utf8');
  let route, dispose, rows = [], services = ['sessions', 'uiRenderer', 'uiSession', 'uiWorkspace'], noReport = false;
  function publishFor(run) {
    dispose?.();
    process.env.NEXUS_BROWSER_HEALTH_FILE = file;
    process.env.NEXUS_BROWSER_HEALTH_RUN = run;
    apply({
      inject(_names, fn) { fn({ effect: fn => fn(), webServer: { register(value) { route = value.handler; return () => {}; } } }); },
      effect(fn) { dispose = fn(); },
    });
  }
  const server = createServer((req, res) => {
    if (req.url === '/nexus-browser-health') { void route(req, res); return; }
    res.writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' });
    res.end(`<!doctype html><title>Local audit fixture</title><script>
      window.__ModuleLoader__ = { load({factory}) {
        ${noReport ? 'return;' : ''}
        const rows = ${JSON.stringify(rows)}, services = ${JSON.stringify(services)};
        factory().apply({ loader: { entries: () => rows, await: async () => {} },
          get: name => services.includes(name) ? {} : undefined,
          effect: () => {}, inject: () => {} });
      } };
      ${source}
    </script>`);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${server.address().port}/`;
  const windows = [];
  let clock = Date.now();
  const audit = new ClientAudit({ now: () => clock, createWindow: options => {
    const window = new BrowserWindow(options); windows.push(window); return window;
  } });
  const info = run => {
    let health = { state: 'unverified' };
    try { const value = JSON.parse(fs.readFileSync(file, 'utf8')); if (value.run === run) health = value; } catch {}
    return { available: true, generation: 1, run_id: run, url, browser_health: health };
  };
  const awaitState = async (run, state) => {
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      const result = audit.observe(info(run));
      if (result.browser_health.state === state) return result.browser_health;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    throw Error(`No ${state} report for ${run}`);
  };
  try {
    publishFor('healthy'); rows = [{ options: { name: 'disabled', disabled: true }, disabled: true }];
    await awaitState('healthy', 'active');
    assert.equal(windows.length, 1); assert.equal(windows[0].isVisible(), false);
    assert.equal(await windows[0].webContents.executeJavaScript('typeof require'), 'undefined');

    publishFor('broken'); rows = [{ options: { name: 'broken-import' } }, { options: { name: 'chat' }, fiber: { state: 0, inject: { sessions: null } } }];
    services = [];
    const blocked = await awaitState('broken', 'blocked');
    assert.equal(windows[0].isDestroyed(), true);
    assert.deepEqual(blocked.entries.map(e => e.name), ['broken-import', 'chat']);
    assert.deepEqual(blocked.entries[1].missing, ['sessions']);

    publishFor('limited'); rows = [];
    await awaitState('limited', 'limited');
    publishFor('silent'); noReport = true;
    assert.equal(audit.observe(info('silent')).browser_health.state, 'checking');
    clock += 45000;
    assert.equal(audit.observe(info('silent')).browser_health.reason, 'client_audit_timeout');
    audit.observe({ available: false });
    assert.ok(windows.every(window => window.isDestroyed()));
    console.log('PASS: hidden Electron audit, shipped bridge publication, active/blocked/limited/timeout, run replacement and cleanup');
  } finally {
    audit.stop(); dispose?.(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
  }
}).then(() => app.exit(0), error => { console.error(error); app.exit(1); });
