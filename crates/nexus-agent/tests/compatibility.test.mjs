import test from 'node:test';
import { replacedOfficialEntries, activationRepairCandidates, verificationIdentity } from '../src/compatibility.mjs';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { diagnoseStartup, parseActivation, duplicateEntrySources, dependencyOrigins, check, checkCanary, canarySearch, planCanary, sourceInfo, incompatibleBundles } from '../src/compatibility.mjs';

test('parallel inventory stays stable, ignores only generated Desktop assets, and detects changed files and link targets', async t => {
  const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-inventory-'));
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  const desktop=path.join(root,'apps/desktop/.desktop-build');
  fs.mkdirSync(desktop,{recursive:true});
  for(const name of ['a','b']) { fs.mkdirSync(path.join(root,name)); fs.writeFileSync(path.join(root,name,'plugin.js'),name); }
  const link=path.join(root,'linked'); fs.symlinkSync(path.join(root,'a'),link,process.platform==='win32'?'junction':'dir');
  const identity=()=>verificationIdentity([root],{},desktop);
  const original=await identity(); assert.ok(original);
  assert.equal(await identity(),original);
  fs.writeFileSync(path.join(desktop,'generated.py'),'irrelevant to Web');
  assert.equal(await identity(),original);
  const file=path.join(root,'a/plugin.js'), before=fs.statSync(file);
  fs.writeFileSync(file,'c'); fs.utimesSync(file,before.atime,before.mtime);
  const edited=await identity(); assert.notEqual(edited,original);
  fs.unlinkSync(link); fs.symlinkSync(path.join(root,'b'),link,process.platform==='win32'?'junction':'dir');
  assert.notEqual(await identity(),edited);
});

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
  const options = {home,slot,selected:'original',release_id:'one',node:process.execPath,work:path.join(root,'work'),cache:path.join(root,'verified.json'),output:path.join(root,'report.json'),timeout_ms:3000};
  fs.mkdirSync(options.work,{recursive:true});
  return {root,home,slot,source,options,write,close:()=>fs.rmSync(root,{recursive:true,force:true})};
}

