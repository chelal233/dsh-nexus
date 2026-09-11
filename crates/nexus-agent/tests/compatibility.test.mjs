import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { check, checkCanary, canarySearch, planCanary, sourceInfo, incompatibleBundles } from '../src/compatibility.mjs';

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-compat-test-'));
  const home = path.join(root, 'home'), slot = path.join(root, 'slot');
  const source = path.join(home, 'profiles/original');
  fs.mkdirSync(source, {recursive:true});
  fs.mkdirSync(path.join(slot, 'vendor/settings'), {recursive:true});
  fs.writeFileSync(path.join(slot, 'vendor/settings/package.json'), JSON.stringify({name:'@deepseek-ai/settings',version:'1'}));
  fs.mkdirSync(path.join(slot, 'apps/cli/lib'), {recursive:true});
  fs.writeFileSync(path.join(slot, 'apps/cli/lib/bin.js'), `
    const fs=require('node:fs'),path=require('node:path'),http=require('node:http');
    const m=JSON.parse(fs.readFileSync(path.join(process.env.DSH_HOME,'profiles',process.argv[3],'package.json')));
    const bundles=m.dsh.profile.bundles;
    if(bundles.includes('unknown')) {console.error('Unattributable initialization failure');process.exit(1);}
    if(bundles.includes('timeout')) {setInterval(()=>{},1000);}
    else if(bundles.includes('bad') && !fs.existsSync(path.join(__dirname,'supported'))) {
      console.error('failed to apply loader entry child (bad): ctx.missing is not a function');process.exit(1);
    } else {
      if(bundles.includes('soft-bad')) console.error('failed to apply loader entry child (soft-bad): ctx.missing is not a function');
      const server=http.createServer((req,res)=>{
        if(req.url.includes('token=')) {res.writeHead(302,{'set-cookie':'session=test; HttpOnly','location':'/'});res.end();}
        else if(req.headers.cookie==='session=test') res.end('<HTML>ready</HTML>');
        else {res.writeHead(401);res.end('authentication required');}
      });
      server.listen(0,'127.0.0.1',()=>console.log('dsh web: http://127.0.0.1:'+server.address().port+'/?token=test'));}
  `);
  const write = bundles => fs.writeFileSync(path.join(source,'package.json'), JSON.stringify({name:'original',dependencies:{bad:'1'},dsh:{profile:{bundles}}}));
  const options = {home,slot,selected:'original',release_id:'one',node:process.execPath,work:path.join(home,'profiles/.work'),output:path.join(root,'report.json'),timeout_ms:3000};
  fs.mkdirSync(options.work,{recursive:true});
  return {root,home,slot,source,options,write,close:()=>fs.rmSync(root,{recursive:true,force:true})};
}

test('Windows canonical external slot works in compatibility and Canary', {skip:process.platform !== 'win32'}, async()=>{
  const f=fixture();
  try {
    f.write(['good']);
    const options={...f.options,slot:path.toNamespacedPath(f.slot)};
    assert.equal((await check(options)).status, 'passed');
    assert.ok(planCanary(options));
    const report = await checkCanary({...options, mode:'diagnostic_only'});
    assert.equal(report.outcome, 'passed');
    assert.ok(report.rounds.length > 0);
  } finally { f.close(); }
});

test('preference and patch changes invalidate compatibility cache', async()=>{
  const f=fixture();
  try {
    f.write(['good']);
    const patch=path.join(f.root,'extra.yml');
    fs.writeFileSync(patch,'[]');
    const first=await check({...f.options, patches:[patch], preferences_env:{DSH_TOOLS_MODE:'native'}});
    const changed=await check({...f.options, patches:[patch], preferences_env:{DSH_TOOLS_MODE:'both'}});
    assert.notEqual(first.effective_profile, changed.effective_profile);
    fs.writeFileSync(patch,'# changed\n[]');
    const patched=await check({...f.options, patches:[patch], preferences_env:{DSH_TOOLS_MODE:'both'}});
    assert.notEqual(changed.effective_profile, patched.effective_profile);
    const reused=await check({...f.options, patches:[patch], preferences_env:{DSH_TOOLS_MODE:'both'}});
    assert.equal(reused.effective_profile, patched.effective_profile);
    assert.equal(reused.cache_reused, true);
  } finally {f.close();}
});

