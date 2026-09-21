import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {HarnessDesktop,desktopActive} from '../electron/harness-desktop.mjs';
import {stopDesktopChild} from '../electron/desktop-process.mjs';
const temp=t=>{const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-stop-test-'));t.after(()=>fs.rmSync(root,{recursive:true,force:true}));return root;};
test('Desktop start is protected before first await and can be cancelled before spawning',async t=>{
 const root=temp(t);let release;
 const bridge={request(command){if(command==='desktop_launch_context')return new Promise(resolve=>release=resolve);return Promise.resolve({});}};
 const desktop=new HarnessDesktop({bridge,userData:root,resources:root});
 const start=desktop.start();
 assert.equal(desktopActive(desktop.status()),true);
 const stop=desktop.stop(); release({available:true,data_root:root});
 await assert.rejects(start,/desktop_start_cancelled/);
 assert.equal((await stop).phase,'idle');
});
test('Desktop stop requests the exact operation and waits for worker acknowledgement',async t=>{
 const root=temp(t), operationId=randomUUID();
 const desktop=new HarnessDesktop({bridge:{},userData:root,resources:root});
 fs.mkdirSync(desktop.directory,{recursive:true});
 const state={phase:'launched',pid:process.pid,operationId,stopError:'previous failure',stopRequestId:randomUUID()};fs.writeFileSync(desktop.file,JSON.stringify(state));
 const timer=setInterval(()=>{const request=path.join(desktop.directory,`stop-${operationId}.json`);if(fs.existsSync(request)){fs.writeFileSync(desktop.file,JSON.stringify({...state,phase:'stopped'}));clearInterval(timer);}},250);
 t.after(()=>clearInterval(timer));
 const first=desktop.stop(); assert.equal(desktop.stop(),first,'concurrent stops share acknowledgement');
 assert.equal((await first).phase,'stopped');
});
test('owned process-tree termination removes an isolated child and its descendant',{timeout:15000},async t=>{
 const child=spawn(process.execPath,['-e',`const {spawn}=require('node:child_process');const sub=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});console.log(sub.pid);setInterval(()=>{},1000);`],{detached:process.platform!=='win32',windowsHide:true,stdio:['ignore','pipe','pipe']});
 t.after(()=>stopDesktopChild(child));
 const [chunk]=await once(child.stdout,'data');const descendant=Number(chunk.toString().trim());assert.ok(descendant>0);
 const exited=once(child,'exit');await stopDesktopChild(child);await exited;
 let alive=true;
 for(let i=0;i<30;i++){try{process.kill(descendant,0);}catch{alive=false;break;}await new Promise(r=>setTimeout(r,100));}
 assert.equal(alive,false,'descendant must exit before stop is considered complete');
});

test('a current stop failure is returned while the instance remains active',async t=>{
 const root=temp(t), operationId=randomUUID();
 const desktop=new HarnessDesktop({bridge:{},userData:root,resources:root});
 fs.mkdirSync(desktop.directory,{recursive:true});
 const state={phase:'launched',pid:process.pid,operationId};fs.writeFileSync(desktop.file,JSON.stringify(state));
 const timer=setInterval(()=>{const request=path.join(desktop.directory,`stop-${operationId}.json`);if(fs.existsSync(request)){
  const {requestId}=JSON.parse(fs.readFileSync(request,'utf8'));
  fs.writeFileSync(desktop.file,JSON.stringify({...state,stopRequestId:requestId,stopError:'fixture stop rejected'}));clearInterval(timer);
 }},20);
 t.after(()=>clearInterval(timer));
 await assert.rejects(desktop.stop(),/fixture stop rejected/);
 assert.equal(desktopActive(desktop.status()),true);
});

test('legacy instances are never terminated using their persisted PID',async t=>{
 const root=temp(t),desktop=new HarnessDesktop({bridge:{},userData:root,resources:root});
 fs.mkdirSync(desktop.directory,{recursive:true});
 fs.writeFileSync(desktop.file,JSON.stringify({phase:'launched',pid:process.pid,childPid:process.pid}));
 await assert.rejects(desktop.stop(),/desktop_stop_unsupported/);
 assert.equal(desktop.status().phase,'launched');
});

test('Unix stop waits for a TERM-resistant descendant after the leader exits', {skip:process.platform==='win32',timeout:15000}, async t=>{
 const source=`const {spawn}=require('node:child_process');const sub=spawn(process.execPath,['-e',"process.on('SIGTERM',()=>{});console.log('ready');setInterval(()=>{},1000)"],{stdio:['ignore','pipe','ignore']});sub.stdout.once('data',()=>console.log(sub.pid));process.on('SIGTERM',()=>process.exit(0));setInterval(()=>{},1000);`;
 const child=spawn(process.execPath,['-e',source],{detached:true,stdio:['ignore','pipe','pipe']});
 t.after(()=>stopDesktopChild(child));
 const [chunk]=await once(child.stdout,'data'); const descendant=Number(chunk.toString().trim());
 await stopDesktopChild(child);
 assert.throws(()=>process.kill(descendant,0),{code:'ESRCH'});
});


test('Desktop startup failure remains readable after the worker drops its child PID', async t => {
 const {readDesktopState}=await import('../electron/harness-desktop.mjs');
 const root=temp(t),operationId=randomUUID(),file=path.join(root,'state.json');
 fs.writeFileSync(path.join(root,`startup-${operationId}.json`),JSON.stringify({operationId,pid:123,state:'failed',error:'credentials failed to import'}));
 fs.writeFileSync(file,JSON.stringify({phase:'launched',operationId,pid:process.pid,childPid:123}));
 assert.equal(readDesktopState(file,()=>true).audit.error,'credentials failed to import');
 fs.writeFileSync(file,JSON.stringify({phase:'failed',operationId,pid:process.pid}));
 assert.equal(readDesktopState(file,()=>false).audit.state,'failed');
 fs.writeFileSync(file,JSON.stringify({phase:'launched',operationId,pid:process.pid,childPid:456}));
 assert.notEqual(readDesktopState(file,()=>true).audit.state,'failed');
});


test('official failure file is captured without structured audit and preserves the first cause', async t => {
 const {readDesktopState}=await import('../electron/harness-desktop.mjs');
 const root=temp(t),operationId=randomUUID(),file=path.join(root,'state.json');
 fs.writeFileSync(file,JSON.stringify({phase:'launched',operationId,pid:process.pid,childPid:123}));
 const evidence=path.join(root,`startup-${operationId}.json`);
 fs.writeFileSync(`${evidence}.error`,'credentials import failed ?token=secret\n'+'waiting consumer\n'.repeat(500));
 for(const corrupt of [false,true]) {
   if(corrupt)fs.writeFileSync(evidence,'invalid json');
   const result=readDesktopState(file,()=>true);
   assert.equal(result.audit.state,'failed');
   assert.ok(result.audit.error.startsWith('credentials import failed'));
   assert.ok(!result.audit.error.includes('secret'));
   assert.ok(result.audit.error.length<=6010);
 }
});