test('path aliases preserve generated-asset exclusion, fallback identity and official declaration containment', async t => {
  const f = fixture(), alias = f.root + '-alias';
  t.after(() => { fs.unlinkSync(alias); f.close(); });
  fs.symlinkSync(f.root, alias, process.platform === 'win32' ? 'junction' : 'dir');
  const aliased = file => path.join(alias, path.relative(f.root, file));
  const desktop = path.join(f.slot, 'apps/desktop/.desktop-build');
  fs.mkdirSync(desktop, {recursive:true});
  const identity = () => verificationIdentity([aliased(f.slot)], {}, aliased(desktop));
  const before = await identity(); assert.ok(before);
  fs.writeFileSync(path.join(desktop, 'generated.py'), 'not a Web input');
  assert.equal(await identity(), before);

  f.write(['@deepseek-ai/settings', 'uploader']);
  for (const [name, dir] of [['@deepseek-ai/settings',path.join(f.slot,'vendor/settings')],['uploader',path.join(f.source,'node_modules/uploader')]]) {
    fs.mkdirSync(dir,{recursive:true});
    fs.writeFileSync(path.join(dir,'package.json'),JSON.stringify({name,dsh:{bundle:{patch:'patch.yml'}}}));
    fs.writeFileSync(path.join(dir,'patch.yml'), `- insert:\n    - id: upload\n      name: ${name}\n`);
  }
  const source = sourceInfo(f.home, 'original');
  assert.equal(duplicateEntrySources(source, aliased(f.slot), 'duplicate loader entry id: upload').length, 2);
  assert.equal(replacedOfficialEntries(source, aliased(f.slot)).length, 1);
  const link = path.join(f.source, 'node_modules/missing');
  fs.symlinkSync(aliased(path.join(f.source, '.dsh-module-fallback/node_modules/missing')), link, process.platform === 'win32' ? 'junction' : 'dir');
  assert.equal((await check(f.options)).status, 'passed');
  fs.unlinkSync(link);
  fs.symlinkSync(aliased(path.join(f.source, 'outside-fallback/missing')), link, process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(check({...f.options, force:true}), /Installed dependency link is broken/);
  fs.unlinkSync(link);
  const outside = path.join(f.root, 'outside'); fs.mkdirSync(outside);
  fs.writeFileSync(path.join(outside, 'package.json'), JSON.stringify({name:'missing'}));
  fs.symlinkSync(aliased(outside), link, process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(check({...f.options, force:true}), /Dependency link escapes source node_modules/);
});

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

test('declaration mismatch is advisory and target manifest changes invalidate cached reports', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    fs.mkdirSync(path.join(f.source, 'node_modules/good'), { recursive: true });
    const plugin = path.join(f.source, 'node_modules/good/package.json');
    fs.writeFileSync(plugin, JSON.stringify({ name: 'good', version: '1.0.0', engines: { dsh: '>=2.0.0' } }));
    const original = fs.readFileSync(plugin, 'utf8');
    const host = path.join(f.slot, 'apps/cli/package.json');
    fs.writeFileSync(host, JSON.stringify({ version: '1.0.0' }));
    // Recover transparently from an oversized result written by an old checker.
    fs.writeFileSync(f.options.cache, JSON.stringify({ padding: 'x'.repeat(70000) }));
    const first = await check(f.options);
    assert.equal(first.status, 'passed');
    assert.ok(fs.statSync(f.options.cache).size < 65536);
    assert.equal(first.declarations[0].status, 'mismatch');
    assert.equal((await check(f.options)).cache_reused, true);
    fs.writeFileSync(host, JSON.stringify({ version: '2.0.0' }));
    const changed = await check(f.options);
    assert.equal(changed.cache_reused, false);
    assert.equal(changed.declarations[0].status, 'match');
    assert.equal(fs.readFileSync(plugin, 'utf8'), original);
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
    assert.equal(changed.effective_profile, 'original'); assert.equal(changed.cache_reused, false);
    fs.writeFileSync(patch,'# changed\n[]');
    const patched=await check({...f.options, patches:[patch], preferences_env:{DSH_TOOLS_MODE:'both'}});
    assert.equal(patched.effective_profile, 'original'); assert.equal(patched.cache_reused, false);
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

test('Electron-only profile rejection is not reported as faulty plugins', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const manifest = fs.readFileSync(path.join(f.source, 'package.json'));
    fs.writeFileSync(path.join(f.slot, 'apps/cli/lib/bin.js'), `console.error('error: profile "desktop" is managed exclusively by the Electron application'); process.exit(1);`);
    await assert.rejects(check(f.options), /Select a regular profile such as "web"/);
    assert.equal(JSON.parse(fs.readFileSync(f.options.output)).status, "failed");
    assert.deepEqual(JSON.parse(fs.readFileSync(f.options.output)).candidates, []);
    assert.deepEqual(fs.readFileSync(path.join(f.source, 'package.json')), manifest);
  } finally { f.close(); }
});

test('obsolete official fallback junctions do not prevent compatibility probing', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const scope = path.join(f.source, 'node_modules/@deepseek-ai');
    fs.mkdirSync(scope, {recursive:true});
    const link = path.join(scope, 'dsh-client-runtime');
    fs.symlinkSync(path.join(f.source, '.dsh-module-fallback/node_modules/missing'), link, process.platform === 'win32' ? 'junction' : 'dir');
    assert.equal((await check(f.options)).status, 'passed');
    assert.equal(fs.lstatSync(link).isSymbolicLink(), true);
  } finally { f.close(); }
});

test('third-party generated fallback links are omitted but ordinary broken installations fail clearly', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const scope = path.join(f.source, 'node_modules/@noble');
    fs.mkdirSync(scope, {recursive:true});
    const link = path.join(scope, 'hashes');
    fs.symlinkSync(path.join(f.source, '.dsh-module-fallback/node_modules/@noble/hashes'), link, process.platform === 'win32' ? 'junction' : 'dir');
    const passed = await check(f.options);
    assert.equal(passed.status, 'passed');
    assert.deepEqual(passed.dependency_origins, []);
    assert.ok(fs.lstatSync(link).isSymbolicLink());
    fs.unlinkSync(link);
    fs.symlinkSync(path.join(f.source, 'missing-install'), link, process.platform === 'win32' ? 'junction' : 'dir');
    await assert.rejects(check({...f.options, force:true}), /Installed dependency link is broken/);
    const report = JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.status, 'failed');
    assert.equal(report.failure_stage, "dependency_preparation");
    assert.deepEqual(report.candidates, []);
  } finally { f.close(); }
});

