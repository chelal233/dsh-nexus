import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { signMacApplication } from '../desktop/scripts/sign-macos.mjs';
import { inventoryResources, releaseIdentity, verifyIdentity, verifyInventory } from '../desktop/scripts/prepare-release.mjs';

test('macOS smoke builds select ad-hoc signing while release builds require a certificate', () => {
  for (const smoke of ['1', '0']) {
    const result = JSON.parse(execFileSync(process.execPath, ['-e', "const c=require('./electron-builder.cjs'); console.log(JSON.stringify({identity:c.mac.identity??null,notarize:c.mac.notarize}))"], {
      cwd: new URL('..', import.meta.url), env: { ...process.env, NEXUS_UNSIGNED_SMOKE: smoke }, encoding: 'utf8', windowsHide: true,
    }));
    assert.deepEqual(result, { identity: smoke === '1' ? '-' : null, notarize: smoke !== '1' });
  }
});

test('macOS signing refreshes resource hashes before sealing the app and preserves signing policy', async t => {
  const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-signing-'));
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  const app=path.join(root,'Nexus Launcher.app'),resources=path.join(app,'Contents/Resources');
  fs.mkdirSync(path.join(resources,'runtime/node'),{recursive:true});fs.mkdirSync(path.join(resources,'notices'));
  const names=['nexus-agent','nexus-launcher','nexusctl','nexus-desktop-bridge','runtime','notices'];
  for(const name of names.slice(0,4))fs.writeFileSync(path.join(resources,name),'unsigned binary');
  const node=path.join(resources,'runtime/node/node');fs.writeFileSync(node,'unsigned node');
  const runtime={node:{sha256:'original',version:'24.20.0',npmVersion:'11.19.0'},pnpm:{version:'11.7.0'},target:'aarch64-apple-darwin'};
  fs.writeFileSync(path.join(resources,'runtime/manifest.json'),JSON.stringify(runtime));
  const manifest={schemaVersion:1,version:'fixture',buildId:'fixture',createdAt:'fixture',commit:'fixture',dirty:false,node:'fixture',npm:'fixture',pnpm:'fixture',runtime,files:await inventoryResources(resources,names)};
  const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
  fs.writeFileSync(path.join(resources,'release-manifest.json'),JSON.stringify(manifest));
  fs.writeFileSync(path.join(resources,'release-identity.json'),JSON.stringify(releaseIdentity(manifest,sha(JSON.stringify(manifest)))));
  const calls=[];
  await signMacApplication({app,identity:'-',keychain:'fixture.keychain'}, {
    sign:async()=>{calls.push('inner');fs.writeFileSync(node,'signed node');},
    codesign:(program,args)=>{
      calls.push(args.includes('--verify')?'verify':'seal');
      assert.equal(program,'/usr/bin/codesign');assert.equal(args.at(-1),app);
      if(args.includes('--sign')){assert.ok(args.includes('--preserve-metadata=identifier,entitlements,requirements,flags,runtime'));assert.ok(!args.includes('--deep'));assert.ok(args.includes('fixture.keychain'));}
      const refreshed=JSON.parse(fs.readFileSync(path.join(resources,'release-manifest.json')));
      assert.equal(refreshed.runtime.node.sha256,sha('signed node'));
    },
  });
  assert.deepEqual(calls,['inner','seal','verify']);
  const bytes=fs.readFileSync(path.join(resources,'release-manifest.json')), refreshed=JSON.parse(bytes);
  verifyIdentity(refreshed,JSON.parse(fs.readFileSync(path.join(resources,'release-identity.json'))),sha(bytes));
  await verifyInventory(resources,refreshed.files);
  fs.writeFileSync(node,'unexpected edit');
  await assert.rejects(signMacApplication({app,identity:'-'},{sign:()=>{throw Error('must not sign changed input')}}),/resource changed/i);
});
