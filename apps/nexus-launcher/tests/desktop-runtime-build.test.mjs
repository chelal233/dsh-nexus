import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { preparePrimaryPayload, nativeTar, runtimeInventory } from '../electron/desktop-runtime-cache.mjs';
import { cachedAsset, desktopAssets, pin } from '../desktop/scripts/prepare-desktop-runtime.mjs';
import { verifyDesktopKit, digest } from '../electron/desktop-runtime.mjs';
import { probeDesktopSupport } from '../electron/harness-desktop.mjs';
import { desktopRuntimeForExport, desktopHostForExport } from '../../../crates/nexus-agent/scripts/offline-package.mjs';
const temp = t => { const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-desktop-build-')); t.after(()=>fs.rmSync(root,{recursive:true,force:true})); return root; };

test('Launcher pins the same exact Electron release as official Harness Desktop',()=>{
 const launcher=JSON.parse(fs.readFileSync(new URL('../package.json',import.meta.url),'utf8'));
 assert.equal(launcher.devDependencies.electron,pin.electronVersion);
});

test('build cache rejects corrupt downloads and repairs a stale cache without accepting wrong bytes',async t=>{
 const cache=temp(t), bytes=Buffer.from('verified artifact'), hash=createHash('sha256').update(bytes).digest('hex');
 await assert.rejects(cachedAsset('https://example.test/runtime',hash,cache,async()=>new Response('wrong')),/checksum mismatch/);
 assert.equal(fs.existsSync(path.join(cache,hash)),false);
 fs.writeFileSync(path.join(cache,hash),'old bad cache');
 const file=await cachedAsset('https://example.test/runtime',hash,cache,async()=>new Response(bytes));
 assert.equal(digest(file),hash);
 assert.equal(await cachedAsset('https://example.test/runtime',hash,cache,()=>{throw Error('must stay offline');}),file);
 assert.deepEqual(fs.readdirSync(cache),[hash]);
});

test('archive kits preserve offline export and missing supported targets fail closed',async t=>{
 const root=temp(t), kit=path.join(root,'runtime/desktop'), slot=path.join(root,'slot');
 fs.mkdirSync(path.join(kit,'assets'),{recursive:true});
 const put=(name,bytes)=>{const file=path.join(kit,name);fs.writeFileSync(file,bytes);return {path:name,sha256:digest(file)};};
 const asset=put('asset','payload');fs.renameSync(path.join(kit,'asset'),path.join(kit,'assets',asset.sha256));asset.path=`assets/${asset.sha256}`;
 const target=`${({win32:'win',darwin:'mac',linux:'linux'})[process.platform]}-${process.arch}`;
 const lock={targets:{[target]:{nodeSha256:asset.sha256,pythonSha256:asset.sha256,wheels:[]}},wheels:[]};
 const locked=put('lock.json',JSON.stringify(lock)),archive=put('electron.zip','fixture archive');
 const manifest={schema:2,platform:process.platform,arch:process.arch,supported:true,lockSha256:locked.sha256,electronArchiveSha256:archive.sha256,electronVersion:'44.0.0',pnpmVersion:'11.7.0',files:[locked,asset,archive]};
 const save=()=>fs.writeFileSync(path.join(kit,'manifest.json'),JSON.stringify(manifest));save();
 assert.equal(verifyDesktopKit(kit).archive,path.join(kit,'electron.zip'));
 for(const name of ['scripts','node_modules/electron','node_modules/pnpm'])fs.mkdirSync(path.join(slot,'apps/desktop',name),{recursive:true});
 fs.copyFileSync(path.join(kit,'lock.json'),path.join(slot,'apps/desktop/scripts/primary-runtime-lock.json'));
 for(const [name,version] of [['electron','44.0.0'],['pnpm','11.7.0']])fs.writeFileSync(path.join(slot,'apps/desktop/node_modules',name,'package.json'),JSON.stringify({version}));
 assert.equal(await desktopRuntimeForExport(slot,path.join(root,'runtime')),kit);
 manifest.supported=false;manifest.reason='upstream_target_unsupported';save();
 assert.throws(()=>verifyDesktopKit(kit),/invalid/);
 manifest.supported=true;manifest.files=manifest.files.filter(f=>f.path!=='electron.zip');save();
 assert.throws(()=>verifyDesktopKit(kit),/invalid/);
 await assert.rejects(desktopRuntimeForExport(slot,path.join(root,'runtime')),/Incomplete/);
});

test('unsupported Desktop marker is valid only when the upstream lock omits that target',t=>{
 const root=temp(t);fs.writeFileSync(path.join(root,'lock.json'),JSON.stringify({targets:{'win-x64':{}},wheels:[]}));
 const manifest={schema:2,platform:'win32',arch:'arm64',supported:false,reason:'upstream_target_unsupported',lockSha256:digest(path.join(root,'lock.json')),files:[{path:'lock.json',sha256:digest(path.join(root,'lock.json'))}]};
 fs.writeFileSync(path.join(root,'manifest.json'),JSON.stringify(manifest));
 assert.equal(verifyDesktopKit(root,{platform:'win32',arch:'arm64'}).supported,false);
 const lock={nodeVersion:'24.21.0',pythonVersion:'3.12.14',pythonRelease:'20260901',targets:{'win-x64':{nodeArchive:'win-x64.zip',nodeSha256:'node',pythonTarget:'x86_64-pc-windows-msvc',pythonSha256:'python',wheels:[]}},wheels:[]};
 assert.equal(desktopAssets(lock,'win32','arm64'),null);
 assert.equal(desktopAssets(lock,'win32','x64')[0].url,'https://nodejs.org/dist/v24.21.0/node-v24.21.0-win-x64.zip');
 assert.match(desktopAssets(lock,'win32','x64')[1].url,/3\.12\.14%2B20260901/);
});

test('Linux ARM64 never inherits macOS ARM64 Desktop support', t => {
 const root=temp(t), app=path.join(root,'apps/desktop');fs.mkdirSync(path.join(app,'scripts'),{recursive:true});
 const lock={targets:{'win-x64':{},'mac-x64':{},'mac-arm64':{}},wheels:[]};
 fs.writeFileSync(path.join(app,'package.json'),JSON.stringify({name:'@deepseek-ai/dsh-desktop',main:'lib/main.js',version:'fixture'}));
 fs.writeFileSync(path.join(app,'scripts/primary-runtime-lock.json'),JSON.stringify(lock));
 assert.equal(probeDesktopSupport(root,{platform:'darwin',arch:'arm64'}).supported,true);
 assert.equal(probeDesktopSupport(root,{platform:'linux',arch:'arm64'}).supported,false);
 assert.equal(desktopAssets(lock,'linux','arm64'),null);
 fs.writeFileSync(path.join(root,'lock.json'),JSON.stringify(lock));
 const hash=digest(path.join(root,'lock.json'));
 fs.writeFileSync(path.join(root,'manifest.json'),JSON.stringify({schema:3,platform:'linux',arch:'arm64',supported:false,reason:'upstream_target_unsupported',lockSha256:hash,files:[{path:'lock.json',sha256:hash}]}));
 assert.equal(verifyDesktopKit(root,{platform:'linux',arch:'arm64'}).supported,false);
});

test('parallel inventory preserves order, bounds filesystem work and drains failures', async t => {
 const root=temp(t);
 for(let i=0;i<48;i++) fs.writeFileSync(path.join(root,`file-${i}`),'fixture');
 const original=fs.promises.lstat; let active=0,peak=0;
 fs.promises.lstat=async (...args)=>{
   active++; peak=Math.max(peak,active);
   try { await new Promise(resolve=>setTimeout(resolve,2)); return await original(...args); }
   finally {active--;}
 };
 try {
   const result=await runtimeInventory(root);
   assert.deepEqual(result.map(entry=>entry[0]),['',...fs.readdirSync(root).sort()]);
   assert.ok(peak>1&&peak<=16,`filesystem concurrency ${peak}`);
   const outside=temp(t), link=path.join(root,'escape');
   fs.symlinkSync(outside,link,process.platform==='win32'?'junction':'dir');
   try { await assert.rejects(runtimeInventory(root),/desktop_runtime_invalid/); assert.equal(active,0); }
   finally { fs.unlinkSync(link); }
 } finally {fs.promises.lstat=original;}
});

test('shared runtime cache reuses unchanged files and repairs edits, missing entries, and redirected roots', async t => {
 const root=temp(t), payload=path.join(root,'payload'), cache=path.join(root,'cache');
 fs.mkdirSync(path.join(payload,'primary-runtime'),{recursive:true});
 fs.mkdirSync(path.join(payload,'office-skills'));
 fs.writeFileSync(path.join(payload,'primary-runtime/runtime.json'),'{}');
 fs.writeFileSync(path.join(payload,'primary-runtime/test.txt'),'original');
 const archive=path.join(root,'primary.tar.gz');
 execFileSync(nativeTar(),['-czf',archive,'-C',payload,'primary-runtime','office-skills'],{windowsHide:true});
 const kit={primaryArchive:archive,primaryArchiveSha256:digest(archive)};
 const destination=await preparePrimaryPayload(kit,cache), file=path.join(destination,'primary-runtime/test.txt');
 const original=fs.statSync(file,{bigint:true});
 assert.equal(await preparePrimaryPayload(kit,cache),destination);
 assert.equal(fs.statSync(file,{bigint:true}).ctimeNs,original.ctimeNs);
 fs.writeFileSync(file,'modified');
 fs.utimesSync(file,Number(original.atimeNs)/1e9,Number(original.mtimeNs)/1e9);
 await preparePrimaryPayload(kit,cache); assert.equal(fs.readFileSync(file,'utf8'),'original');
 fs.unlinkSync(file); await preparePrimaryPayload(kit,cache); assert.equal(fs.readFileSync(file,'utf8'),'original');
 fs.rmSync(destination,{recursive:true});
 fs.symlinkSync(payload,destination,process.platform==='win32'?'junction':'dir');
 await preparePrimaryPayload(kit,cache);
 assert.equal(fs.lstatSync(destination).isSymbolicLink(),false);
 assert.equal(fs.readFileSync(path.join(payload,'primary-runtime/test.txt'),'utf8'),'original');
});

test('shared kits export without a second Electron and reject missing or corrupt preassembled payloads', async t => {
 const root=temp(t), kit=path.join(root,'runtime/desktop'), slot=path.join(root,'slot');
 fs.mkdirSync(kit,{recursive:true});
 const target=`${({win32:'win',darwin:'mac',linux:'linux'})[process.platform]}-${process.arch}`;
 fs.writeFileSync(path.join(kit,'lock.json'),JSON.stringify({targets:{[target]:{}}}));
 fs.writeFileSync(path.join(kit,'primary.tar.gz'),'fixture');
 const manifest={schema:3,platform:process.platform,arch:process.arch,supported:true,electronMode:'launcher',electronVersion:'44.0.0',electronNodeVersion:'24.18.1',pnpmVersion:'11.7.0',primarySmokePassed:true,lockSha256:digest(path.join(kit,'lock.json')),primaryArchiveSha256:digest(path.join(kit,'primary.tar.gz')),files:['lock.json','primary.tar.gz'].map(name=>({path:name,sha256:digest(path.join(kit,name))}))};
 fs.writeFileSync(path.join(kit,'manifest.json'),JSON.stringify(manifest));
 for(const name of ['scripts','node_modules/electron','node_modules/pnpm'])fs.mkdirSync(path.join(slot,'apps/desktop',name),{recursive:true});
 fs.copyFileSync(path.join(kit,'lock.json'),path.join(slot,'apps/desktop/scripts/primary-runtime-lock.json'));
 for(const [name,version] of [['electron','44.0.0'],['pnpm','11.7.0']])fs.writeFileSync(path.join(slot,'apps/desktop/node_modules',name,'package.json'),JSON.stringify({version}));
 assert.equal(verifyDesktopKit(kit).electronMode,'launcher');
 assert.equal(await desktopRuntimeForExport(slot,path.join(root,'runtime')),kit);
 assert.equal(fs.existsSync(path.join(kit,'electron.zip')),false);
 fs.writeFileSync(path.join(kit,'primary.tar.gz'),'corrupt');
 assert.throws(()=>verifyDesktopKit(kit),/invalid/);
 await assert.rejects(desktopRuntimeForExport(slot,path.join(root,'runtime')),/integrity/);
});

test('export rejects an old or changed same-version launcher before executing it', {skip:process.platform==='linux'}, async t => {
 const root=temp(t), kit=path.join(root,'kit'), host=path.join(root,'host');
 const resources=path.join(host,process.platform==='darwin'?'Nexus Launcher.app/Contents/Resources':'resources');
 fs.mkdirSync(kit); fs.mkdirSync(resources,{recursive:true});
 fs.writeFileSync(path.join(kit,'manifest.json'),JSON.stringify({schema:3,electronVersion:'44.0.0'}));
 await assert.rejects(desktopHostForExport(kit,{desktop_host:host}),/ENOENT/);
 fs.writeFileSync(path.join(resources,'app.asar'),'changed');
 fs.writeFileSync(path.join(resources,'nexus-electron-host.json'),JSON.stringify({schema:1,entry:'nexus-official-desktop',electronVersion:'44.0.0',appAsarSha256:'wrong'}));
 await assert.rejects(desktopHostForExport(kit,{desktop_host:host}),/verified Nexus host/);
});