test('desktop name is rejected early only when the selected upstream declares it reserved', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    fs.renameSync(f.source, path.join(f.home, 'profiles/desktop'));
    const options = {...f.options, selected:'desktop'};
    const args = path.join(f.slot, 'apps/cli/src/args.ts');
    fs.mkdirSync(path.dirname(args), {recursive:true});
    fs.writeFileSync(args, '// profile "desktop" is managed exclusively by the Electron application');
    await assert.rejects(check(options), /reserves the "desktop" profile/);
    assert.deepEqual(JSON.parse(fs.readFileSync(f.options.output)).candidates, []);
    fs.unlinkSync(args);
    assert.equal((await check(options)).status, 'passed');
  } finally { f.close(); }
});

test('automatic entry points share results; manual checks and changed plugin files invalidate them', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const plugin = path.join(f.source, 'node_modules/good');
    fs.mkdirSync(plugin, {recursive:true});
    fs.writeFileSync(path.join(plugin, 'package.json'), '{"name":"good","version":"1"}');
    const code = path.join(plugin, 'index.js');
    fs.writeFileSync(code, 'one');
    assert.equal((await check({...f.options, trigger:'version_switch'})).cache_reused, false);
    assert.equal((await check({...f.options, trigger:'profile_switch'})).cache_reused, true);
    assert.equal((await check({...f.options, trigger:'startup'})).cache_reused, true);
    assert.equal((await check({...f.options, force:true, trigger:'manual_check'})).cache_reused, false);
    fs.writeFileSync(code, 'two'); // Same package manifest and file size.
    assert.equal((await check(f.options)).cache_reused, false);
    assert.equal((await check(f.options)).cache_reused, true);
    const entry = path.join(f.slot, 'apps/cli/lib/bin.js');
    fs.appendFileSync(entry, '\n// changed Harness implementation');
    assert.equal((await check(f.options)).cache_reused, false);
    await check({...f.options, release_id:'two'});
    assert.equal((await check(f.options)).cache_reused, true);
  } finally { f.close(); }
});

test('runtime environment changes invalidate verification without recording environment values', async () => {
  const f = fixture(), before = process.env.NEXUS_TEST_RUNTIME_IDENTITY;
  try {
    f.write(['good']);
    process.env.NEXUS_TEST_RUNTIME_IDENTITY = 'private-runtime-a';
    await check(f.options);
    process.env.NEXUS_TEST_RUNTIME_IDENTITY = 'private-runtime-b';
    assert.equal((await check(f.options)).cache_reused, false);
    assert.equal((await check(f.options)).cache_reused, true);
    assert.equal(fs.readFileSync(f.options.cache, 'utf8').includes('private-runtime'), false);
    fs.writeFileSync(f.options.cache, '{broken');
    assert.equal((await check(f.options)).cache_reused, false);
  } finally {
    if (before === undefined) delete process.env.NEXUS_TEST_RUNTIME_IDENTITY;
    else process.env.NEXUS_TEST_RUNTIME_IDENTITY = before;
    f.close();
  }
});

test('a failed forced check removes the matching old success', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const sentinel = path.join(f.root, 'external-failure');
    const entry = path.join(f.slot, 'apps/cli/lib/bin.js');
    fs.writeFileSync(entry, `if (require('node:fs').existsSync(${JSON.stringify(sentinel)})) process.exit(1);\n` + fs.readFileSync(entry, 'utf8'));
    await check(f.options);
    fs.writeFileSync(sentinel, 'fail');
    await assert.rejects(check({...f.options, force:true}));
    fs.unlinkSync(sentinel);
    assert.equal((await check(f.options)).cache_reused, false);
  } finally { f.close(); }
});

