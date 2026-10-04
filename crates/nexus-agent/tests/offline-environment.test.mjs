import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { environmentShims, mergeEnvironment } from '../scripts/offline-package.mjs';

async function fixture(run) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'nexus-offline-environment-'));
  const put = async (file, text) => { await fs.mkdir(path.dirname(file), { recursive: true }); await fs.writeFile(file, text); };
  const link = async (target, file) => { await fs.mkdir(path.dirname(file), { recursive: true }); await fs.symlink(target, file, 'junction'); };
  const pkg = async (folder, name, version = '1.0.0', bin = { [name]: 'cli.js' }) => {
    await put(path.join(folder, 'package.json'), JSON.stringify({ name, version, bin }));
    await put(path.join(folder, 'cli.js'), `console.log(${JSON.stringify(version)})`);
  };
  try { await run({ root, put, link, pkg }); }
  finally { await fs.rm(root, { recursive: true, force: true }); }
}

test('reimport regenerates current shims without scanning unreferenced prior-slot package units', async () => fixture(async ({ root, put, link, pkg }) => {
  const oldSlot = path.join(root, 'old-slot'), slot = path.join(root, 'new-slot');
  const home = path.join(root, 'old-home'), environment = path.join(root, 'new-home'), work = path.join(root, 'work');
  await pkg(path.join(oldSlot, 'tool'), 'tool', '0.5.0');
  await pkg(path.join(slot, 'tool'), 'tool');
  await link(path.join(oldSlot, 'tool'), path.join(home, '.packages/old-unit/node_modules/tool'));
  await put(path.join(home, 'profiles/web/package.json'), '{}');
  await put(path.join(work, 'payload/environment/profiles/web/package.json'), '{}');
  await put(path.join(work, 'payload/manifest.json'), JSON.stringify({
    schema: 3, platform: process.platform, arch: process.arch, version: 'fixture', total: 2,
    entries: [
      { path: 'environment', kind: 'directory' }, { path: 'environment/profiles', kind: 'directory' },
      { path: 'environment/profiles/web', kind: 'directory' },
      { path: 'environment/profiles/web/package.json', kind: 'file', size: 2, sha256: '0'.repeat(64), mode: 0o644 },
    ], links: [], active_profile: 'web',
    contents: { runtime: false, environment: false, sessions: false, profiles: ['web'], plugins: true, configuration: false, credentials: false },
  }));
  await mergeEnvironment({ work, home, environment, id: 'fixture' });
  await fs.rename(path.join(work, 'payload/merged-environment'), environment);
  await link(path.join(slot, 'tool'), path.join(environment, 'profiles/web/node_modules/tool'));
  const calls = [];
  await environmentShims(environment, slot, { shim: async (from, to) => calls.push({ from, to }) });
  assert.equal(calls.length, 1);
  assert.equal(await fs.realpath(calls[0].from), await fs.realpath(path.join(slot, 'tool/cli.js')));
  assert.equal(calls[0].to, path.join(environment, 'profiles/web/node_modules/.bin/tool'));
  assert.equal(await fs.realpath(path.join(environment, '.packages/old-unit/node_modules/tool')), await fs.realpath(path.join(oldSlot, 'tool')));
}));

test('retained profiles keep separate package versions and reachable transitive shims terminate through cycles', { timeout: 10000 }, async () => fixture(async ({ root, link, pkg }) => {
  const environment = path.join(root, 'environment'), slot = path.join(root, 'slot');
  const previous = path.join(environment, '.packages/previous/node_modules/plugin');
  const incoming = path.join(environment, '.packages/incoming/node_modules/plugin');
  const dependency = path.join(environment, '.packages/dependency/node_modules/dependency');
  await fs.mkdir(slot);
  await pkg(previous, 'plugin', '0.5.0'); await pkg(incoming, 'plugin', '1.0.0', undefined);
  await pkg(dependency, 'dependency');
  await link(previous, path.join(environment, 'profiles/private/node_modules/plugin'));
  await link(incoming, path.join(environment, 'profiles/web/node_modules/plugin'));
  await link(dependency, path.join(incoming, 'node_modules/dependency'));
  await link(incoming, path.join(dependency, 'node_modules/plugin'));
  const calls = [];
  await environmentShims(environment, slot, { shim: async (from, to) => calls.push({ from, to }) });
  assert.equal(JSON.parse(await fs.readFile(path.join(environment, 'profiles/private/node_modules/plugin/package.json'))).version, '0.5.0');
  assert.equal(JSON.parse(await fs.readFile(path.join(environment, 'profiles/web/node_modules/plugin/package.json'))).version, '1.0.0');
  assert.ok(calls.some(call => call.to === path.join(environment, 'profiles/private/node_modules/.bin/plugin')));
  assert.ok(calls.some(call => path.basename(call.to) === 'dependency'));
  assert.ok(calls.length <= 8, 'cyclic units must have bounded shim generation');
}));

