// Read-only notification observer: bounded user-visible previews, no decisions.
import fs from 'node:fs';
import { randomUUID } from 'node:crypto';
export const name = 'nexus-notifications';

export function preview(value, limit = 500) {
  if (typeof value !== 'string') return '';
  const clean = value.replace(/\x1b\][^\x07]*(?:\x07|$)/g, '').replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, ' ').replace(/\s+/g, ' ').trim();
  return [...clean].length > limit ? [...clean].slice(0, limit - 1).join('') + '…' : clean;
}
// Same-origin, non-privileged page presence; never accepts notification content.
export function viewHandler(views, changed, now = Date.now) {
  return async (req, res) => {
    const end = status => { res.writeHead(status, { 'cache-control': 'no-store' }); res.end(); };
    if (req.method !== 'POST') return end(405);
    let origin;
    try { origin = new URL(req.headers.origin); } catch { return end(403); }
    if (!['127.0.0.1', 'localhost', '[::1]'].includes(origin.hostname) || origin.protocol !== 'http:'
      || origin.host !== req.headers.host || req.headers['x-nexus-view'] !== '1'
      || req.headers['content-type'] !== 'application/json') return end(403);
    try {
      let body = '', size = 0;
      for await (const chunk of req) {
        size += Buffer.byteLength(chunk);
        if (size > 2048) return end(413);
        body += chunk;
      }
      const value = JSON.parse(body);
      if (typeof value.page !== 'string' || !/^[a-zA-Z0-9-]{1,80}$/.test(value.page)
        || typeof value.session !== 'string' || value.session.length > 200 || typeof value.focused !== 'boolean') return end(400);
      for (const [id, view] of views) if (now() - view.time > 10000) views.delete(id);
      if (!value.focused) views.delete(value.page);
      else {
        if (views.size >= 64) views.delete(views.keys().next().value);
        views.set(value.page, { session: value.session, focused: true, time: now() });
      }
      changed(); end(204);
    } catch { if (!res.headersSent) end(400); }
  };
}
function textContent(message) {
  return Array.isArray(message?.content) ? message.content.filter(b => b.type === 'text').map(b => preview(b.text)).join(' ') : '';
}
function questionText(value) {
  try {
    const args = typeof value === 'string' ? JSON.parse(value) : value;
    return Array.isArray(args?.questions) ? args.questions.slice(0, 5).map(q => preview(q.question)).filter(Boolean).join(' / ') : '';
  } catch { return ''; }
}
export function createTracker(send, later = setTimeout, cancel = clearTimeout, titleOf = () => '') {
  const pending = new Map();
  const turns = new Map();
  const seen = new Set();
  const titles = new Map();
  function emit(kind, session, key, body = '', detailCode) {
    const sid = typeof session === 'string' ? session : String(session?.header?.id ?? '');
    let title = titles.get(sid) || '';
    if (typeof session === 'object') {
      try { title = titleOf(session) || title || session.header?.title || ''; } catch {}
    }
    const id = `${sid}:${kind}:${key}`.slice(0, 600);
    if (seen.has(id)) return;
    seen.add(id);
    if (seen.size > 2048) seen.delete(seen.values().next().value);
    send({ kind, session: sid.slice(0, 200), id, title: preview(title, 120), body: preview(body), ...(detailCode ? { detailCode } : {}) });
  }
  function clear(key) { if (pending.has(key)) cancel(pending.get(key)); pending.delete(key); }
  return {
    event(session, event) {
      if (session?.header?.origin === 'subagent') return;
      const sid = String(session?.header?.id ?? '');
      if (!sid || !event?.data) return;
      const d = event.data;
      if (event.type === 'session/title') {
        titles.set(sid, preview(d.title, 120));
        if (titles.size > 2048) titles.delete(titles.keys().next().value);
      }
      if (event.type === 'user/message' && !titles.has(sid)) {
        const title = preview(textContent(d.message), 120);
        if (title) titles.set(sid, title);
        if (titles.size > 2048) titles.delete(titles.keys().next().value);
      }
      if (event.type === 'turn/start') {
        turns.set(sid, { turn: d.turn, reply: '' });
        if (turns.size > 2048) turns.delete(turns.keys().next().value);
      }
      const turn = turns.get(sid);
      if (event.type === 'assistant/message' && turn?.turn === d.turn) turn.reply = preview(textContent(d.message));
      // Delay transient requests so immediately auto-resolved approvals don't alert.
      const key = `${sid}:${d.id ?? d.callId}`;
      if (event.type === 'approval/asked' || (event.type === 'tool/call' && d.name === 'ask_user_question')) {
        clear(key);
        if (pending.size >= 2048) clear(pending.keys().next().value);
        pending.set(key, later(() => {
          pending.delete(key);
          emit(event.type === 'approval/asked' ? 'approval' : 'question', session, d.id ?? d.callId,
            event.type === 'approval/asked' ? [preview(d.toolName, 80), preview(d.reason)].filter(Boolean).join(' — ') : questionText(d.arguments));
        }, 1000));
      }
      if (event.type === 'approval/decided' || event.type === 'tool/result') clear(key);
      if (event.type === 'turn/end') {
        for (const key of pending.keys()) if (key.startsWith(`${sid}:`)) clear(key);
        const live = turn?.turn === d.turn;
        turns.delete(sid);
        if (!live) return; // Ignore history/replayed terminal events.
        const kind = d.reason?.kind;
        if (kind === 'completed') emit('completed', session, d.turn, turn.reply);
        else if (kind === 'error' || kind === 'max-tokens') emit('failed', session, d.turn,
          [preview(d.reason?.error?.message), preview(d.reason?.error?.code, 80)].filter(Boolean).join(' — '), kind);
        else if (kind === 'blocked') emit('blocked', session, d.turn, preview(d.reason?.message) || turn.reply);
      }
    },
    job(job, session) {
      if (job.status === 'completed' || job.status === 'failed') emit(`job-${job.status}`, session ?? job.ownerSession ?? '', job.id,
        [preview(job.label, 250) || preview(job.id, 100), preview(job.detail)].filter(Boolean).join(' — '));
    },
    dispose() { for (const key of pending.keys()) clear(key); turns.clear(); titles.clear(); },
  };
}