test('checks never publish profiles or change the source; cache contains results only', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const before = fs.readFileSync(path.join(f.source, 'package.json'));
    const first = await check({ ...f.options, trigger: 'version_switch' });
    assert.equal(first.effective_profile, 'original');
    assert.deepEqual(fs.readdirSync(path.join(f.home, 'profiles')), ['original']);
    assert.deepEqual(fs.readdirSync(f.options.work), []);
    const cache = fs.readFileSync(f.options.cache);
    const reused = await check({ ...f.options, trigger: 'startup' });
    assert.equal(reused.cache_reused, true); assert.equal(reused.trigger, 'version_switch');
    assert.equal(reused.last_trigger, 'startup'); assert.deepEqual(fs.readFileSync(f.options.cache), cache);
    const next = await check({ ...f.options, release_id: 'two' });
    assert.equal(next.cache_reused, false); assert.equal(next.effective_profile, 'original');
    assert.deepEqual(fs.readFileSync(path.join(f.source, 'package.json')), before);
    assert.deepEqual(fs.readdirSync(path.join(f.home, 'profiles')), ['original']);
  } finally { f.close(); }
});

test('real Harness validates the native profile without publishing a replacement', { skip: !process.env.NEXUS_TEST_DSH_ROOT }, async () => {
  const f = fixture();
  try {
    f.write(['@deepseek-ai/dsh-base', '@deepseek-ai/dsh-web-app']);
    const before = fs.readFileSync(path.join(f.source, 'package.json'));
    const report = await check({ ...f.options, slot: process.env.NEXUS_TEST_DSH_ROOT, timeout_ms: 45000 });
    assert.equal(report.status, 'passed'); assert.equal(report.effective_profile, 'original');
    assert.deepEqual(fs.readdirSync(path.join(f.home, 'profiles')), ['original']);
    assert.deepEqual(fs.readFileSync(path.join(f.source, 'package.json')), before);
    assert.deepEqual(fs.readdirSync(f.options.work), []);
  } finally { f.close(); }
});

test('disabled native bundles keep their order and dependencies and checks do not rewrite them', async () => {
  const f = fixture();
  try {
    f.write(['good']);
    const manifest = JSON.parse(fs.readFileSync(path.join(f.source, 'package.json')));
    manifest.dsh.profile.nexusDisabledBundles = [{ package: 'bad', index: 0, following: ['good'] }];
    const before = JSON.stringify(manifest); fs.writeFileSync(path.join(f.source, 'package.json'), before);
    const result = await check(f.options);
    assert.equal(result.status, 'isolated'); assert.equal(result.effective_profile, 'original');
    assert.deepEqual(result.disabled, [{ package: 'bad', reason: 'Disabled by user' }]);
    assert.equal(fs.readFileSync(path.join(f.source, 'package.json'), 'utf8'), before);
    assert.deepEqual(fs.readdirSync(path.join(f.home, 'profiles')), ['original']);
  } finally { f.close(); }
});

test('owned checks leave only disposable work until outer process reconciliation', async () => {
  const f = fixture();
  try {
    f.write(['good']); await check({ ...f.options, owned_round: true });
    assert.deepEqual(fs.readdirSync(path.join(f.home, 'profiles')), ['original']);
    assert.ok(fs.readdirSync(f.options.work).length > 0);
    assert.equal(fs.existsSync(path.join(f.options.work, 'publication.json')), false);
    f.write(['soft-bad']); await assert.rejects(check({ ...f.options, owned_round: true }), /explicit plugin decision/);
    assert.deepEqual(fs.readdirSync(path.join(f.home, 'profiles')), ['original']);
    assert.deepEqual(JSON.parse(fs.readFileSync(path.join(f.source, 'package.json'))).dsh.profile.bundles, ['soft-bad']);
  } finally { f.close(); }
});

test('enabled patches prevent automatic plugin removal after a failed probe', async()=>{
  const f=fixture();
  try {
    f.write(['bad']); const patch=path.join(f.root,'extra.yml'); fs.writeFileSync(patch,'[]');
    await assert.rejects(check({...f.options, patches:[patch]}), /explicit plugin decision/);
    const report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.status,'needs_choice'); assert.deepEqual(report.disabled,[]);
    assert.deepEqual(JSON.parse(fs.readFileSync(path.join(f.source,'package.json'))).dsh.profile.bundles,['bad']);
  } finally { f.close(); }
});

test('unknown errors fail closed without publishing a profile',async()=>{
  const f=fixture();
  try {
    f.write(['unknown']);
    await assert.rejects(check({...f.options,trigger:'version_switch'}),/Harness startup check failed/);
    const report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.status,'failed');
    assert.equal(report.trigger,'version_switch');
    assert.equal(report.last_trigger,'version_switch');
    assert.equal(report.cache_reused,false);
    assert.deepEqual(report.candidates,[]);
    assert.deepEqual(fs.readdirSync(f.home+'/profiles').filter(x=>x.startsWith('nexus-')),[]);
    assert.deepEqual(fs.readdirSync(f.options.work),[]);
  } finally {f.close();}
});

