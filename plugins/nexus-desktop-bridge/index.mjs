import fs from 'node:fs';
import { randomUUID } from 'node:crypto';
// Read-only browser evidence; never grants readiness for release promotion.
export const name = 'nexus-desktop-bridge';
export function observeHostStartup(ctx, file, run, autoOpen = false) {
  if (!file || !run) return;
  // The official CLI commits this only after boot() has checked required
  // entries. Register synchronously: awaiting readiness here would deadlock boot.
  ctx.inject(['appReady'], c => c.effect(() => c.appReady.onReady(() => {
    const temporary = `${file}.${process.pid}.tmp`;
    try {
      fs.writeFileSync(temporary, JSON.stringify({ run, pid: process.pid, state: 'ready', auto_open: autoOpen }), { mode: 0o600 });
      fs.renameSync(temporary, file);
    } catch { try { fs.unlinkSync(temporary); } catch {} }
  })));
}
export function healthHandler(publish) {
  const token = randomUUID();
  return async (req, res) => {
    const end = status => { res.writeHead(status, { 'cache-control': 'no-store' }); res.end(); };
    let origin;
    try { origin = new URL(req.headers.origin); } catch { return end(403); }
    if (req.method !== 'POST') return end(405);
    if (origin.protocol !== 'http:' || !['localhost', '127.0.0.1', '[::1]'].includes(origin.hostname)
      || origin.host !== req.headers.host || req.headers['x-nexus-health'] !== '1'
      || req.headers['content-type'] !== 'application/json') return end(403);
    try {
      let body = '', size = 0;
      for await (const chunk of req) {
        size += Buffer.byteLength(chunk);
        if (size > 49152) return end(413);
        body += chunk;
      }
      const value = JSON.parse(body);
      if (value.action === 'begin') {
        res.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
        res.end(JSON.stringify({ token })); return;
      }
      if (value.token !== token) return end(409);
      const label = v => typeof v === 'string' && v.length <= 240;
      if (!['checking', 'blocked', 'active', 'limited', 'unverified'].includes(value.state)
        || !Array.isArray(value.entries) || value.entries.length > 128
        || !value.entries.every(e => label(e.name) && label(e.state) && Array.isArray(e.missing)
          && e.missing.length <= 32 && e.missing.every(label))
        || !Array.isArray(value.missing_core) || value.missing_core.length > 16 || !value.missing_core.every(label)
        || typeof value.truncated !== 'boolean') return end(400);
      // Copy only the diagnostic schema. No messages, credentials or session data.
      publish({ state: value.state, entries: value.entries.map(e => ({ name: e.name, state: e.state, missing: e.missing })),
        missing_core: value.missing_core, truncated: value.truncated, observed_at: Date.now() });
      end(204);
    } catch { if (!res.headersSent) end(400); }
  };
}
export function apply(ctx) {
  observeHostStartup(ctx, process.env.NEXUS_HOST_STARTUP_FILE, process.env.NEXUS_BROWSER_HEALTH_RUN, process.env.NEXUS_OPEN_AFTER_READY === '1');
  const file = process.env.NEXUS_BROWSER_HEALTH_FILE, run = process.env.NEXUS_BROWSER_HEALTH_RUN;
  if (!file || !run) return;
  let disposed = false;
  const publish = value => {
    if (disposed) return;
    const temp = `${file}.${process.pid}.tmp`;
    try {
      fs.writeFileSync(temp, JSON.stringify({ ...value, run }), { mode: 0o600 });
      fs.renameSync(temp, file);
    } catch { try { fs.unlinkSync(temp); } catch {} }
  };
  ctx.inject(['webServer'], c => c.effect(() => c.webServer.register({
    kind: 'exact', path: '/nexus-browser-health', handler: healthHandler(publish),
  })));
  ctx.effect(() => () => { disposed = true; });
}
