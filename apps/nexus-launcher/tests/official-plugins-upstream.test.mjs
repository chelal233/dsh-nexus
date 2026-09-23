import fs from 'node:fs';import path from 'node:path';import assert from 'node:assert/strict';
import {manage} from '../../../crates/nexus-agent/src/official-plugins.mjs';
import test from 'node:test';import os from 'node:os';
test('official manager changes only the stopped profile and honors upstream protection',{skip:!process.env.NEXUS_OFFICIAL_HARNESS_ROOT},async()=>{
const root=process.env.NEXUS_OFFICIAL_HARNESS_ROOT;
const home=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-official-manager-'));
const dir=path.join(home,'profiles','web');fs.mkdirSync(dir,{recursive:true});
for(const [name,module] of [['fixture-bundle','fixture-throws'],['fixture-protected','@deepseek-ai/dsh-plugin-manager']]) {
 const pkg=path.join(dir,'node_modules',name);fs.mkdirSync(pkg,{recursive:true});
 fs.writeFileSync(path.join(pkg,'package.json'),JSON.stringify({name,version:'1.0.0',type:'module',exports:{'./package.json':'./package.json'},dsh:{bundle:{patch:'bundle.yml'}}}));
 fs.writeFileSync(path.join(pkg,'bundle.yml'),`- insert:\n    - id: ${name}\n      name: ${JSON.stringify(module)}\n`);
}
fs.writeFileSync(path.join(dir,'package.json'),JSON.stringify({name:'fixture-profile',dependencies:{'fixture-bundle':'1.0.0','fixture-protected':'1.0.0'},dsh:{profile:{bundles:['fixture-bundle','fixture-protected']}}}));
const other=path.join(home,'profiles','desktop');fs.mkdirSync(other,{recursive:true});
const original=JSON.stringify({name:'untouched-desktop',dsh:{profile:{bundles:[]}}});fs.writeFileSync(path.join(other,'package.json'),original);
const input={root,home,profile:'web',work:path.join(home,'worker')};
const list=await manage({...input,action:'list'});console.log(JSON.stringify(list.bundles.filter(v=>v.name.startsWith('fixture'))));
assert.equal(list.bundles.find(v=>v.name==='fixture-protected').readOnlyReason,'management-required');
const disabled=await manage({...input,action:'disable',package:'fixture-bundle'});assert.equal(disabled.result.application,'restart-required');assert.equal(disabled.bundles.find(v=>v.name==='fixture-bundle').enabled,false);
assert.equal(JSON.parse(fs.readFileSync(path.join(dir,'package.json'))).dependencies['fixture-bundle'],'1.0.0');
const denied=await manage({...input,action:'disable',package:'fixture-protected'});assert.equal(denied.result.error.code,'management-required');
const enabled=await manage({...input,action:'enable',package:'fixture-bundle'});assert.equal(enabled.bundles.find(v=>v.name==='fixture-bundle').enabled,true);
assert.equal(fs.readFileSync(path.join(other,'package.json'),'utf8'),original);
console.log('PASS: official list, disable, enable, dependency retention, management protection; no profile plugins booted.');

});
