import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync, copyFileSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

// Run after prepare:runtime. No system Node/npm/pnpm or developer tools may
// satisfy the nested build commands that failed on a clean Windows machine.
for (const systemRootKey of (process.platform === 'win32' ? ['SystemRoot', 'SYSTEMROOT'] : ['unix'])) test(`bundled pnpm -> npm -> node works without system tools (${systemRootKey})`, {
  skip: !['win32', 'darwin', 'linux'].includes(process.platform),
}, () => {
  const runtime = process.env.NEXUS_TEST_RUNTIME_DIR
    || fileURLToPath(new URL('../desktop/resources/runtime/', import.meta.url));
  const node = path.join(runtime, process.platform === 'win32' ? 'node/node.exe' : 'node/node');
  const pnpm = path.join(runtime, 'pnpm/bin/pnpm.cjs');
  const fixture = mkdtempSync(path.join(tmpdir(), 'nexus runtime 用户 '));
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) =>
    !['PATH', 'SYSTEMROOT'].includes(key.toUpperCase()) && !/^(npm_|pnpm_)/i.test(key)));
  if (process.platform === 'win32') env[systemRootKey] = Object.entries(process.env).find(([key]) => key.toUpperCase() === 'SYSTEMROOT')?.[1];
  const systemRoot = Object.entries(env).find(([key]) => key.toUpperCase() === 'SYSTEMROOT')?.[1];
  if (process.platform === 'win32') assert.ok(systemRoot, 'Windows SystemRoot is required for the isolated runtime test');
  env.PATH = [path.dirname(node), path.dirname(pnpm), ...(process.platform === 'win32' ? [path.join(systemRoot, 'System32')] : ['/usr/bin', '/bin'])].join(path.delimiter);
  env.EXPECTED_NODE = node;
  env.NEXUS_RUNTIME_NODE = node;
  try {
    writeFileSync(path.join(fixture, 'package.json'), JSON.stringify({
      name: 'nexus-bundled-runtime-check', private: true,
      scripts: {
        build: 'npm run build:lib:host && npm run build:lib:client && pnpm run build:web',
        'build:lib:host': 'node verify.cjs host',
        'build:lib:client': 'node verify.cjs client',
        'build:web': 'node verify.cjs web',
      },
    }));
    writeFileSync(path.join(fixture, 'verify.cjs'), `
      const fs = require('node:fs');
      const assert = require('node:assert/strict');
      assert.equal(fs.realpathSync(process.execPath).toLowerCase(), fs.realpathSync(process.env.EXPECTED_NODE).toLowerCase());
      fs.appendFileSync('result.txt', process.argv[2] + '\\n');
    `);
    const result = spawnSync(node, [pnpm, 'run', 'build'], {
      cwd: fixture, env, encoding: 'utf8', windowsHide: true, timeout: 60000,
    });
    assert.equal(result.error, undefined);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.equal(readFileSync(path.join(fixture, 'result.txt'), 'utf8'), 'host\nclient\nweb\n');
  } finally {
    rmSync(fixture, { recursive: true, force: true });
  }
});