test('only specific third-party import/API failures are attributable',()=>{
  assert.deepEqual(incompatibleBundles('failed to apply loader entry include (cordis:include): failed to apply loader entry child (bad): ctx.missing is not a function',['bad']).map(x=>x.package),['bad']);
  assert.deepEqual(incompatibleBundles('failed to apply loader entry child (@deepseek-ai/core): ctx.missing is not a function',['@deepseek-ai/core']),[]);
  assert.deepEqual(incompatibleBundles('failed to apply loader entry child (bad): API key missing',['bad']),[]);
});

test('enabled patches prevent automatic plugin removal after a failed probe', async()=>{
  const f=fixture();
  try {
    f.write(['bad']); const patch=path.join(f.root,'extra.yml'); fs.writeFileSync(patch,'[]');
    await assert.rejects(check({...f.options, patches:[patch]}), /Enabled patch combination failed/);
    const report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.status,'needs_choice'); assert.deepEqual(report.disabled,[]);
    assert.deepEqual(JSON.parse(fs.readFileSync(path.join(f.source,'package.json'))).dsh.profile.bundles,['bad']);
  } finally { f.close(); }
});

test('isolates in a copy, reuses checked cache, and restores plugins on another release',async()=>{
  const f=fixture();
  try {
    f.write(['bad','good']); const before=fs.readFileSync(path.join(f.source,'package.json'),'utf8');
    const result=await check(f.options);
    assert.equal(result.status,'isolated'); assert.deepEqual(result.disabled.map(x=>x.package),['bad']);
    assert.equal(fs.readFileSync(path.join(f.source,'package.json'),'utf8'),before);
    assert.equal(sourceInfo(f.home,result.effective_profile).source,'original');
    const cached=await check(f.options); assert.equal(cached.effective_profile,result.effective_profile);
    fs.writeFileSync(path.join(f.slot,'apps/cli/lib/supported'),'yes');
    const next=await check({...f.options,selected:result.effective_profile,release_id:'two'});
    assert.equal(next.status,'passed'); assert.deepEqual(next.disabled,[]);
    const manifest=JSON.parse(fs.readFileSync(path.join(f.home,'profiles',next.effective_profile,'package.json')));
    assert.ok(manifest.dsh.profile.bundles.includes('bad'));
    assert.notEqual(next.effective_profile,result.effective_profile);
    assert.deepEqual(fs.readdirSync(f.options.work),[]);
  } finally { f.close(); }
});

test('cache reports preserve actual check provenance without rewriting its marker',async()=>{
  const f=fixture();
  try {
    f.write(['good']);
    const checked=await check({...f.options,force:true,trigger:'version_switch'});
    assert.equal(checked.trigger,'version_switch');
    assert.equal(checked.cache_reused,false);
    const marker=path.join(f.home,'profiles',checked.effective_profile,'.nexus-compatibility.json');
    const markerBefore=fs.readFileSync(marker,'utf8');
    const reused=await check({...f.options,trigger:'startup'});
    assert.equal(reused.cache_reused,true);
    assert.equal(reused.checked_at_unix,checked.checked_at_unix);
    assert.equal(reused.trigger,'version_switch');
    assert.equal(reused.last_trigger,'startup');
    assert.ok(reused.last_used_at_unix>=checked.checked_at_unix);
    assert.equal(fs.readFileSync(marker,'utf8'),markerBefore);
    const legacy=JSON.parse(markerBefore);
    delete legacy.trigger;
    delete legacy.last_trigger;
    delete legacy.last_used_at_unix;
    delete legacy.cache_reused;
    fs.writeFileSync(marker,JSON.stringify(legacy));
    const legacyBefore=fs.readFileSync(marker,'utf8');
    const unknown=await check({...f.options,trigger:'startup'});
    assert.equal(unknown.trigger,null);
    assert.equal(unknown.last_trigger,'startup');
    assert.equal(unknown.cache_reused,true);
    assert.equal(unknown.checked_at_unix,checked.checked_at_unix);
    assert.equal(fs.readFileSync(marker,'utf8'),legacyBefore);
    const selected=await check({...f.options,force:true,trigger:'profile_switch'});
    assert.equal(selected.trigger,'profile_switch');
    assert.equal(selected.cache_reused,false);
  } finally { f.close(); }
});

