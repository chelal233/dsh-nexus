import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { Readable } from 'node:stream';
import { createRequire } from 'node:module';
import { healthHandler } from './index.mjs';

let bridge;
vm.runInNewContext(fs.readFileSync(new URL('./client.js', import.meta.url), 'utf8'), {
  window: { __ModuleLoader__: { load: ({ factory }) => { bridge = factory(); } } },
});
const core = { sessions: {}, uiRenderer: {}, uiSession: {}, uiWorkspace: {} };
const context = (entries, services = core) => ({ loader: { entries: () => entries }, get: name => services[name] });
const row = (name, state, inject = {}) => ({ options: { name }, ...(state === undefined ? {} : { fiber: { state, inject } }) });

test('activation audit covers upstream states, imports and missing services', () => {
  assert.equal(bridge.inspectClient(context([row('ready', 2)])).state, 'active');
  for (const state of [undefined, 0, 1, 3, 4, 5]) {
    assert.equal(bridge.inspectClient(context([row('broken', state)])).state, 'blocked');
  }
  const result = bridge.inspectClient(context([row('sessions-provider', 0, { fileUpload: null }), row('chat', 0, { sessions: null })], {}));
  assert.equal(result.entries[0].missing[0], 'fileUpload');
  assert.equal(result.entries[1].missing[0], 'sessions');
  assert.equal(result.state, 'blocked');
  assert.equal(bridge.inspectClient(context([row('future', 99)])).state, 'unverified');
  assert.equal(bridge.inspectClient(context([row('loading', 1)]), false, 1).state, 'checking');
});

test('a plugin the user disabled is a choice, not a broken import', () => {
  const disabled = { options: { name: 'third-party', disabled: true }, disabled: true };
  const healthy = bridge.inspectClient(context([row('ready', 2), disabled]));
  assert.equal(healthy.state, 'active');
  assert.equal(healthy.entries.length, 0);
  // An entry that should have loaded and has no fiber is still a failure.
  assert.equal(bridge.inspectClient(context([row('broken')])).state, 'blocked');
});

test('Nexus also checks unavailable core services and bounds large plugin trees', () => {
  assert.equal(bridge.inspectClient(context([row('ready', 2)], {})).state, 'limited');
  const result = bridge.inspectClient(context(Array.from({ length: 200 }, (_, i) => row(`p${i}`, 0))));
  assert.equal(result.entries.length, 128);
  assert.equal(result.truncated, true);
});

test('a hung startup still reports heartbeat diagnostics without accumulating loader waiters', async () => {
  let plugin, start, tick, now = 0, waits = 0, settle;
  const reports = [], dispose = [];
  const pending = new Promise(resolve => { settle = resolve; });
  vm.runInNewContext(fs.readFileSync(new URL('./client.js', import.meta.url), 'utf8'), {
    window: { __ModuleLoader__: { load: ({ factory }) => { plugin = factory(); } } },
    Date: { now: () => now }, AbortSignal,
    setTimeout: fn => { start = fn; return 1; },
    setInterval: fn => { tick = fn; return 2; },
    clearTimeout() {}, clearInterval() {},
    fetch: async (_url, options) => {
      const body = JSON.parse(options.body);
      if (body.action !== 'begin') reports.push(body);
      return { ok: true, status: 200, json: async () => ({ token: 'current-run' }) };
    },
  });
  plugin.apply({
    ...context([row('hung-startup', 1)]),
    loader: { entries: () => [row('hung-startup', 1)], await: () => { waits++; return pending; } },
    effect: setup => { dispose.push(setup()); }, inject() {},
  });
  await start();
  assert.equal(reports.at(-1).state, 'checking');
  now = 5000; await tick();
  assert.equal(reports.length, 2);
  assert.equal(reports.at(-1).state, 'checking');
  now = 30000; await tick();
  assert.equal(reports.at(-1).state, 'blocked');
  assert.equal(reports.at(-1).entries[0].name, 'hung-startup');
  assert.equal(waits, 1);
  const beforeDispose = reports.length;
  for (const cleanup of dispose) cleanup();
  await tick(); settle(); await Promise.resolve();
  assert.equal(reports.length, beforeDispose);
  assert.equal(waits, 1);
});

test('differential audit against the selected upstream assertEntriesActive implementation', { skip: !process.env.NEXUS_TEST_DSH_ROOT }, () => {
  const source = fs.readFileSync(path.join(process.env.NEXUS_TEST_DSH_ROOT, 'packages/client/web/src/boot-client.ts'), 'utf8');
  const require = createRequire(new URL('../../apps/nexus-launcher/package.json', import.meta.url));
  const ts = require('typescript');
  const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS } }).outputText;
  const exports = {};
  vm.runInNewContext(compiled, { exports, require: id => {
    if (id.endsWith('loader-status.ts')) return { STATE_LABELS: ['pending', 'loading', 'active', 'failed', 'disposed', 'unloading'] };
    return {};
  } });
  for (const state of [undefined, 0, 1, 2, 3, 4, 5]) {
    const ctx = context([row('example', state, { absent: null })]);
    let failed = false;
    try { exports.assertEntriesActive(ctx); } catch { failed = true; }
    assert.equal(bridge.inspectClient(ctx).state === 'blocked', failed, `upstream state ${state}`);
  }
});

async function request(body, headers = {}) {
  let captured, status, response;
  const handler = healthHandler(value => { captured = value; });
  const send = async value => {
    const req = Readable.from([JSON.stringify(value)]);
    req.method = 'POST';
    req.headers = { origin: 'http://127.0.0.1:1234', host: '127.0.0.1:1234', 'content-type': 'application/json', 'x-nexus-health': '1', ...headers };
    await handler(req, { writeHead: n => { status = n; }, end: text => { response = text; } });
  };
  await send({ action: 'begin' });
  const token = response ? JSON.parse(response).token : '';
  await send({ token, ...body });
  return { captured, status };
}
test('report route validates origin and bounded schema and drops extra fields', async () => {
  const body = { state: 'blocked', entries: [{ name: 'chat', state: 'pending', missing: ['sessions'] }], missing_core: ['sessions'], truncated: false, secret: 'not persisted' };
  const valid = await request(body);
  assert.equal(valid.status, 204);
  assert.equal(valid.captured.secret, undefined);
  assert.equal((await request({ ...body, token: 'old-host-token' })).status, 409);
  assert.equal((await request(body, { origin: 'https://untrusted.example' })).status, 403);
  assert.equal((await request({ ...body, entries: [{}] })).status, 400);
  assert.equal((await request({ ...body, extra: 'x'.repeat(50000) })).status, 413);
});
