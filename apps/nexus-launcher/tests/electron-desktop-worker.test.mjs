import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { pathToFileURL } from 'node:url';

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
  put('desktop-startup-audit.mjs', fs.readFileSync(new URL('../electron/desktop-startup-audit.mjs', import.meta.url)));
  put('harness-desktop-pnpm.mjs', fs.readFileSync(new URL('../electron/harness-desktop-pnpm.mjs', import.meta.url)));
  put('harness-desktop.mjs', "export const desktopCapability = source => ({app: source});");
  put('desktop-runtime.mjs', "export const desktopKitMatchesSource=()=>true; export const legacyElectronEntry=()=>''; export const portableHostEntry=()=>'';");
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
  const retryFixture = put('transient-state-lock.mjs', `import fs from 'node:fs';
    const rename = fs.renameSync; let attempts = 0;
    fs.renameSync = (...args) => {
      if (process.platform === 'win32' && args[1].endsWith('state.json') && attempts++ < 2)
        throw Object.assign(new Error('fixture sharing violation'), {code:'EPERM'});
      return rename(...args);
    };`);
  const child = spawn(process.execPath, ['--import', pathToFileURL(retryFixture).href, path.join(root, 'harness-desktop-worker.mjs'), recipe], { stdio: 'ignore', windowsHide: true });
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
  assert.ok(state().stageDurations.verify >= 0);
  assert.ok(!fs.existsSync(cache));
  assert.ok(!fs.existsSync(path.join(root, 'desktop-started')));
});

test('official Desktop worker opens a visible Windows window', { skip: process.platform !== 'win32', timeout: 30000 }, async t => {
  const { createRequire } = await import('node:module');
  const { execFileSync } = await import('node:child_process');
  const electron = createRequire(import.meta.url)('electron');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-visible-desktop-'));
  const put = (name, content) => {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, content); return file;
  };
  put('harness-desktop-worker.mjs', fs.readFileSync(new URL('../electron/harness-desktop-worker.mjs', import.meta.url)));
  put('desktop-startup-audit.mjs', fs.readFileSync(new URL('../electron/desktop-startup-audit.mjs', import.meta.url)));
  put('harness-desktop-pnpm.mjs', fs.readFileSync(new URL('../electron/harness-desktop-pnpm.mjs', import.meta.url)));
  put('harness-desktop.mjs', 'export const desktopCapability = source => ({app:source});');
  put('desktop-runtime.mjs', "export const desktopKitMatchesSource=()=>true; export const legacyElectronEntry=()=>''; export const portableHostEntry=()=>'';");
  put('desktop-paths.mjs', 'export const desktopSourceView=source=>source;');
  put('desktop-process.mjs', fs.readFileSync(new URL('../electron/desktop-process.mjs', import.meta.url)));
  put('node_modules/tsx/dist/loader.mjs', '');
  put('node_modules/electron/package.json', JSON.stringify({version:'fixture'}));
  put('kit/manifest.json', JSON.stringify({schema:3,electronVersion:'fixture',lockSha256:'lock'}));
  put('apps/desktop/.keep', '');
  put('prepare-harness-desktop.mjs', '');
  const app = put('official.cjs', `const {app,BrowserWindow}=require('electron');
    app.whenReady().then(()=>{new BrowserWindow({show:true,title:'Nexus visibility regression'});});`);
  const stateFile = path.join(root,'state.json'), stopFile=path.join(root,'stop.json');
  const recipe=put('recipe.json',JSON.stringify({source:root,kit:path.join(root,'kit'),home:root,userData:path.join(root,'user'),
    stateFile,stopFile,operationId:'visibility-test',electronVersion:'fixture',electronNodeVersion:process.versions.node,
    electronExecutable:electron,electronApp:app}));
  const worker=spawn(process.execPath,[path.join(root,'harness-desktop-worker.mjs'),recipe],{stdio:'ignore',windowsHide:true});
  const exited=once(worker,'exit');
  t.after(async()=>{
    put('stop.json',JSON.stringify({requestId:'cleanup'}));
    await Promise.race([exited,new Promise(resolve=>setTimeout(resolve,6000))]);
    if(worker.exitCode===null && worker.signalCode===null){
      const state=fs.existsSync(stateFile)?JSON.parse(fs.readFileSync(stateFile)):{};
      if(state.childPid) execFileSync('taskkill.exe',['/PID',String(state.childPid),'/T','/F'],{windowsHide:true,stdio:'ignore'});
      worker.kill();await exited;
    }
    // Windows can retain Chromium directory handles briefly after process exit.
    // Retry transient cleanup errors, but still fail if the directory stays locked.
    await fs.promises.rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  });
  const deadline=Date.now()+15000;
  let handle='0';
  while(Date.now()<deadline){
    const state=fs.existsSync(stateFile)?JSON.parse(fs.readFileSync(stateFile)):{};
    assert.notEqual(state.phase,'failed',state.error);
    if(state.phase==='launched'){
      handle=execFileSync(process.env.NEXUS_TEST_POWERSHELL||'powershell.exe', ['-NoProfile','-NonInteractive','-Command',
        `(Get-Process -Id ${Number(state.childPid)} -ErrorAction Stop).MainWindowHandle.ToInt64()`],{encoding:'utf8',windowsHide:true}).trim();
      if(handle!=='0')break;
    }
    await new Promise(resolve=>setTimeout(resolve,100));
  }
  assert.notEqual(handle,'0','the actual official Desktop child must have a visible top-level window');
  const launched=JSON.parse(fs.readFileSync(stateFile));
  assert.ok(launched.stageDurations.verify >= 0);
  assert.ok(launched.stageDurations.launch >= 0);
  const measured=JSON.stringify(launched.stageDurations);
  await new Promise(resolve=>setTimeout(resolve,100));
  assert.equal(JSON.stringify(JSON.parse(fs.readFileSync(stateFile)).stageDurations),measured,'runtime use must not inflate preparation timings');
});