test('unknown errors fail closed without publishing a profile',async()=>{
  const f=fixture();
  try {
    f.write(['unknown']);
    await assert.rejects(check({...f.options,trigger:'version_switch'}),/user decision/);
    const report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.status,'needs_choice');
    assert.equal(report.trigger,'version_switch');
    assert.equal(report.last_trigger,'version_switch');
    assert.equal(report.cache_reused,false);
    assert.deepEqual(report.candidates.map(x=>x.package),['unknown']);
    assert.deepEqual(fs.readdirSync(f.home+'/profiles').filter(x=>x.startsWith('nexus-')),[]);
    assert.deepEqual(fs.readdirSync(f.options.work),[]);
  } finally {f.close();}
});

test('timeout fails closed and cleans the owned probe',async()=>{
  const f=fixture();
  try {
    f.write(['timeout']);
    await assert.rejects(check({...f.options,timeout_ms:300}),/timed out/);
    assert.equal(JSON.parse(fs.readFileSync(f.options.output)).status,'needs_choice');
    assert.deepEqual(fs.readdirSync(f.options.work),[]);
  } finally {f.close();}
});

test('a reachable web page cannot conceal a loader initialization failure',async()=>{
  const f=fixture();
  try {
    f.write(['soft-bad','good']);
    const result=await check(f.options);
    assert.equal(result.status,'isolated');
    assert.deepEqual(result.disabled.map(x=>x.package),['soft-bad']);
  } finally {f.close();}
});

test('edited effective profiles are rechecked and retained; changed home settings invalidate cache',async()=>{
  const f=fixture();
  try {
    f.write(['good']);
    const first=await check(f.options);
    const effective=path.join(f.home,'profiles',first.effective_profile);
    const manifest=JSON.parse(fs.readFileSync(path.join(effective,'package.json')));
    manifest.dsh.profile.bundles.push('unknown');
    fs.writeFileSync(path.join(effective,'package.json'),JSON.stringify(manifest));
    await assert.rejects(check(f.options),/user decision/);
    assert.ok(JSON.parse(fs.readFileSync(path.join(effective,'package.json'))).dsh.profile.bundles.includes('unknown'));
    manifest.dsh.profile.bundles= ['good','bad'];
    fs.writeFileSync(path.join(effective,'package.json'),JSON.stringify(manifest));
    const checked=await check(f.options);
    assert.deepEqual(checked.disabled.map(x=>x.package),['bad']);
    assert.ok(fs.readdirSync(effective).some(x=>x.startsWith('package.nexus-backup-')));
    fs.writeFileSync(path.join(f.home,'settings.yaml'),'test: true');
    const next=await check(f.options);
    assert.notEqual(next.effective_profile,first.effective_profile);
    assert.equal(fs.readFileSync(path.join(f.home,'settings.yaml'),'utf8'),'test: true');
  } finally {f.close();}
});

test('user can isolate an unclassified failure and restore it without changing the source',async()=>{
  const f=fixture();
  try {
    f.write(['unknown','good']);
    const original=fs.readFileSync(path.join(f.source,'package.json'),'utf8');
    await assert.rejects(check(f.options),/user decision/);
    const dir=path.join(f.home,'profiles/.nexus-plugin-isolation');
    fs.mkdirSync(dir);
    fs.writeFileSync(path.join(dir,'original.json'),JSON.stringify(['unknown']));
    const isolated=await check(f.options);
    assert.equal(isolated.status,'isolated');
    assert.deepEqual(isolated.disabled,[{package:'unknown',reason:'Disabled by user'}]);
    assert.equal(fs.readFileSync(path.join(f.source,'package.json'),'utf8'),original);
    fs.writeFileSync(path.join(dir,'original.json'),'[]');
    await assert.rejects(check(f.options),/user decision/);
    assert.equal(fs.readFileSync(path.join(f.source,'package.json'),'utf8'),original);
  } finally {f.close();}
});