export function apply(ctx) {
  const file = process.env.NEXUS_NOTIFICATION_FILE;
  const run = process.env.NEXUS_NOTIFICATION_RUN;
  if (!file || !run) return;
  const epoch = randomUUID();
  let sequence = 0, disposed = false;
  const events = [], capabilities = [];
  const views = new Map();
  function publish() {
    if (disposed) return;
    const temp = `${file}.${epoch}.tmp`;
    try {
      const state = () => JSON.stringify({ run, epoch, sequence, capabilities, events, views: [...views.values()].filter(v => Date.now() - v.time <= 10000) });
      let snapshot = state();
      while (Buffer.byteLength(snapshot) > 240000 && events.length) {
        events.shift(); snapshot = state();
      }
      fs.writeFileSync(temp, snapshot, { mode: 0o600 });
      fs.renameSync(temp, file);
    } catch {
      // Notification failure must not fail a turn or change an approval decision.
      try { fs.unlinkSync(temp); } catch {}
    }
  }
  const tracker = createTracker(event => {
    events.push({ ...event, sequence: ++sequence, time: Date.now() });
    if (events.length > 128) events.shift();
    publish();
  }, setTimeout, clearTimeout, session => ctx.get('sessionTitle')?.get(session)?.title);
  ctx.inject(['webServer'], c => c.effect(() => c.webServer.register({
    kind: 'exact', path: '/nexus-notifications/view', handler: viewHandler(views, publish),
  })));
  ctx.inject(['sessions'], c => c.effect(() => {
    capabilities.push('sessions'); publish();
    const stop = c.on('session/event', (session, event) => tracker.event(session, event));
    return () => { stop(); tracker.dispose(); capabilities.splice(capabilities.indexOf('sessions'), 1); publish(); };
  }));
  ctx.inject(['jobs'], c => c.effect(() => {
    if (typeof c.jobs.onJobDone !== 'function') return;
    capabilities.push('jobs'); publish();
    const stop = c.jobs.onJobDone((job, owner) => tracker.job(job, ctx.get('sessions')?.get?.(job.ownerSession ?? owner?.id)));
    return () => { stop(); capabilities.splice(capabilities.indexOf('jobs'), 1); publish(); };
  }));
  ctx.effect(() => () => { disposed = true; tracker.dispose(); });
  publish();
}