test('timeout fails closed and cleans the owned probe',async()=>{
  const f=fixture();
  try {
    f.write(['timeout']);
    await assert.rejects(check({...f.options,timeout_ms:300}),/timed out/);
    assert.equal(JSON.parse(fs.readFileSync(f.options.output)).status,'failed');
    assert.equal(JSON.parse(fs.readFileSync(f.options.output)).failure_stage, 'readiness_timeout');
    assert.deepEqual(fs.readdirSync(f.options.work),[]);
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
    assert.deepEqual(fs.readdirSync(path.join(f.home,'profiles')),['original']);
    const outside=path.join(f.root,'outside');fs.mkdirSync(outside);
    fs.symlinkSync(outside,path.join(modules,'escape'),process.platform==='win32'?'junction':'dir');
    assert.throws(()=>planCanary(f.options),/escapes/);
  } finally {f.close();}
});

test('local dependency origins distinguish transitive declarations from loader failures', () => {
  const f = fixture();
  try {
    f.write(['plugin-a', 'plugin-b']);
    for (const [name, dependencies] of [['plugin-a', {'helper':'1'}], ['helper', {'@noble/hashes':'2'}], ['plugin-b', {'@noble/hashes':'2'}]]) {
      const dir = path.join(f.source, 'node_modules', name);
      fs.mkdirSync(dir, {recursive:true});
      fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({name, dependencies}));
    }
    const [origin] = dependencyOrigins(sourceInfo(f.home, 'original'), ['@noble/hashes']);
    assert.deepEqual(origin.chains, [['plugin-a','helper','@noble/hashes'], ['plugin-b','@noble/hashes']]);
    assert.deepEqual(origin.direct_dependents, ['helper','plugin-b']);
    assert.deepEqual(origin.loader_failures, []);
    const [unknown] = dependencyOrigins(sourceInfo(f.home, 'original'), ['missing']);
    assert.deepEqual(unknown.chains, []);
  } finally { f.close(); }
});

test('duplicate loader IDs identify declared bundle sources without executing plugins', () => {
  const f = fixture();
  try {
    f.write(['@deepseek-ai/settings','uploader']);
    for (const [name, dir] of [['@deepseek-ai/settings',path.join(f.slot,'vendor/settings')],['uploader',path.join(f.source,'node_modules/uploader')]]) {
      fs.mkdirSync(dir,{recursive:true});
      fs.writeFileSync(path.join(dir,'package.json'),JSON.stringify({name,dsh:{bundle:{patch:'./cordis.patch.yml'}}}));
      fs.writeFileSync(path.join(dir,'cordis.patch.yml'), '- insert:\n    - id: file-upload\n      name: uploader\n');
    }
    const matches=duplicateEntrySources(sourceInfo(f.home,'original'),f.slot,'duplicate loader entry id: file-upload');
    assert.deepEqual(matches.map(row=>row.package),['@deepseek-ai/settings','uploader']);
    assert.ok(matches.every(row=>row.line===2));
    assert.deepEqual(duplicateEntrySources(sourceInfo(f.home,'original'),f.slot,'unrelated failure'),[]);
  } finally { f.close(); }
});

