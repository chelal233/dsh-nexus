import { harnessUrl } from './policy.mjs';

// Observe the real Host. Keep the page alive so the existing health report's
// freshness boundary remains meaningful; never manufacture a successful audit.
export class ClientAudit {
  constructor({ createWindow, now = Date.now, openReady, openedRun }) {
    Object.assign(this, { createWindow, now, openReady, openedRun });
  }
  observe(info) {
    if (this.stopped) return info;
    if (info?.available !== true || typeof info.run_id !== 'string' || !info.run_id) {
      this.clear(); return info;
    }
    let url;
    try { url = harnessUrl(info.url); } catch { this.clear(); return info; }
    const key = JSON.stringify([info.generation, info.run_id, url.href]);
    if (this.key !== key) {
      this.clear();
      this.key = key; this.started = this.now(); this.reason = undefined;
      const window = this.createWindow({ show: false, webPreferences: {
        sandbox: true, contextIsolation: true, nodeIntegration: false,
        webviewTag: false, backgroundThrottling: false,
        // Memory-only session, separate from Launcher and independent windows.
        partition: 'nexus-client-audit',
      } });
      this.window = window;
      const fail = () => {
        if (this.window !== window) return;
        this.reason = 'client_audit_load_failed';
        this.window = undefined;
        window.destroy();
      };
      window.webContents.session.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
      window.webContents.session.setPermissionCheckHandler(() => false);
      window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
      window.webContents.on('will-attach-webview', event => event.preventDefault());
      const guard = (event, target) => {
        try { if (harnessUrl(target).origin === url.origin) return; } catch {}
        event.preventDefault(); fail();
      };
      window.webContents.on('will-navigate', guard);
      window.webContents.on('will-redirect', guard);
      window.webContents.on('render-process-gone', fail);
      window.on('unresponsive', fail);
      void window.loadURL(url.href).catch(fail);
    }
    const result = this.result(info);
    if (result.open_browser_after_ready === true && ['active', 'limited'].includes(result.browser_health?.state)
        && this.openReady && this.openedRun !== result.run_id) {
      this.openedRun = result.run_id; // Never retry an uncertain external open.
      void Promise.resolve().then(() => this.openReady(url.href, result.run_id)).catch(() => {});
    }
    return result;
  }
  result(info) {
    let url;
    try { url = harnessUrl(info?.url); } catch { return info; }
    if (this.stopped || info?.available !== true || !this.key || JSON.stringify([info.generation, info.run_id, url.href]) !== this.key) return info;
    const health = info.browser_health;
    // Only the Agent's current, fresh evidence can finish the check.
    if (['active', 'limited', 'blocked'].includes(health?.state)) return info;
    const expired = this.now() - this.started >= 45000;
    if (expired || this.reason) {
      return { ...info, browser_health: { ...health, state: 'unverified',
        reason: this.reason || 'client_audit_timeout' } };
    }
    return { ...info, browser_health: { ...health, state: 'checking' } };
  }
  clear() {
    const window = this.window;
    this.window = undefined; this.key = undefined;
    if (window && !window.isDestroyed()) window.destroy();
  }
  stop() { this.stopped = true; this.clear(); }
}