test('Canary narrows an interaction without mutating candidates and rejects failed baseline', async () => {
  const candidates = ['good', 'a', 'b'];
  const report = await canarySearch(candidates, async enabled => ({outcome: enabled.includes('a') && enabled.includes('b') ? 'failed' : 'passed'}));
  assert.deepEqual(report.suspect_combination, ['a', 'b']);
  assert.deepEqual(candidates, ['good', 'a', 'b']);
  const baseline = await canarySearch(candidates, async () => ({outcome:'failed'}));
  assert.equal(baseline.outcome, 'inconclusive');
  const budget = await canarySearch(candidates, async enabled => ({outcome: enabled.length ? 'failed' : 'passed'}), 'bisect', 2);
  assert.equal(budget.outcome, 'inconclusive');
  assert.equal(budget.rounds.length, 2);
});

test('Canary real child report preserves source and original error without production latest', async () => {
  const f = fixture();
  try {
    f.write(['bad']);
    const original = fs.readFileSync(path.join(f.source, 'package.json'));
    const report = await checkCanary({...f.options, mode:'diagnostic_only'});
    assert.equal(report.outcome, 'failed');
    assert.match(report.rounds[0].raw_error, /ctx.missing/);
    assert.equal(report.checks.feature, 'unsupported');
    assert.deepEqual(fs.readFileSync(path.join(f.source, 'package.json')), original);
    assert.equal(fs.existsSync(path.join(f.home, 'compatibility', 'latest.json')), false);
    assert.deepEqual(fs.readdirSync(f.options.work), []);
  } finally {f.close();}
});


test('owned compatibility defers publication until quiescent finalize and keeps failed scratch', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const result = await check({...f.options, owned_round:true});
    assert.equal(fs.existsSync(path.join(f.home,'profiles',result.effective_profile)), false);
    assert.equal(fs.existsSync(path.join(f.options.work,'publication.json')), true);
    await check({...f.options, owned_round:true, finalize:true});
    assert.equal(fs.existsSync(path.join(f.home,'profiles',result.effective_profile)), true);
    f.write(['bad']);
    await assert.rejects(check({...f.options, owned_round:true}), /ctx.missing/);
    assert.ok(fs.readdirSync(f.options.work).length > 0);
  } finally { f.close(); }
});

test('owned Canary performs exactly one subset and leaves scratch for the outer job owner', async () => {
  const f = fixture();
  try {
    f.write(['good','bad']);
    const report = await checkCanary({...f.options,mode:'diagnostic_only',owned_round:true,subset:[]});
    assert.equal(report.rounds.length,1);
    assert.equal(report.outcome,'passed');
    assert.deepEqual(report.all_candidates,['good','bad']);
    assert.ok(fs.readdirSync(f.options.work).length > 0);
  } finally { f.close(); }
});


test('Canary copy plan shares exclusions and does not copy or execute plugins', () => {
  const f=fixture();
  try {
    f.write(['good']);
    const modules=path.join(f.source,'node_modules');
    fs.mkdirSync(path.join(modules,'good'),{recursive:true});
    fs.writeFileSync(path.join(modules,'good','payload'),Buffer.alloc(100));
    fs.mkdirSync(path.join(modules,'.pnpm'),{recursive:true});
    fs.writeFileSync(path.join(modules,'.pnpm','ignored'),Buffer.alloc(1024*1024));
    const plan=planCanary({...f.options,canary_plan:true});
    assert.ok(plan.copy_bytes<100000);
    assert.ok(plan.required_bytes>=64*1024*1024+plan.copy_bytes);
    assert.deepEqual(fs.readdirSync(f.options.work),[]);
    assert.deepEqual(fs.readdirSync(path.join(f.home,'profiles')),['.work','original']);
    const outside=path.join(f.root,'outside');fs.mkdirSync(outside);
    fs.symlinkSync(outside,path.join(modules,'escape'),process.platform==='win32'?'junction':'dir');
    assert.throws(()=>planCanary(f.options),/escapes/);
  } finally {f.close();}
});
