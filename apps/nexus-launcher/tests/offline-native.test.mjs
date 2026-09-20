import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const app = fileURLToPath(new URL('..', import.meta.url));
const tools = process.env.NEXUS_TEST_OFFLINE_TOOLS || path.join(app, 'desktop/resources/runtime');
const helper = fileURLToPath(new URL('../../../crates/nexus-agent/scripts/offline-package.mjs', import.meta.url));

test('native offline roundtrip preserves executable modes, relocates links and rejects foreign targets',
  { skip: process.platform === 'win32', timeout: 120000 }, t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-offline-native-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const slot = path.join(root, 'source'), runtime = path.join(root, 'runtime');
  const put = (name, text, mode = 0o644) => { fs.mkdirSync(path.dirname(name), { recursive: true }); fs.writeFileSync(name, text); fs.chmodSync(name, mode); };
  put(path.join(slot, 'apps/cli/lib/bin.js'), 'console.log("fixture");', 0o755);
  put(path.join(slot, 'node_modules/fixture/bin.js'), '#!/usr/bin/env node\nconsole.log("relocated");', 0o755);
  fs.mkdirSync(path.join(slot, 'node_modules/.bin'));
  fs.symlinkSync('../fixture/bin.js', path.join(slot, 'node_modules/.bin/fixture'));
  put(path.join(slot, 'node_modules/.modules.yaml'), JSON.stringify({ layoutVersion: 5, virtualStoreDir: '/old/machine/.pnpm' }));
  put(path.join(runtime, 'node/npm'), '#!/bin/sh\nexit 0\n', 0o755);
  fs.copyFileSync(process.execPath, path.join(runtime, 'node/node'));
  fs.chmodSync(path.join(runtime, 'node/node'), 0o755);
  put(path.join(runtime, 'node/node_modules/npm/bin/npm-cli.js'), 'console.log("11.7.0")');
  put(path.join(runtime, 'pnpm/bin/pnpm.cjs'), 'console.log("11.7.0")');
  const writerJs = path.join(root, 'writer.mjs'), writer = path.join(root, 'writer');
  put(writerJs, 'import fs from "node:fs";import {pipeline} from "node:stream/promises";await pipeline(process.stdin,fs.createWriteStream(process.argv[3],{flags:"wx",mode:0o600}));');
  const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
  put(writer, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(writerJs)} "$@"\n`, 0o755);
  const archive = path.join(root, 'offline.tar.gz');
  const selected = { runtime: true, profiles: [], environment: false, configuration: false, sessions: false, plugins: false, credentials: false };
  function run(action, extra = {}) {
    const work = path.join(root, action === 'finalize' ? 'import' : action);
    fs.mkdirSync(work, { recursive: true });
    const jobFile = path.join(work, 'job.json');
    fs.writeFileSync(jobFile, JSON.stringify({ id: action, action, work, archive, slot, runtime, tools, private_writer: writer, version: 'fixture', nexus: {}, contents: selected, ...extra }));
    const code = `import http from 'node:http';import https from 'node:https';const deny=()=>{throw Error('network forbidden')};http.request=http.get=https.request=https.get=globalThis.fetch=deny;process.umask(0o077);process.argv=[process.execPath,${JSON.stringify(helper)},${JSON.stringify(jobFile)}];await import(${JSON.stringify(pathToFileURL(helper).href)});`;
    return execFileSync(process.execPath, ['--input-type=module', '-e', code], { encoding: 'utf8', timeout: 90000, stdio: ['ignore', 'pipe', 'pipe'] });
  }
  run('export'); run('import');
  const imported = path.join(root, 'import/payload');
  run('finalize', { slot: path.join(imported, 'slot'), runtime: path.join(imported, 'runtime') });
  const executable = path.join(imported, 'runtime/node/node');
  assert.equal(fs.statSync(executable).mode & 0o777, 0o755);
  const bin = path.join(imported, 'slot/node_modules/.bin/fixture');
  assert.equal(fs.readlinkSync(bin), '../fixture/bin.js');
  assert.equal(execFileSync(executable, [bin], { encoding: 'utf8' }).trim(), 'relocated');
  const manifestFile = path.join(imported, 'manifest.json'), manifest = JSON.parse(fs.readFileSync(manifestFile));
  assert.equal(manifest.platform, process.platform); assert.equal(manifest.arch, process.arch);
  manifest.arch = process.arch === 'arm64' ? 'x64' : 'arm64'; fs.writeFileSync(manifestFile, JSON.stringify(manifest));
  assert.throws(() => run('finalize', { slot: path.join(imported, 'slot'), runtime: path.join(imported, 'runtime') }), /Command failed/);
});