test('upstream startup signatures map to explicit Nexus repair categories', () => {
  const cases = [
    ['profile "desktop" is managed exclusively by the Electron application','profile_restriction'],
    ['duplicate loader entry id: upload','duplicate_entry'],
    ['dsh: failed to parse config /profile/cordis.yml: bad token','configuration'],
    ['dsh: patches /profile/a.yml must be a top-level YAML array of loader patch entries','configuration'],
    ['dsh: cannot resolve profile bundle "third-party" from the dsh installation','missing_bundle'],
    ['does not provide an export named tools','module_api'],
    ["Cannot find package 'missing' imported from /plugin/index.js",'missing_module'],
    ['profile resolution mismatch for "pkg" from /source: disk selected nothing','module_layout'],
    ['profile resolution: replacing "pkg" requires a process restart','restart_required'],
    ['cannot resolve entry missing-entry','patch_target'],
    ['listen EADDRINUSE: address already in use','port_conflict'],
    ['EPERM: operation not permitted, open /config','permission'],
    ['dsh: startup failed: 1 required plugin did not activate\nPlugins waiting for services (1):\nserver (required) database','required_services'],
    ['dsh: startup failed: 1 required plugin did not activate\nFailed plugins (1):\n a (required)\n Package: third-party','plugin_activation'],
    ['error: --profile needs a name','runtime_arguments'],
    ['dsh: installed package pkg must declare a non-empty version','package_manifest'],
    ['Compatibility probe timed out','readiness_timeout'],
    ['Something unexpected happened','unknown'],
  ];
  for (const [message, code] of cases) {
    const diagnosis=diagnoseStartup(message);
    assert.equal(diagnosis.code,code,message);
    assert.equal(diagnosis.level,'blocking');
    assert.ok(diagnosis.remedy && diagnosis.help);
    assert.ok(diagnosis.evidence.length);
  }
  assert.equal(diagnoseStartup('Something unexpected happened').certainty,'unconfirmed');
});

test('upstream optional activation warnings allow authenticated readiness and remain nonblocking', async () => {
  const f=fixture();
  try {
    f.write(['soft-bad']);
    const boot=path.join(f.slot,'packages/boot/app-boot/src/index.ts');
    fs.mkdirSync(path.dirname(boot),{recursive:true});
    fs.writeFileSync(boot,'// required\nfunction startupDiagnostic() {}\nfunction activationDiagnostic() {}');
    const entry=path.join(f.slot,'apps/cli/lib/bin.js');
    fs.appendFileSync(entry, `\nconsole.error('dsh: warning: 1 entry did not activate\\noptional (soft-bad): failed to import');`);
    const report=await check(f.options);
    assert.equal(report.status,'passed');
    assert.equal(report.diagnosis.level,'limited');
    assert.equal(report.diagnosis.code,'optional_plugins');
    assert.equal(report.candidates,undefined);
  } finally {f.close();}
});

test('service failure plus a failed extension and a replaced built-in yields a bounded repair plan', async () => {
  const f = fixture();
  try {
    f.write(['@deepseek-ai/settings', 'upload-addon', '@example/archive', 'consumer']);
    fs.writeFileSync(path.join(f.slot, 'vendor/settings/package.json'), JSON.stringify({ name: '@deepseek-ai/settings', version: '1', dsh: { bundle: { patch: 'patch.yml' } } }));
    fs.writeFileSync(path.join(f.slot, 'vendor/settings/patch.yml'), '- insert:\n    - id: upload\n      name: "@deepseek-ai/upload"\n');
    const replacement = path.join(f.source, 'node_modules/upload-addon'); fs.mkdirSync(replacement, { recursive: true });
    fs.writeFileSync(path.join(replacement, 'package.json'), JSON.stringify({ name: 'upload-addon', dsh: { bundle: { patch: 'patch.yml' } } }));
    fs.writeFileSync(path.join(replacement, 'patch.yml'), '- insert:\n    - id: upload\n      name: upload-addon\n');
    const boot = path.join(f.slot, 'packages/boot/app-boot/src/index.ts'); fs.mkdirSync(path.dirname(boot), { recursive: true });
    fs.writeFileSync(boot, '// required\nfunction startupDiagnostic() {}\nfunction activationDiagnostic() {}');
    const warning = 'dsh: warning: 3 entries did not activate\narchive (@example/archive/workspace): Error: codec has no create() factory\ncontroller (@deepseek-ai/controller): pending (waiting for services: uploads, workspace)\nconsumer (consumer): pending (waiting for service: sessions)';
    fs.appendFileSync(path.join(f.slot, 'apps/cli/lib/bin.js'), '\nconsole.error(' + JSON.stringify(warning) + ');');
    const report = await check(f.options);
    assert.equal(report.status, 'needs_choice');
    assert.deepEqual(report.diagnosis.repair_candidates.map(row => row.package), ['@example/archive', 'upload-addon']);
    assert.equal(report.diagnosis.replacements[0].original, '@deepseek-ai/upload');
    assert.equal(report.diagnosis.repair_candidates.some(row => row.package === 'consumer'), false);
    assert.deepEqual(sourceInfo(f.home, 'original').manifest.dsh.profile.bundles, ['@deepseek-ai/settings', 'upload-addon', '@example/archive', 'consumer']);
    assert.ok(Buffer.byteLength(JSON.stringify(report)) < 65536);
    assert.deepEqual(activationRepairCandidates(parseActivation(warning), ['@example/archive-other', 'consumer']), []);
    assert.equal(replacedOfficialEntries(sourceInfo(f.home, 'original'), f.slot).length, 1);
    // The same evidence must survive an early process exit before HTTP readiness.
    fs.writeFileSync(path.join(f.slot, 'apps/cli/lib/bin.js'), 'console.error(' + JSON.stringify(warning.replace('dsh: warning:', 'dsh: required startup failure:')) + '); process.exit(1);');
    await assert.rejects(check(f.options));
    const failedReport = JSON.parse(fs.readFileSync(f.options.output, 'utf8'));
    assert.equal(failedReport.status, 'needs_choice');
    assert.equal(failedReport.failure_stage, 'plugin_loading');
    assert.deepEqual(failedReport.diagnosis.repair_candidates.map(row => row.package), ['@example/archive', 'upload-addon']);
    assert.match(failedReport.candidates.find(row => row.package === '@example/archive').reason, /codec/);

  } finally { f.close(); }
});

