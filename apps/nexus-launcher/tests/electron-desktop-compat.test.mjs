import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';
import os from 'node:os';
import vm from 'node:vm';
import { createPnpm, apply } from '../../../plugins/nexus-desktop-compat/index.mjs';
import { RustBridge } from '../electron/bridge.mjs';

const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
const config = { node: process.execPath, entry: path.resolve('fixture/cli.mjs'), home: path.resolve('fixture/home'), dir: path.resolve('fixture/home/profiles/web'), profile: 'web', pnpm: path.resolve('fixture/pnpm.cjs') };
function subprocess() {
  const outcome = deferred(), tree = deferred(); let terminated = 0, spec;
  const child = { stdout: new PassThrough(), stderr: new PassThrough(), done: outcome.promise, waitForExit: () => tree.promise, terminate: () => { terminated++; } };
  return { service: { spawn: value => { spec = value; return child; } }, outcome, tree, get spec() { return spec; }, get terminated() { return terminated; } };
}

test('package service preserves profile/Node ABI and retains ownership until the process tree exits', async () => {
  const proc = subprocess(); const provider = createPnpm(proc.service, config, { PATH: 'inherited', ELECTRON_RUN_AS_NODE: '1', npm_config_target: '44' });
  const handle = provider.api.runExternalMarketPluginInstall(['add', '@example/plugin@1.2.3'], config.dir);
  assert.deepEqual(proc.spec.argv, [config.node, config.entry, 'plugin', '--profile', 'web', 'add', '@example/plugin@1.2.3']);
  assert.equal(proc.spec.env.DSH_HOME, config.home); assert.equal(proc.spec.env.ELECTRON_RUN_AS_NODE, undefined);
  assert.equal(proc.spec.env.npm_config_target, undefined); assert.ok(proc.spec.env.PATH.startsWith(path.dirname(process.execPath)));
  proc.outcome.resolve({ exitCode: 0, signal: null }); await Promise.resolve();
  assert.throws(() => provider.api.run(['list']), /already running/);
  handle.cancel(); assert.equal(proc.terminated, 1);
  proc.tree.resolve(); assert.deepEqual(await handle.done, { exitCode: 0, signal: null });
  await provider.dispose(); assert.throws(() => provider.api.run(['list']), /disposed/);
});

test('package service rejects ambiguous installs and probe mutations, and disposal waits for cleanup', async () => {
  const proc = subprocess(), provider = createPnpm(proc.service, config, {});
  for (const args of [['add', 'pkg@latest'], ['remove', 'pkg@1.0.0'], ['add', 'pkg@1.0.0', '--profile=x']]) {
    assert.throws(() => provider.api.runExternalMarketPluginInstall(args, config.dir));
  }
  assert.throws(() => createPnpm(proc.service, config, { NEXUS_DESKTOP_PROBE: '1' }).api.run(['add', 'pkg']), /probe/);
  const handle = provider.api.run(['install']); let disposed = false;
  const cleanup = provider.dispose().then(() => { disposed = true; });
  await Promise.resolve(); assert.equal(disposed, false); assert.equal(proc.terminated, 1);
  proc.outcome.resolve({ exitCode: null, signal: 'SIGTERM' }); proc.tree.resolve();
  await cleanup; assert.equal((await handle.done).signal, 'SIGTERM');
});

test('adapter reconnects after exit without replaying an uncertain mutation', async () => {
  const children = [];
  const bridge = new RustBridge('fixture', () => {
    const child = new EventEmitter(); child.stdout = new PassThrough(); child.stdin = new PassThrough(); child.kill = () => {};
    children.push(child); return child;
  });
  const first = bridge.request('proxy_request', { method: 'POST' });
  children[0].emit('exit', 1); await assert.rejects(first, /check operation status/);
  const next = bridge.request('proxy_request', { method: 'GET' });
  assert.equal(children.length, 2);
  const request = JSON.parse(children[1].stdin.read().toString()); assert.equal(request.args.method, 'GET');
  children[0].emit('exit', 1); // old callbacks cannot fail the replacement.
  children[1].stdout.write(JSON.stringify({ id: request.id, value: 'ready' }) + '\n');
  assert.equal(await next, 'ready'); bridge.close(); await assert.rejects(bridge.request('x'), /closed/);
});

