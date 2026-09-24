import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { detachDesktopSourceLinks } from '../electron/harness-desktop-pnpm.mjs';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-pnpm-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const source = path.join(root, 'source'), profile = path.join(root, 'profile');
  for (const dir of [source, profile]) fs.mkdirSync(dir);
  const link = (target, destination) => {
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.symlinkSync(target, destination, process.platform === 'win32' ? 'junction' : 'dir');
  };
  return { root, source, profile, link };
}

test('detach both upstream fallback projections and direct source links without deleting source', t => {
  const f = fixture(t);
  const pkg = path.join(f.source, 'package'); fs.mkdirSync(pkg);
  fs.writeFileSync(path.join(pkg, 'package.json'), '{"name":"@official/core"}');
  const fallback = path.join(f.profile, '.dsh-module-fallback/node_modules/@official/core');
  const projected = path.join(f.profile, 'node_modules/@official/core');
  f.link(pkg, fallback); f.link(fallback, projected);
  f.link(pkg, path.join(f.profile, 'node_modules/direct'));
  assert.equal(detachDesktopSourceLinks(f), 3);
  assert.equal(fs.readFileSync(path.join(pkg, 'package.json'), 'utf8'), '{"name":"@official/core"}');
  assert.equal(fs.existsSync(projected), false);
  assert.equal(fs.existsSync(fallback), false);
  assert.equal(detachDesktopSourceLinks(f), 0);
});

test('preserve installed packages, external plugin links, and profile configuration', t => {
  const f = fixture(t);
  const external = path.join(f.root, 'source-other'); fs.mkdirSync(external);
  fs.writeFileSync(path.join(external, 'plugin.js'), 'user plugin');
  f.link(external, path.join(f.profile, 'node_modules/plugin'));
  fs.mkdirSync(path.join(f.profile, 'node_modules/installed'));
  fs.writeFileSync(path.join(f.profile, 'package.json'), 'user config');
  assert.equal(detachDesktopSourceLinks(f), 0);
  assert.ok(fs.lstatSync(path.join(f.profile, 'node_modules/plugin')).isSymbolicLink());
  assert.equal(fs.readFileSync(path.join(f.profile, 'package.json'), 'utf8'), 'user config');
});

test('refuse redirected scope before touching any valid source projection', t => {
  const f = fixture(t);
  const pkg = path.join(f.source, 'package'); fs.mkdirSync(pkg);
  f.link(pkg, path.join(f.profile, 'node_modules/plain'));
  f.link(f.source, path.join(f.profile, 'node_modules/@redirected'));
  assert.throws(() => detachDesktopSourceLinks(f), /redirected/);
  assert.ok(fs.lstatSync(path.join(f.profile, 'node_modules/plain')).isSymbolicLink());
  assert.ok(fs.existsSync(pkg));
});
