import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

// Run after prepare:runtime. No system Node/npm/pnpm or developer tools may
// satisfy the nested build commands that failed on a clean Windows machine.
for (const systemRootKey of ['SystemRoot', 'SYSTEMROOT']) test(`bundled pnpm -> npm -> node works without system tools (${systemRootKey})`, {
  skip: process.platform !== 'win32',
}, () => {
  const runtime = process.env.NEXUS_TEST_RUNTIME_DIR
    || fileURLToPath(new URL('../src-tauri/resources/runtime/', import.meta.url));
  const node = path.join(runtime, 'node/node.exe');
  const pnpm = path.join(runtime, 'pnpm/bin/pnpm.cjs');
  const fixture = mkdtempSync(path.join(tmpdir(), 'nexus runtime 用户 '));
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) =>
    !['PATH', 'SYSTEMROOT'].includes(key.toUpperCase()) && !/^(npm_|pnpm_)/i.test(key)));
  env[systemRootKey] = Object.entries(process.env).find(([key]) => key.toUpperCase() === 'SYSTEMROOT')?.[1];
  const systemRoot = Object.entries(env).find(([key]) => key.toUpperCase() === 'SYSTEMROOT')?.[1];
  assert.ok(systemRoot, 'Windows SystemRoot is required for the isolated runtime test');
  env.PATH = [path.dirname(node), path.dirname(pnpm), path.join(systemRoot, 'System32')].join(';');
  env.EXPECTED_NODE = node;
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