test('published dshmarket consumes Nexus package handles for npm and GitHub sources', { skip: !process.env.NEXUS_TEST_MARKET_ROOT }, async () => {
  const { createDesktopPluginRuntime } = await import(pathToFileURL(path.join(process.env.NEXUS_TEST_MARKET_ROOT, 'lib/dsh-cli.js')).href);
  for (const target of ['example-plugin@1.2.3', 'github:example/plugin']) {
    const proc = subprocess(), provider = createPnpm(proc.service, config, {});
    const market = createDesktopPluginRuntime(provider.api, config.dir, config.dir, 1000);
    try {
      const result = market.runPlugin('web', ['add', target]);
      await new Promise(resolve => setImmediate(resolve));
      assert.ok(proc.spec, 'published market invoked the Nexus provider');
      assert.ok(proc.spec.argv.includes(target)); assert.ok(proc.spec.argv.includes('--reporter=ndjson'));
      proc.outcome.resolve({ exitCode: 0, signal: null }); proc.tree.resolve();
      assert.equal((await result).exitCode, 0);
    } finally { await market.dispose(); await provider.dispose(); }
  }
});

test('real Cordis mounts desktop services and disposes a running package operation', { skip: !process.env.NEXUS_TEST_DSH_ROOT }, async () => {
  const require = createRequire(path.join(process.env.NEXUS_TEST_DSH_ROOT, 'apps/cli/package.json'));
  const { Context } = await import(pathToFileURL(require.resolve('@deepseek-ai/cordis')).href);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-compat-'));
  const old = process.env.NEXUS_DESKTOP_CONTEXT; let fork;
  try {
    const dir = path.join(root, 'profiles/web'); fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({ dsh: { profile: { bundles: [] } } }));
    process.env.NEXUS_DESKTOP_CONTEXT = JSON.stringify({ ...config, root, home: root, bridge: process.execPath });
    const proc = subprocess(), ctx = new Context(); ctx.provide('subprocess', proc.service);
    fork = ctx.plugin({ name: 'nexus-test-compat', apply });
    await new Promise(r => setTimeout(r, 50));
    assert.equal(ctx.desktopProfiles.current.name, 'web'); assert.equal(ctx.desktopProfiles.list()[0].name, 'web');
    const handle = ctx.desktopPnpm.run(['list']);
    const dispose = fork.dispose(); await new Promise(r => setTimeout(r, 20)); assert.equal(proc.terminated, 1);
    proc.outcome.resolve({ exitCode: 0, signal: null }); proc.tree.resolve(); await handle.done; await dispose;
  } finally {
    await fork?.dispose(); if (old === undefined) delete process.env.NEXUS_DESKTOP_CONTEXT; else process.env.NEXUS_DESKTOP_CONTEXT = old;
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('client factory registers native geometry/chooser and routes a notification to its session', async () => {
  let plugin, poll, healthy, opened, ack; const cleanups = [], tasks = [];
  const native = () => Promise.resolve('/native'); const previous = () => Promise.resolve('/browser');
  const services = { uiWorkspace: { pickDirectory: previous }, sessions: { refresh: async () => {}, open: id => { opened = id; } } };
  const context = { ...services, loader: { await: async () => {}, entries: () => [{ fiber: { state: 2 } }, { disabled: true }] },
    reflect: { provide: (key, value) => { services[key] = value; return () => delete services[key]; } },
    effect: run => { const dispose = run(); if (dispose) cleanups.push(dispose); }, inject: (_names, run) => run(context) };
  const sandbox = { window: { __ModuleLoader__: { load: value => { plugin = value.factory(); } }, __DSH_DESKTOP_PICK_DIRECTORY__: native,
    addEventListener() {}, removeEventListener() {},
    nexusShell: { platform: 'win32', health: async value => { healthy = value; }, session: async () => 'session-1', sessionOpened: async id => { ack = id; }, onSession: () => () => {} } },
    location: { href: 'http://127.0.0.1/?nexus-session=session-1' }, history: { replaceState() {} }, URL,
    crypto: { randomUUID: () => 'test-page' }, AbortSignal, fetch: async () => {},
    document: { visibilityState: 'visible', hasFocus: () => true, addEventListener() {}, removeEventListener() {} },
    setTimeout: fn => { tasks.push(fn); return 1; }, clearTimeout() {}, setInterval: fn => { poll = fn; return 2; }, clearInterval() {} };
  vm.runInNewContext(fs.readFileSync(new URL('../../../plugins/nexus-desktop-bridge/client.js', import.meta.url), 'utf8'), sandbox);
  plugin.apply(context); for (const task of tasks) await task(); await poll();
  assert.equal(healthy, true); assert.equal(opened, 'session-1'); assert.equal(ack, 'session-1');
  assert.equal(await services.uiWorkspace.pickDirectory(), '/native'); assert.equal(services.desktopWindow.safeAreaInsets.top, 0);
  for (const dispose of cleanups.reverse()) await dispose(); assert.equal(services.uiWorkspace.pickDirectory, previous);
});
