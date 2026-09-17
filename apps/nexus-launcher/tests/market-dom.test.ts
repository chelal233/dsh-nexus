import assert from 'node:assert/strict';
import test from 'node:test';
import { JSDOM } from 'jsdom';
import { mockIPC, clearMocks } from './desktop-mocks.ts';
import { createUiTestLoader } from './ui-test-loader.ts';

test('marketplace selection sends the displayed profile scope once and reports failed installation', async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: 'http://localhost/' });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator,
    HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, Event: dom.window.Event, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  let root, loader, act;
  try {
    const react = await import('react'); act = react.act;
    const { createRoot } = await import('react-dom/client');
    const state = { profile: 'web', scope: 'C:/test/profiles/web', provider: 'none', status: 'ready', installed: false };
    const profiles = {api_version:'v1',active_profile:'web',manifests:[{name:'web',bundles:[],plugins:[]}]};
    const posts = []; let settle;
    mockIPC((command, payload) => {
      assert.equal(command, 'proxy_request');
      if (payload.path === '/v1/profiles' && payload.method === 'GET') return profiles;
      assert.equal(payload.path, '/v1/market');
      if (payload.method === 'GET') return { ...state };
      posts.push(payload.body);
      return new Promise((_resolve, reject) => { settle = () => reject(new Error('installation fixture failed')); });
    });
    loader = await createUiTestLoader();
    const { MarketplaceSettings } = await loader.loadModule('/src/App.tsx');
    root = createRoot(document.getElementById('root'));
    await act(async () => root.render(react.createElement(MarketplaceSettings, {snapshot: {profiles, startup: {available: true}, harnessRuntime:{harness:{state:'stopped'}}}, busyAction:null, refresh:async()=>{}, runAction:async()=>true})));
    assert.equal(document.querySelector('select'), null);
    await act(async () => { document.querySelector('button').click(); document.querySelector('button').click(); });
    assert.deepEqual(posts, [{ profile: state.profile, scope: state.scope, provider: 'dsh-market' }]);
    assert.equal(document.querySelector('button').disabled, true);
    await act(async () => settle());
    assert.match(document.querySelector('[role="alert"]').textContent, /installation fixture failed/);
    assert.equal(document.querySelector('button').disabled, false);
    assert.equal(document.querySelector('[role="status"]'), null);
  } finally {
    if (root && act) await act(async () => root.unmount());
    await loader?.close(); clearMocks(); dom.window.close();
    for (const [key, value] of previous) value ? Object.defineProperty(globalThis, key, value) : delete globalThis[key];
  }
});