test('an actively referenced executable in a prior slot still fails the environment boundary', async () => fixture(async ({ root, link, pkg }) => {
  const environment = path.join(root, 'environment'), slot = path.join(root, 'slot'), previous = path.join(root, 'prior-slot/tool');
  await fs.mkdir(slot); await pkg(previous, 'tool');
  await link(previous, path.join(environment, 'profiles/private/node_modules/tool'));
  let calls = 0;
  await assert.rejects(environmentShims(environment, slot, { shim: async () => { calls++; } }), error => {
    assert.equal(error.message, 'Plugin executable leaves environment at "profiles/private/node_modules/tool/cli.js"');
    assert.ok(!error.message.includes(root), 'diagnostic must not include an absolute receiver path');
    return true;
  });
  assert.equal(calls, 0);
}));

test('rejected executable diagnostics remain relative through an aliased environment root', async () => fixture(async ({ root, link, pkg }) => {
  const environment = path.join(root, 'environment'), alias = path.join(root, 'alias');
  const slot = path.join(root, 'slot'), previous = path.join(root, 'prior-slot/tool');
  await fs.mkdir(slot); await pkg(previous, 'tool');
  await link(previous, path.join(environment, 'profiles/private/node_modules/tool'));
  await link(environment, alias);
  let calls = 0;
  await assert.rejects(environmentShims(alias, slot, { shim: async () => { calls++; } }), {
    message: 'Plugin executable leaves environment at "profiles/private/node_modules/tool/cli.js"',
  });
  assert.equal(calls, 0);
}));

test('a reachable pnpm unit regenerates executable shims for hoisted sibling dependencies', async () => fixture(async ({ root, link, pkg }) => {
  const environment = path.join(root, 'environment'), slot = path.join(root, 'slot');
  const unit = path.join(environment, '.packages/active/node_modules');
  await fs.mkdir(slot);
  await pkg(path.join(unit, 'plugin'), 'plugin', '1.0.0', {});
  await pkg(path.join(unit, 'dependency'), 'dependency');
  await link(path.join(unit, 'plugin'), path.join(environment, 'profiles/web/node_modules/plugin'));
  const calls = [];
  await environmentShims(environment, slot, { shim: async (from, to) => calls.push({ from, to }) });
  const expectedDirectory = await fs.realpath(path.join(unit, '.bin'));
  const expectedSource = await fs.realpath(path.join(unit, 'dependency/cli.js'));
  let found = false;
  for (const call of calls) if (path.basename(call.to) === 'dependency'
    && await fs.realpath(path.dirname(call.to)) === expectedDirectory
    && await fs.realpath(call.from) === expectedSource) found = true;
  assert.ok(found, 'hoisted shim must use the actual unit directory and dependency executable');
}));

test('reachable package executables cannot escape their package lexically or through links', async () => fixture(async ({ root, put, link, pkg }) => {
  const environment = path.join(root, 'environment'), slot = path.join(root, 'slot'), installed = path.join(environment, 'profiles/web/node_modules/tool');
  await fs.mkdir(slot); await pkg(installed, 'tool', '1.0.0', { tool: '../outside.js' });
  await assert.rejects(environmentShims(environment, slot, { shim: async () => assert.fail('must not create escaped shim') }), /Plugin executable leaves its package/);
  await put(path.join(installed, 'package.json'), JSON.stringify({ name: 'tool', bin: { tool: 'linked/cli.js' } }));
  await pkg(path.join(root, 'outside'), 'outside');
  await link(path.join(root, 'outside'), path.join(installed, 'linked'));
  await assert.rejects(environmentShims(environment, slot, { shim: async () => assert.fail('must not create escaped shim') }), /Plugin executable leaves environment/);
}));