test('the activation audit is read as root causes plus the entries waiting on them', () => {
  // Exact upstream shapes: app-boot inactiveEntries and client assertEntriesActive.
  const host = parseActivation([
    'dsh: warning: 3 entries did not activate',
    'store (@deepseek-ai/dsh-store): failed to import',
    'chat (@deepseek-ai/dsh-client-ui-chat): pending (waiting for service: sessions)',
    'jobs (@deepseek-ai/dsh-client-ui-jobs): pending (waiting for services: sessions, uiWorkspace)',
  ].join('\n'));
  assert.equal(host.entries.length, 3);
  assert.deepEqual(host.entries.filter(entry => entry.state === 'failed').map(entry => entry.package), ['@deepseek-ai/dsh-store']);
  // The most widely awaited service is named first so a remedy has one target.
  assert.deepEqual(host.missing_services, ['sessions', 'uiWorkspace']);
  assert.equal(host.truncated, false);

  const web = parseActivation('web boot: 1 entry did not activate\nchat: pending (waiting for service: sessions)');
  assert.equal(web.entries[0].package, 'chat');
  assert.deepEqual(web.missing_services, ['sessions']);

  // A required failure carries a stack; stack frames are never read as entries.
  const required = parseActivation([
    'dsh: required startup failure: 1 entry did not activate',
    'boot (@deepseek-ai/dsh-boot): Error: broke',
    '    at boot (file:///slot/lib/index.js:10:3)',
  ].join('\n'));
  assert.equal(required.entries.length, 1);
  assert.equal(required.entries[0].state, 'failed');

  assert.equal(parseActivation('dsh: harness ready'), null);
  assert.equal(parseActivation(''), null);
  assert.equal(parseActivation('dsh: warning: 0 entries did not activate'), null);

  const many = parseActivation(['dsh: warning: 60 entries did not activate',
    ...Array.from({ length: 60 }, (_, i) => `e${i} (pkg-${i}): pending (waiting for service: sessions)`)].join('\n'));
  assert.equal(many.entries.length, 48);
  assert.equal(many.truncated, true);
});

test('startup infrastructure causes are not plugin-isolation decisions', () => {
  for (const [text, code] of [
    ['failed to read config: EACCES: permission denied', 'permission'],
    ['failed to apply loader entry nexus-desktop-bridge: Cannot find module x', 'nexus_integration'],
    ['failed to read config: ENOSPC: no space left on device', 'storage_full'],
    ['Probe process cleanup timed out', 'cleanup_timeout'],
    ['Source profile changed during compatibility check', 'inputs_changed'],
    ['Harness process exited before readiness (exit code: 7)', 'process_exit'],
    ['Optional provider request timed out', 'unknown'],
  ]) assert.equal(diagnoseStartup(text).code, code);
  const activation = parseActivation('dsh: warning: 2 entries did not activate\nprovider (addon): EPERM: permission denied\nconsumer (@deepseek-ai/core): pending (waiting for service: store)');
  assert.deepEqual(activationRepairCandidates(activation, ['addon'], [{package:'replacement',id:'store'}]), []);
});