// Exercise helpers and child discovery, not just git --version.
test('bundled Git commits and clones locally without host Git', () => {
  const runtime = process.env.NEXUS_TEST_RUNTIME_DIR || fileURLToPath(new URL('../desktop/resources/runtime/', import.meta.url));
  const root = path.join(runtime, 'git');
  const windows = process.platform === 'win32';
  const binary = path.join(root, windows ? 'cmd/git.exe' : 'bin/git');
  const platformRoot = windows ? path.join(root, process.arch === 'arm64' ? 'clangarm64' : 'mingw64') : root;
  const fixture = mkdtempSync(path.join(tmpdir(), 'nexus Git 用户 '));
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^(GIT_|PATH$|HOME$|USERPROFILE$)/i.test(key)));
  env.PATH = [path.dirname(binary), ...(windows ? [path.join(platformRoot, 'bin'), path.join(root, 'usr/bin'), path.join(process.env.SystemRoot, 'System32')] : ['/usr/bin', '/bin'])].join(path.delimiter);
  Object.assign(env, { HOME: fixture, USERPROFILE: fixture, GIT_CONFIG_GLOBAL: path.join(fixture, 'empty-config'), GIT_EXEC_PATH: path.join(platformRoot, 'libexec/git-core'), GIT_TERMINAL_PROMPT: '0' });
  if (!windows) Object.assign(env, { GIT_CONFIG_SYSTEM: path.join(root, 'etc/gitconfig'), GIT_TEMPLATE_DIR: path.join(root, 'share/git-core/templates') });
  if (process.platform === 'linux') Object.assign(env, { PREFIX: root, GIT_SSL_CAINFO: path.join(root, 'ssl/cacert.pem') });
  const git = (...args) => {
    const result = spawnSync(binary, args, { cwd: fixture, env, encoding: 'utf8', windowsHide: true, timeout: 30000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    return result.stdout.trim();
  };
  try {
    assert.match(git('--version'), /^git version 2\.53\.0/);
    assert.equal(path.resolve(git('--exec-path')), path.resolve(env.GIT_EXEC_PATH));
    git('init', 'source');
    writeFileSync(path.join(fixture, 'source/proof.txt'), 'offline Git works');
    git('-C', 'source', 'add', 'proof.txt');
    git('-C', 'source', '-c', 'user.name=Nexus Test', '-c', 'user.email=test@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture');
    git('clone', '--no-local', 'source', 'copy');
    assert.equal(readFileSync(path.join(fixture, 'copy/proof.txt'), 'utf8'), 'offline Git works');
    assert.equal(git('-C', 'source', 'rev-parse', 'HEAD'), git('-C', 'copy', 'rev-parse', 'HEAD'));
  } finally { rmSync(fixture, { recursive: true, force: true }); }
});

test('nested bundled pnpm honors an explicitly selected Node executable', () => {
  const runtime = process.env.NEXUS_TEST_RUNTIME_DIR || fileURLToPath(new URL('../desktop/resources/runtime/', import.meta.url));
  const fixture = mkdtempSync(path.join(tmpdir(), 'nexus explicit Node 用户 '));
  const node = path.join(fixture, process.platform === 'win32' ? 'node.exe' : 'node');
  const pnpm = path.join(runtime, 'pnpm/bin/pnpm.cjs');
  try {
    copyFileSync(path.join(runtime, process.platform === 'win32' ? 'node/node.exe' : 'node/node'), node);
    chmodSync(node, 0o755);
    writeFileSync(path.join(fixture, 'package.json'), JSON.stringify({ private: true, scripts: { check: 'pnpm run verify', verify: 'node verify.cjs' } }));
    writeFileSync(path.join(fixture, 'verify.cjs'), "require('node:assert/strict').equal(require('node:fs').realpathSync(process.execPath), require('node:fs').realpathSync(process.env.NEXUS_RUNTIME_NODE));");
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^(PATH$|npm_|pnpm_)/i.test(key)));
    env.NEXUS_RUNTIME_NODE = node;
    env.PATH = [fixture, path.dirname(pnpm), ...(process.platform === 'win32' ? [path.join(process.env.SystemRoot, 'System32')] : ['/usr/bin', '/bin'])].join(path.delimiter);
    const result = spawnSync(node, [pnpm, 'run', 'check'], { cwd: fixture, env, encoding: 'utf8', windowsHide: true, timeout: 60000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
  } finally { rmSync(fixture, { recursive: true, force: true }); }
});

// Both layouts can be selected explicitly by users. Test the actual executable
// siblings, not only npm-cli.js (which bypasses the broken distribution link).
test('Unix distribution bin npm and npx remain relocatable', { skip: process.platform === 'win32' }, () => {
  const runtime = process.env.NEXUS_TEST_RUNTIME_DIR || fileURLToPath(new URL('../desktop/resources/runtime/', import.meta.url));
  const bin = path.join(runtime, 'node/bin');
  for (const name of ['npm', 'npx']) {
    const result = spawnSync(path.join(bin, name), ['--version'], { env: { ...process.env, PATH: '/usr/bin:/bin' }, encoding: 'utf8', timeout: 30000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.match(result.stdout.trim(), /^\d+\.\d+\.\d+/);
  }
});
