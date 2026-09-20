import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';

test('failed preparation stop retains ownership until retry and never starts Desktop', { timeout: 15000 }, async t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-worker-stop-'));
  const put = (name, content) => {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, content); return file;
  };
  const stateFile = path.join(root, 'state.json'), stopFile = path.join(root, 'stop.json');
  const state = () => JSON.parse(fs.readFileSync(stateFile));
  const wait = async predicate => {
    const deadline = Date.now() + 5000;
    while (!predicate()) {
      if (Date.now() > deadline) throw new Error('Worker fixture timed out');
      await new Promise(resolve => setTimeout(resolve, 20));
    }
  };
  // Keep the real worker state machine; replace only external preparation,
  // platform stop execution and runtime discovery with isolated fixtures.
  put('harness-desktop-worker.mjs', fs.readFileSync(new URL('../electron/harness-desktop-worker.mjs', import.meta.url)));
  put('harness-desktop.mjs', "export const desktopCapability = source => ({app: source});");
  put('desktop-runtime.mjs', "export const digest=()=> 'lock'; export const legacyElectronEntry=()=>''; export const portableHostEntry=()=>'';");
  put('desktop-paths.mjs', 'export const desktopSourceView = source => source;');
  put('desktop-process.mjs', "let attempts=0; export async function stopDesktopChild(){if(++attempts===1)throw new Error('fixture stop failed');}");
  put('node_modules/tsx/dist/loader.mjs', '');
  put('node_modules/electron/package.json', JSON.stringify({ version: 'fixture' }));
  put('kit/manifest.json', JSON.stringify({ schema: 3, electronVersion: 'fixture', lockSha256: 'lock' }));
  put('apps/desktop/.keep', '');
  const desktop = put('desktop.mjs', "import fs from 'node:fs';fs.writeFileSync(new URL('./desktop-started',import.meta.url),'unexpected');");
  put('prepare-harness-desktop.mjs', `import fs from 'node:fs';import path from 'node:path';
    const root=process.argv[2], cache=process.argv[4];
    fs.mkdirSync(path.join(cache,'.primary-'+process.pid+'-fixture'),{recursive:true});
    fs.writeFileSync(path.join(root,'prepared'),String(process.pid));
    setInterval(()=>{if(fs.existsSync(path.join(root,'finish')))process.exit(0);},20);`);
  const recipe = put('recipe.json', JSON.stringify({ source: root, kit: path.join(root, 'kit'), userData: path.join(root, 'user'), home: root,
    stateFile, stopFile, operationId: 'fixture', electronVersion: 'fixture', electronNodeVersion: process.versions.node,
    electronExecutable: process.execPath, electronApp: desktop }));
  const child = spawn(process.execPath, [path.join(root, 'harness-desktop-worker.mjs'), recipe], { stdio: 'ignore', windowsHide: true });
  const exited = once(child, 'exit');
  t.after(async () => {
    put('finish', '');
    if (child.exitCode === null && child.signalCode === null) { child.kill(); await exited; }
    fs.rmSync(root, { recursive: true, force: true });
  });
  await wait(() => fs.existsSync(path.join(root, 'prepared')));
  put('stop.json', JSON.stringify({ requestId: 'first' }));
  await wait(() => state().stopError === 'fixture stop failed');
  const preparationPid = Number(fs.readFileSync(path.join(root, 'prepared')));
  const cache = path.join(root, 'user/runtime', `.primary-${preparationPid}-fixture`);
  put('finish', '');
  await new Promise(resolve => setTimeout(resolve, 400));
  assert.equal(child.exitCode, null);
  assert.equal(state().phase, 'preparing');
  assert.equal(state().childPid, preparationPid);
  assert.ok(fs.existsSync(cache), 'failed stop must retain preparation files');
  assert.ok(!fs.existsSync(path.join(root, 'desktop-started')));
  put('stop.json', JSON.stringify({ requestId: 'retry' }));
  await exited;
  assert.equal(state().phase, 'stopped');
  assert.ok(!fs.existsSync(cache));
  assert.ok(!fs.existsSync(path.join(root, 'desktop-started')));
});