test('unattributed missing official services block startup without inventing a plugin choice', async () => {
  const f=fixture();
  try {
    f.write(['good']);
    const boot=path.join(f.slot,'packages/boot/app-boot/src/index.ts');fs.mkdirSync(path.dirname(boot),{recursive:true});
    fs.writeFileSync(boot,'// required\nfunction startupDiagnostic() {}\nfunction activationDiagnostic() {}');
    fs.appendFileSync(path.join(f.slot,'apps/cli/lib/bin.js'), '\nconsole.error("dsh: warning: 1 entry did not activate\\ncontroller (@deepseek-ai/controller): pending (waiting for service: store)");');
    const report=await check(f.options);
    assert.equal(report.status,'failed');
    assert.equal(report.diagnosis.code,'required_services');
    assert.deepEqual(report.candidates,[]);
    assert.equal(report.diagnosis.repair_candidates,undefined);
  } finally {f.close();}
});

test('silent exit and timeout preserve actionable startup evidence', async () => {
  const f=fixture();
  try {
    f.write(['good']);
    const entry=path.join(f.slot,'apps/cli/lib/bin.js');
    fs.writeFileSync(entry,'process.exit(7);');
    await assert.rejects(check(f.options));
    let report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.diagnosis.code,'process_exit');
    assert.match(report.error,/exit code: 7/);
    fs.writeFileSync(entry,'console.error("EADDRINUSE: address already in use");setInterval(()=>{},1000);');
    await assert.rejects(check({...f.options,timeout_ms:500}));
    report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.diagnosis.code,'port_conflict');
    assert.deepEqual(report.candidates,[]);
  } finally {f.close();}
});

test('grouped fatal startup reports retain failed package identity and pending consumers', async () => {
  const text='dsh: startup failed: 1 required plugin did not activate\n\nFailed plugins (1):\n  archive (required)\n    Package: @example/archive/workspace\n    apply: Error: strict codec has no create() factory\n\nPlugins waiting for services (2):\n  Plugin                 Missing services\n  session (required)     workspaceRegistry, fileUploads\n  chat                   sessions';
  const activation=parseActivation(text);
  assert.equal(activation.entries[0].package,'@example/archive/workspace');
  assert.equal(activation.entries[0].state,'failed');
  assert.deepEqual(activation.entries[1].missing,['workspaceRegistry','fileUploads']);
  assert.equal(activation.entries[1].state,'pending');
  assert.deepEqual(activationRepairCandidates(activation,['@example/archive','chat']).map(row=>row.package),['@example/archive']);
  const f=fixture();
  try {
    f.write(['@example/archive','chat']);
    fs.writeFileSync(path.join(f.slot,'apps/cli/lib/bin.js'),'console.error('+JSON.stringify(text)+');process.exit(1);');
    await assert.rejects(check(f.options));
    const report=JSON.parse(fs.readFileSync(f.options.output));
    assert.equal(report.status,'needs_choice');
    assert.deepEqual(report.diagnosis.repair_candidates.map(row=>row.package),['@example/archive']);
  } finally {f.close();}
});

test('parallel dependency copies stay isolated, skip official packages and reject escaped links', async t => {
  const root=fs.mkdtempSync(path.join(os.tmpdir(),'nexus-copy-'));
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  const source=path.join(root,'source'), target=path.join(root,'target');
  fs.mkdirSync(path.join(source,'plugin'),{recursive:true});
  fs.mkdirSync(path.join(source,'official'),{recursive:true});
  fs.writeFileSync(path.join(source,'plugin/index.js'),'original');
  fs.writeFileSync(path.join(source,'official/index.js'),'skip');
  const {copyModules}=await import('../src/compatibility.mjs');
  await copyModules(source,target,new Map([['official',true]]));
  assert.equal(fs.existsSync(path.join(target,'official')),false);
  fs.writeFileSync(path.join(target,'plugin/index.js'),'probe write');
  assert.equal(fs.readFileSync(path.join(source,'plugin/index.js'),'utf8'),'original');
  const outside=path.join(root,'outside');fs.mkdirSync(outside);
  fs.symlinkSync(outside,path.join(source,'escape'),process.platform==='win32'?'junction':'dir');
  await assert.rejects(copyModules(source,path.join(root,'rejected'),new Map()),/escapes/);
});
