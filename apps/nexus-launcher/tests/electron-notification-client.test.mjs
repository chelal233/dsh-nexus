import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import fs from 'node:fs';

test('browser client reports matching conversation focus, visibility, session changes and disposal', async () => {
  const requests = [], listeners = new Map(), timers = new Set(), cleanups = [];
  let plugin, focus = true, current = 's1', subscriber;
  const events = { addEventListener: (name, fn) => listeners.set(name, fn), removeEventListener: name => listeners.delete(name) };
  const document = { ...events, visibilityState: 'visible', hasFocus: () => focus };
  const sandbox = { window: { ...events, __ModuleLoader__: { load: value => { plugin = value.factory(); } } }, document,
    crypto: { randomUUID: () => 'browser-page' }, AbortSignal, URL,
    location: { href: 'http://127.0.0.1:1234/' }, history: { replaceState() {} },
    fetch: async (url, options) => { assert.equal(url, '/nexus-notifications/view'); requests.push(JSON.parse(options.body)); },
    setTimeout: () => 1, clearTimeout() {},
    setInterval: fn => { timers.add(fn); return fn; }, clearInterval: fn => timers.delete(fn),
  };
  const ctx = { sessions: { list: { getSnapshot: () => ({ current }), subscribe: fn => { subscriber = fn; return () => { subscriber = undefined; }; } } },
    inject: (_keys, run) => run(ctx), effect: run => { cleanups.push(run()); } };
  vm.runInNewContext(fs.readFileSync(new URL('../../../plugins/nexus-desktop-bridge/client.js', import.meta.url), 'utf8'), sandbox);
  plugin.apply(ctx); await new Promise(r => setImmediate(r));
  assert.deepEqual(requests.at(-1), { page: 'browser-page', session: 's1', focused: true });
  focus = false; await listeners.get('blur')(); assert.equal(requests.at(-1).focused, false);
  focus = true; document.visibilityState = 'hidden'; await listeners.get('visibilitychange')(); assert.equal(requests.at(-1).focused, false);
  document.visibilityState = 'visible'; current = 's2'; await subscriber();
  assert.equal(requests.at(-1).session, 's2'); assert.equal(requests.at(-1).focused, true);
  cleanups.forEach(fn => fn()); assert.equal(requests.at(-1).focused, false);
  assert.equal(timers.size, 0); assert.equal(listeners.size, 0); assert.equal(subscriber, undefined);
});
