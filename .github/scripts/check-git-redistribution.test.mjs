import test from 'node:test';import assert from 'node:assert/strict';
import {cleared} from './check-git-redistribution.mjs';
import {gitDistribution} from '../../apps/nexus-launcher/desktop/scripts/bundled-git.mjs';
const ready=()=>[['win32','x64'],['win32','arm64'],['darwin','x64'],['darwin','arm64'],['linux','arm64']].map(([platform,arch])=>({...gitDistribution({platform,arch}),publicRedistributionReady:true}));
test('public package gate requires exact current inventories and every clearance',()=>{
 assert.equal(cleared(ready()),true);
 for(const mutate of [x=>x.pop(),x=>x.push(x[0]),x=>x[0].sha256='old',x=>x[0].publicRedistributionReady=false]){const rows=ready();mutate(rows);assert.equal(cleared(rows),false);}
 assert.equal(cleared(null),false);
});
