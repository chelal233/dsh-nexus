import fs from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import path from 'node:path';
import { createHash, randomBytes } from 'node:crypto';
import { constants as zlibConstants, zstdCompressSync, zstdDecompressSync } from 'node:zlib';

/**
 * Frozen rc2 synthetic business acceptance; actual results live in run evidence.
 * fresh/old/final native phases.
 * Required additions to the supplied context:
 * phase: 'fresh'|'old'|'final'
 * expectedIdentity: >=2 exact fields from the independently verified package identity
 * budgetGate({stage, cleanup:false}): Promise<{ok:true, ...measurement}>; throws/ok:false blocks
 * findOfficialPage({url, openedAfter}): Promise<{page:{evaluate}, targetId, method:'existing-cdp-target'}>
 *   Must enumerate the actual browser opened by xdg-open. Never create/navigate a substitute.
 * deadlineAt: absolute milliseconds, leaves caller's global cleanup time
 * Optional signal: AbortSignal. Caller owns all Electron/browser process cleanup.
 * desktop/page.evaluate accepts a JavaScript EXPRESSION STRING (existing CDP contract).
 * Its Runtime.evaluate call must awaitPromise:true and returnByValue:true.
 * An evidence directory may be a separate explicitly supplied mount, e.g. /evidence/gui.
 * root defaults are supplied by caller: /qa/test; A data=root/business, HOME=root/home, DSH=root/dsh.
 * This module creates bounded synthetic files, not a package-manager installation test.
 * final additionally requires launchInstance(spec), restartInstance({instance,spec}).
 * spec={executable,dataRoot,home,dshHome,userDataDir}; adapters use existing owned start/connect.
 * Both return {desktop:{evaluate},proc:{pid}} after native GUI is connected.
 * Restart MUST normally stop old GUI/Agent and start the same spec, no lock deletion.
 * Preserve Chromium sandbox; unset NEXUS_HARNESS_ROOT instead of setting an empty directory.
 * Package transactions remain external. Recovery here is in-flight Cancel + normal restart,
 * NOT SIGKILL/crash recovery. Caller still performs global owned process cleanup.
 * Each import explicitly Stops Harness after startup_status may autostart it.
 * Reserve up to 40 seconds for local cleanup (cold settlement + Harness Stop).
 */
const TAG = 'dsh-v0.2.0-rc.2';
const HEAD = '639ed015397290b3745d163aafe02ffee4aa3f84';
const TEXT = 'Synthetic valid offline migration session. No model request.';
const MARKER = 'DSH_QA_ACTIVATED:qa-round2-transitive-ok';
const READ_MARKER = 'DSH_QA_READ_OK:qa-valid-session';
const WORKSPACE = '/qa/test/fixture-workspace';
// Frozen format.ts projectKey('/qa/test/fixture-workspace'); header and path agree.
const SESSION_DIRECTORY = 'sessions/--qa-test-fixture-workspace--/qa-valid-session';
const SESSION = '{"type":"session","version":4,"id":"qa-valid-session","createdAt":1,"cwd":"/qa/test/fixture-workspace","isSeeded":false,"delegationDepth":0}\n' +
  '{"type":"user/message","seq":0,"time":2,"data":{"content":[{"type":"text","text":"Synthetic valid offline migration session. No model request."}],"source":{"kind":"user"},"role":"user","id":"qa-valid-message"},"surfaceOp":"append"}\n';
const SESSION_HASH = '8e6ab8c4df5804b7fb813d92a24dab00231fb76404abcdf7dcfe05fcd2f19b8f';
const digest = x => createHash('sha256').update(x).digest('hex');
const fail = (code, message) => { throw Object.assign(new Error(message), {code}); };
async function fileHash(file) {
  const hash=createHash('sha256');
  for await (const bytes of createReadStream(file)) hash.update(bytes);
  return hash.digest('hex');
}
async function evaluate(page, fn, arg) {
  const expression = '(' + fn.toString() + ')(' + JSON.stringify(arg) + ')';
  const response = await page.evaluate(expression);
  if (response?.exceptionDetails) fail('browser_evaluation_failed',response.exceptionDetails.text || 'CDP exception');
  if (response?.result && typeof response.result.type === 'string') {
    if (response.result.subtype === 'promise') fail('contract_missing','Runtime.evaluate requires awaitPromise:true');
    if (!Object.hasOwn(response.result,'value') && response.result.type !== 'undefined')
      fail('contract_missing','Runtime.evaluate requires returnByValue:true');
    return response.result.value;
  }
  return response;
}
const stable = v => v && typeof v === 'object'
  ? Array.isArray(v) ? '[' + v.map(stable).join(',') + ']'
    : '{' + Object.keys(v).sort().map(k => JSON.stringify(k)+':'+stable(v[k])).join(',') + '}'
  : JSON.stringify(v);
const safe = v => {
  if (Array.isArray(v)) return v.map(safe);
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k,x]) =>
    [k, /token|authorization|password|secret/i.test(k) ? '[redacted]' : safe(x)]));
  if (typeof v === 'string') return v.replace(/https?:\/\/[^\s"'<>]+/g, value => {
    try { const u = new URL(value); return u.origin + u.pathname; } catch { return '[url]'; }
  });
  return v;
};
async function ordinary(root, p, directory = true) {
  const realRoot = await fs.realpath(root), target = await fs.realpath(p);
  const rel = path.relative(realRoot, target);
  if (rel.startsWith('..') || path.isAbsolute(rel)) fail('qa_path_escape', p);
  const s = await fs.lstat(p);
  if (s.isSymbolicLink() || (directory ? !s.isDirectory() : !s.isFile())) fail('qa_path_invalid', p);
  return target;
}
async function headAt(slot) {
  let s = (await fs.readFile(path.join(slot,'.git/HEAD'),'utf8')).trim();
  if (s.startsWith('ref: ')) {
    const ref = s.slice(5);
    if (!/^refs\/[A-Za-z0-9/_.-]+$/.test(ref) || ref.includes('..')) fail('git_ref_invalid',ref);
    try { s = (await fs.readFile(path.join(slot,'.git',ref),'utf8')).trim(); }
    catch (e) {
      if (e.code !== 'ENOENT') throw e;
      s = (await fs.readFile(path.join(slot,'.git/packed-refs'),'utf8')).split('\n')
        .find(line => line.endsWith(' '+ref))?.split(' ')[0];
    }
  }
  return s;
}
export async function runBusinessQA(context) {
  const c = context, phase = c.phase || 'fresh';
  if (!['fresh','old','final'].includes(phase)) fail('contract_invalid','phase');
  if (phase === 'final') for (const key of ['launchInstance','restartInstance'])
    if(typeof c[key]!=='function') fail('contract_missing',key);
  for (const key of ['budgetGate','findOfficialPage'])
    if (typeof c[key] !== 'function') fail('contract_missing',key);
  if (!c.desktop?.evaluate || !path.isAbsolute(c.root || '') || !Number.isFinite(c.deadlineAt))
    fail('contract_missing','absolute root, desktop.evaluate, deadlineAt');
  if (!c.expectedIdentity || Object.keys(c.expectedIdentity).length < 2)
    fail('contract_missing','expectedIdentity: exact verified package fields');
  const root = await ordinary(c.root,c.root);
  let data = await ordinary(root,path.join(root,'business'));
  let home = await ordinary(root,path.join(root,'dsh'));
  let desktop=c.desktop, instanceB=null, specB=null, activePage=null;
  const sourceData=data, sourceHome=home;
  const out = typeof c.evidence === 'string' ? c.evidence : path.join(root,'evidence');
  if (!path.isAbsolute(out)) fail('contract_invalid','evidence must be an explicit absolute directory');
  await fs.mkdir(out,{recursive:true}); await ordinary(out,out);
  const stem = phase + '-' + Date.now(), journal = path.join(out,stem+'.jsonl');
  const stateFile = path.join(root,'business-qa-state.json');
  let stage = 'preflight', ownedOperation = null, selected = null, cleanupOK = true, result;
  let verifiedOwnership = false, migrationVerified = false;
  const delay = ms => new Promise(resolve => setTimeout(resolve,ms));
  const record = async (kind, value) => {
    const item = {time:new Date().toISOString(),phase,stage,kind,value:safe(value)};
    await fs.appendFile(journal,JSON.stringify(item)+'\n');
    if (typeof c.report === 'function') await c.report(item);
    else if (Array.isArray(c.report)) c.report.push(item);
  };
  const gate = async () => {
    if (c.signal?.aborted) fail('ABORTED',String(c.signal.reason || 'Aborted'));
    if (Date.now() >= c.deadlineAt) fail('DEADLINE','Business deadline reached');
    const b = await c.budgetGate({stage,cleanup:false,dataRoot:data,dshHome:home});
    if (b?.ok !== true) fail('BUDGET_BLOCKED',JSON.stringify(safe(b)));
  };
  const invoke = (command,args={}) => evaluate(desktop,
    async ({command,args}) => {
      if (!window.nexusDesktop?.invoke) throw new Error('Native Nexus bridge missing');
      return window.nexusDesktop.invoke(command,args);
    }, {command,args});
  const api = async (route, body, cleanup=false) => {
    if (!cleanup) await gate();
    let payload = body;
    if (body && route === '/v1/updates' && ['switch','offline_import'].includes(body.action)) {
      const id = Math.floor(Date.now()/1000)+'-'+randomBytes(16).toString('hex');
      payload = {...body,request_id:id};
      // Persist before the side effect. No automatic transport retry.
      await record('request',{route,body:payload});
    }
    const value = await invoke('proxy_request',{method:body?'POST':'GET',path:route,body:payload??null});
    return value;
  };
  const wait = async (label, probe, ms=90000) => {
    const end = Math.min(Date.now()+ms,c.deadlineAt);
    while (Date.now()<end) {
      await gate();
      const value = await probe(); if (value) return value;
      await delay(500);
    }
    fail('WAIT_TIMEOUT',label);
  };
  const step = async (name, fn) => {
    stage = name; await gate(); await record('begin',{});
    const value = await fn(); await record('PASS',value??{}); return value;
  };
  const stopHarness = async (cleanup=false) => {
    const stopped = response => ['detached','stopped'].includes(response.harness?.state) && response.harness.pid == null;
    const response = await api('/v1/harness',{action:'stop'},cleanup);
    await record('stop_response',response);
    if (stopped(response)) return response;
    if (cleanup) {
      const end = Date.now()+20000;
      while (Date.now()<end) {
        const h = await api('/v1/harness',undefined,true);
        if (stopped(h)) return h;
        await delay(250);
      }
      fail('CLEANUP_TIMEOUT','Harness did not report stopped');
    }
    return wait('Harness stopped',async()=> {
      const h=await api('/v1/harness'); return stopped(h)&&h;
    });
  };
  const verifyPage = async () => {
    await api('/v1/harness',{action:'start'});
    const ready = await wait('current Web Ready',async()=> {
      const h=await api('/v1/harness'), ui=await api('/v1/harness/ui');
      return h.harness?.state==='running' && h.harness?.pid &&
        ui.available===true && ui.browser_health?.state==='active' &&
        ui.run_id===h.log_session_run_id && ui.generation===h.generation && {h,ui};
    });
    const first=ready.h;
    if (!/^[A-Za-z0-9._-]+$/.test(first.log_stdout_name||'')) fail('log_path_invalid','stdout name');
    const log=path.join(data,'logs',first.log_stdout_name);
    await wait('real backend read and Cordis apply markers',async()=> {
      const h=await api('/v1/harness');
      if (h.log_session_run_id!==first.log_session_run_id ||
          h.log_stdout_file_identity!==first.log_stdout_file_identity)
        fail('run_changed','Harness run changed while observing markers');
      await ordinary(root,log,false);
      const s=await fs.stat(log); if(s.size>8*1024*1024) fail('log_budget','stdout exceeds 8 MiB read cap');
      const text=await fs.readFile(log,'utf8');
      return text.includes(MARKER)&&text.includes(READ_MARKER);
    });
    await record('actual_persistence_read',{runId:first.log_session_run_id,marker:READ_MARKER,
      scope:'Real backend list/open/read/close; read-only access, no SessionManager or model invocation'});
    const openedAfter = Date.now();
    const opened = await api('/v1/harness/ui',{action:'open'});
    if (typeof opened.url !== 'string') fail('open_missing_url','Official Open returned no URL');
    const found = await c.findOfficialPage({url:opened.url,openedAfter});
    if (!found?.page?.evaluate || !found.targetId || found.method!=='existing-cdp-target')
      fail('official_target_missing','Need actual officially opened CDP page');
    const page=found.page, expected=new URL(opened.url);
    activePage=found;
    await wait('official browser document',async()=> {
      const observed=await evaluate(page,({origin,pathname})=>({
        blocked:document.body?.innerText.includes('ERR_BLOCKED_BY_CLIENT')===true,
        ready:location.origin===origin&&location.pathname===pathname&&!!document.body}),
        {origin:expected.origin,pathname:expected.pathname});
      if(observed.blocked)fail('BROWSER_BLOCKED','Official browser entry: ERR_BLOCKED_BY_CLIENT');
      return observed.ready;
    });
    const clickVisible = async locator => {
      const point=await evaluate(page,locator=>{
        const matches=locator.label
          ?Array.from(document.querySelectorAll('button')).filter(button=>button.textContent.trim()===locator.label)
          :Array.from(document.querySelectorAll('[role="treeitem"][data-row-key]')).filter(row=>row.getAttribute('data-row-key')===locator.rowKey);
        const visible=matches.filter(element=>!element.disabled&&element.getClientRects().length>0);
        if(visible.length!==1)return null;
        const button=visible[0],r=button.getBoundingClientRect();
        const x=r.left+r.width/2,y=r.top+r.height/2,hit=document.elementFromPoint(x,y);
        return hit===button||button.contains(hit)?{x,y}:null;
      },locator);
      if(!point)return false;
      await page.cdp('Input.dispatchMouseEvent',{type:'mousePressed',...point,button:'left',clickCount:1});
      await page.cdp('Input.dispatchMouseEvent',{type:'mouseReleased',...point,button:'left',clickCount:1});
      await record('normal_pointer_click',{...locator,targetId:found.targetId});
      return true;
    };
    const acknowledged=new Set();
    // rc2 reads welcome/models asynchronously after Workspaces has rendered.
    // Keep normal dialog actions and tree navigation in the same bounded loop.
    await wait('official selected session message body',async()=> {
      const surface=await evaluate(page,({origin,pathname,text,workspaceName})=>{
        if(location.origin!==origin||location.pathname!==pathname)return {ready:false};
        const visibleLabels=Array.from(document.querySelectorAll('button'))
          .filter(button=>button.getClientRects().length>0).map(button=>button.textContent.trim());
        const notice=document.body?.innerText.includes('Preview Notice')===true;
        // rc2 group keys contain bootstrap-generated workspace UUIDs, not cwd.
        const groups=Array.from(document.querySelectorAll('[role="treeitem"][data-row-key]')).filter(row=>
          row.getAttribute('data-row-key').startsWith('workspace:')&&row.getClientRects().length>0&&
          row.innerText.includes(workspaceName));
        const group=groups.length===1?groups[0]:null;
        const row=document.querySelector('[data-row-key="session:qa-valid-session"]');
        const chat=document.querySelector('[data-conversation-content][data-conversation-session="qa-valid-session"][data-conversation-region="chat"]');
        const scroll=chat?.querySelector(':scope > [data-conversation-scroll]');
        // rc2 ConversationContent + DefaultConversationViews: exclude the composer seat.
        const message=!!scroll&&Array.from(scroll.children).some(view=>
          !view.hasAttribute('data-composer-seat')&&view.getClientRects().length>0&&
          view.innerText.includes(text));
        return {ready:true,notice,visibleLabels,ambiguousGroup:groups.length>1,
          groupKey:group?.getAttribute('data-row-key'),collapsed:group?.getAttribute('aria-expanded')==='false',
          session:!!row,selected:row?.getAttribute('aria-selected')==='true',message};
      },{origin:expected.origin,pathname:expected.pathname,text:TEXT,workspaceName:path.basename(WORKSPACE)});
      if(!surface.ready)return false;
      if(surface.ambiguousGroup)fail('workspace_group_ambiguous','QA workspace name must identify one actual group');
      const label=surface.notice?'Continue':surface.visibleLabels.includes('Configure later')?'Configure later':null;
      if(label) {
        if(!acknowledged.has(label)&&await clickVisible({label}))acknowledged.add(label);
        return false;
      }
      if(surface.selected&&surface.message)return true;
      // onToggle belongs to the outer workspace treeitem, not an inner button.
      if(surface.collapsed)await clickVisible({rowKey:surface.groupKey});
      else if(surface.session&&!surface.selected)await clickVisible({rowKey:'session:qa-valid-session'});
      return false;
    });
    if (typeof page.cdp === 'function') {
      if (!/^[A-Za-z0-9._-]+$/.test(first.log_session_run_id || '')) fail('log_path_invalid','run ID');
      const screenshot = await page.cdp('Page.captureScreenshot', { format: 'png' });
      await fs.writeFile(path.join(out, phase + '-' + first.log_session_run_id + '-session.png'), Buffer.from(screenshot.data, 'base64'));
    }
    return {targetId:found.targetId,url:expected.origin+expected.pathname,
      runId:first.log_session_run_id,pid:first.harness.pid,sessionText:TEXT,marker:MARKER,actualPersistenceRead:READ_MARKER};
  };
  const coldFinished = async (id, terminal='succeeded') => wait('cold '+terminal,async()=> {
    const response=await api('/v1/updates'), op=response.operation;
    if(op?.operation_id!==id)fail('operation_changed','Cold operation identity changed');
    await record('cold_progress',{operation:id,phase:op.phase,progress:response.offline_progress});
    if(['failed','cancelled'].includes(op.phase)&&op.phase!==terminal)
      fail('cold_failed',JSON.stringify(safe(op)));
    return op.phase===terminal&&!op.cleanup_pending&&op.owner_quiescent===true&&op;
  },20*60*1000);
  const startImport = async (archive,contents) => {
    // startup_status can autostart a configured Harness after a normal Agent restart.
    await stopHarness();
    const response=await api('/v1/updates',{action:'offline_import',archive_path:archive,offline_contents:contents});
    const id=response.operation?.operation_id;
    if(!id)fail('operation_missing','Import response missing operation_id');
    ownedOperation=id;return id;
  };
  const verifyNative = async () => {
    const startup=await invoke('startup_status'), identity=await invoke('build_identity');
    if(await fs.realpath(startup.data_root)!==data)fail('wrong_data_root','Native Agent is not intended QA root');
    verifiedOwnership=true;
    for(const [k,v] of Object.entries(c.expectedIdentity))
      if(stable(identity[k])!==stable(v))fail('identity_mismatch',k);
    return {startup,identity};
  };
  try {
    await step('native_identity',async()=> {
      const startup=await invoke('startup_status'), identity=await invoke('build_identity');
      if (await fs.realpath(startup.data_root)!==data) fail('wrong_data_root','Native Agent is not QA A');
      verifiedOwnership = true;
      for(const [k,v] of Object.entries(c.expectedIdentity))
        if(stable(identity[k])!==stable(v)) fail('identity_mismatch',k);
      return {startup,identity};
    });
    if (phase==='fresh') {
      await step('fresh_official_fetch',async()=> {
        const catalog=await api('/v1/releases');
        if(catalog.current_release || catalog.releases?.length) fail('not_fresh','A already has a release');
        const accepted=await api('/v1/updates',{action:'switch',tag:TAG,source:'official',mode:'portable'});
        ownedOperation=accepted.operation?.operation_id;
        if(!ownedOperation) fail('operation_missing','Fetch response missing operation_id');
        const op=await wait('official rc2 prepared',async()=> {
          const x=(await api('/v1/updates')).operation;
          if(x?.operation_id!==ownedOperation) fail('operation_changed','Fetch operation changed');
          if(['failed','cancelled'].includes(x.phase)) fail('fetch_failed',JSON.stringify(safe(x)));
          return x.phase==='prepared'&&x;
        },15*60*1000);
        if(op.candidate_revision!==HEAD) fail('wrong_harness_sha',String(op.candidate_revision));
        selected=op.release_id;
        if(!/^[A-Za-z0-9_-]+$/.test(selected||'')) fail('release_id_invalid',String(selected));
        const slot=await ordinary(root,path.join(data,'releases',selected));
        if(await headAt(slot)!==HEAD) fail('checkout_sha_mismatch',slot);
        ownedOperation=null;
        return {operation:op.operation_id,release:selected,head:HEAD};
      });
      await step('normal_promote',async()=> {
        const inspect=await api('/v1/releases',{action:'promote',id:selected,inspect_only:true});
        const body={action:'promote',id:selected};
        if(inspect.rollback_confirmation) body.rollback_confirmation=inspect.rollback_confirmation;
        await api('/v1/releases',body);
        const catalog=await api('/v1/releases');
        if(catalog.current_release!==selected) fail('promotion_failed','Current release mismatch');
        return catalog;
      });
      await step('synthetic_files',async()=> {
        await stopHarness();
        if(Buffer.byteLength(SESSION)!==373||digest(SESSION)!==SESSION_HASH) fail('fixture_invalid','session');
        // rc2 materializes a checksummed header frame followed by an independent
        // checksummed event frame; its default canonical suffix is .jsonl.zstd.
        const split=SESSION.indexOf('\n')+1;
        const frames=[SESSION.slice(0,split),SESSION.slice(split)].map(text=>
          zstdCompressSync(text,{params:{[zlibConstants.ZSTD_c_checksumFlag]:1}}));
        if(frames.some(frame=>frame.readUInt32LE(0)!==0xfd2fb528||!(frame[4]&4))||
           digest(Buffer.concat(frames.map(frame=>zstdDecompressSync(frame))))!==SESSION_HASH)
          fail('fixture_invalid','checksummed two-frame zstd session');
        const sessionBytes=Buffer.concat(frames);
        await fs.mkdir(WORKSPACE,{recursive:true});await ordinary(root,WORKSPACE);
        const profile=path.join(home,'profiles/web'), plugin=path.join(profile,'node_modules/qa-round2-plugin');
        const dep=path.join(plugin,'node_modules/qa-round2-dependency');
        for(const d of [profile,plugin,dep,path.join(home,SESSION_DIRECTORY),path.join(home,'storages/qa-round2')]) {
          await fs.mkdir(d,{recursive:true}); await ordinary(root,d);
        }
        const manifestPath=path.join(profile,'package.json');
        let manifest;
        try {manifest=JSON.parse(await fs.readFile(manifestPath,'utf8'));}
        catch(e){if(e.code!=='ENOENT')throw e;manifest={name:'dsh-profile-web',private:true};}
        manifest.dependencies={...manifest.dependencies,'qa-round2-plugin':'1.0.0'};
        manifest.dsh={...manifest.dsh,profile:{...manifest.dsh?.profile,bundles:[
          ...new Set(['@deepseek-ai/dsh-base','@deepseek-ai/dsh-web-app',...(manifest.dsh?.profile?.bundles??[]),'qa-round2-plugin'])]}};
        const files={
          'profiles/web/package.json':JSON.stringify(manifest,null,2)+'\n',
          'profiles/web/cordis.patch.yml':'[{"id":"qa-offline-fixture","config":{"marker":"qa-round2-config"}}]\n',
          'profiles/web/node_modules/qa-round2-plugin/package.json':JSON.stringify({
            name:'qa-round2-plugin',version:'1.0.0',type:'module',main:'plugin.mjs',
            dependencies:{'qa-round2-dependency':'1.0.0'},dsh:{bundle:{patch:'./cordis.patch.yml'}}})+'\n',
          'profiles/web/node_modules/qa-round2-plugin/cordis.patch.yml':
            '- insert:\n    - id: qa-offline-fixture\n      name: ./plugin.mjs\n',
          'profiles/web/node_modules/qa-round2-plugin/plugin.mjs':
            "import leaf from 'qa-round2-dependency';\nexport const name='qa-round2-plugin';\n"+
            "export function apply(ctx,config){if(leaf!=='qa-round2-transitive-ok')throw Error('dependency mismatch');"+
            "if(config?.marker!=='qa-round2-config')throw Error('profile configuration mismatch');"+
            "const ready=ctx.get('appReady');if(!ready)throw Error('appReady missing');"+
            "ctx.effect(()=>ready.onReady(async()=>{const persistence=ctx.get('sessionPersistence');"+
            "if(!persistence)throw Error('sessionPersistence missing');const snapshots=await persistence.list();"+
            "const match=snapshots.filter(item=>item.header.id==='qa-valid-session');"+
            "if(match.length!==1||match[0].header.cwd!=='/qa/test/fixture-workspace')throw Error('fixture header missing from actual backend');"+
            "let handle;try{handle=await persistence.open('qa-valid-session','read');const loaded=await handle.read();"+
            "if(loaded.events.length!==1||loaded.events[0].type!=='user/message'||"+
            "loaded.events[0].data?.source?.kind!=='user'||loaded.events[0].data?.content?.[0]?.text!=="+JSON.stringify(TEXT)+")throw Error('actual backend message mismatch');"+
            "}finally{if(handle)await handle.close();}"+
            "process.stdout.write('DSH_QA_READ_OK:qa-valid-session\\nDSH_QA_ACTIVATED:'+leaf+'\\n');}),'qa fixture');}\n",
          'profiles/web/node_modules/qa-round2-plugin/node_modules/qa-round2-dependency/package.json':
            '{"name":"qa-round2-dependency","version":"1.0.0","main":"index.js"}\n',
          'profiles/web/node_modules/qa-round2-plugin/node_modules/qa-round2-dependency/index.js':
            'module.exports="qa-round2-transitive-ok";\n',
          [SESSION_DIRECTORY+'/session.v4.jsonl.zstd']:sessionBytes,
          'storages/qa-round2/sentinel.txt':'Synthetic migration storage sentinel.\n'
        };
        const fingerprints={};
        for(const [rel,bytes] of Object.entries(files)) {
          const file=path.join(home,rel);
          try {const s=await fs.lstat(file);if(!s.isFile()||s.isSymbolicLink())fail('fixture_path_invalid',rel);}
          catch(e){if(e.code!=='ENOENT')throw e;}
          await fs.writeFile(file,bytes); fingerprints[rel]=digest(bytes);
        }
        const config=await api('/v1/config');
        await api('/v1/config',{action:'set_harness_preferences',expected_revision:config.revision,
          harness_preferences:{...config.harness_preferences,home,open_browser:false}});
        await api('/v1/profiles',{action:'select',profile:'web'});
        await fs.writeFile(stateFile,JSON.stringify({fixtureVersion:4,release:selected,head:HEAD,home,workspace:WORKSPACE,files:fingerprints},null,2),{flag:'wx'});
        return {files:fingerprints,sessionPlaintextSha256:SESSION_HASH,sessionFrames:2,sessionChecksums:true,
          kind:'synthetic installed package tree and default rc2 zstd session; real Cordis activation checked separately'};
      });
    } else {
      await step('old_preserved_data',async()=> {
        const state=JSON.parse(await fs.readFile(stateFile,'utf8'));selected=state.release;
        if(state.fixtureVersion!==4||state.workspace!==WORKSPACE)fail('fixture_version_mismatch','Use v4 cwd/default-zstd fixture provenance; do not relabel a previous fixture');
        await ordinary(root,WORKSPACE);
        const catalog=await api('/v1/releases');
        if(catalog.current_release!==selected)fail('selection_changed','A release was not preserved');
        if(!/^[A-Za-z0-9_-]+$/.test(selected||''))fail('release_id_invalid',String(selected));
        const slot=await ordinary(root,path.join(data,'releases',selected));
        if(await headAt(slot)!==HEAD)fail('checkout_sha_mismatch','Preserved A checkout no longer matches frozen rc2');
        for(const [rel,h] of Object.entries(state.files))
          if(digest(await fs.readFile(path.join(home,rel)))!==h)fail('fixture_changed',rel);
        return {release:selected,files:Object.keys(state.files).length};
      });
    }
    await step('official_browser_session_and_plugin',verifyPage);
    await step('normal_stop',()=>stopHarness());
    if(phase==='final') {
      const saved=JSON.parse(await fs.readFile(stateFile,'utf8'));
      const contents={runtime:true,profiles:['web'],configuration:true,environment:true,
        sessions:true,plugins:true,credentials:false,credential_policy:'preserve'};
      const archive=path.join(root,'full-offline-'+Date.now()+'.tar.gz');
      const sourceSlot=await ordinary(root,path.join(sourceData,'releases',selected));
      const sourceCliHash=await fileHash(path.join(sourceSlot,'apps/cli/lib/bin.js'));
      await step('full_runtime_export',async()=> {
        const accepted=await api('/v1/updates',{action:'offline_export',release_id:selected,
          archive_path:archive,offline_contents:contents});
        ownedOperation=accepted.operation?.operation_id;
        if(!ownedOperation)fail('operation_missing','Export response missing operation_id');
        const op=await coldFinished(ownedOperation);ownedOperation=null;
        await ordinary(root,archive,false);
        const stat=await fs.stat(archive);
        return {operation:op.operation_id,archive,bytes:stat.size,sha256:await fileHash(archive),contents};
      });
      await step('launch_fresh_B',async()=> {
        const name='migration-B-'+Date.now();
        specB={executable:c.executable||'/opt/Nexus Launcher/nexus-launcher',
          dataRoot:path.join(root,name,'data'),home:path.join(root,name,'home'),
          dshHome:path.join(root,name,'dsh'),userDataDir:path.join(root,name,'electron')};
        await fs.mkdir(path.join(root,name),{recursive:false});
        for(const p of [specB.dataRoot,specB.home,specB.dshHome,specB.userDataDir]) {
          await fs.mkdir(p);await ordinary(root,p);
        }
        verifiedOwnership=false;
        instanceB=await c.launchInstance(specB);
        if(!instanceB?.desktop?.evaluate||!Number.isInteger(instanceB.proc?.pid))
          fail('contract_invalid','launchInstance must return connected desktop and owned proc.pid');
        desktop=instanceB.desktop;data=specB.dataRoot;home=specB.dshHome;
        const native=await verifyNative(), catalog=await api('/v1/releases');
        if(catalog.current_release||catalog.releases?.length)fail('B_not_fresh','B has pre-existing release');
        return {spec:specB,guiPid:instanceB.proc.pid,native,catalog};
      });
      await step('inspect_complete_archive',async()=> {
        const preview=await api('/v1/updates',{action:'offline_inspect',archive_path:archive});
        for(const key of ['runtime','configuration','environment','sessions','plugins'])
          if(preview.contents?.[key]!==true)fail('incomplete_archive',key);
        if(preview.contents.credentials!==false||!preview.contents.profiles?.includes('web'))
          fail('wrong_archive_selection','credentials/profile');
        if(!Number.isFinite(preview.bytes)||!Number.isFinite(preview.files))
          fail('inspect_contract','Archive preview lacks bytes/files');
        return preview;
      });
      let firstImport, firstBRelease;
      const verifyImported = async op => {
        const catalog=await api('/v1/releases'), config=await api('/v1/config');
        if(catalog.current_release!==op.release_id)fail('import_not_promoted','Current B selection mismatch');
        if(!/^[A-Za-z0-9_-]+$/.test(op.release_id||''))fail('release_id_invalid',String(op.release_id));
        const slot=await ordinary(root,path.join(data,'releases',op.release_id));
        if(await fileHash(path.join(slot,'apps/cli/lib/bin.js'))!==sourceCliHash)
          fail('imported_cli_changed','Imported CLI differs from frozen source');
        if(config.external_harness)fail('import_external_reference','B still uses external source');
        const runtimes=await ordinary(root,path.join(data,'runtimes'));
        for(const tool of ['node','pnpm','git']) {
          const p=config.runtime?.[tool]?.path;
          if(typeof p!=='string')fail('runtime_pin_missing',tool);
          await ordinary(runtimes,p,false);
        }
        if(typeof config.harness_preferences?.home!=='string')fail('import_home_missing','B data home');
        if(config.harness_preferences.open_browser!==false)fail('environment_preferences_changed','open_browser was not preserved');
        const expectedHome=path.join(path.dirname(data),path.basename(data)+'-environments',op.operation_id);
        home=await ordinary(root,config.harness_preferences.home);
        if(home!==await fs.realpath(expectedHome))
          fail('import_home_mismatch','B home is not this exact operation environment_root');
        for(const [rel,h] of Object.entries(saved.files)) {
          const file=path.join(home,rel);await ordinary(root,file,false);
          if(['profiles/web/package.json','profiles/web/cordis.patch.yml'].includes(rel)) {
            // The official exporter/importer normalizes JSON/YAML whitespace and paths.
            // Our bounded synthetic configuration is JSON-compatible and has no source-home paths.
            const original=path.join(sourceHome,rel);await ordinary(root,original,false);
            if(stable(JSON.parse(await fs.readFile(file,'utf8')))!==
               stable(JSON.parse(await fs.readFile(original,'utf8'))))fail('migration_config_changed',rel);
          } else if(await fileHash(file)!==h)fail('migration_content_changed',rel);
        }
        return {release:op.release_id,slot,home,runtime:config.runtime,sourceCliHash,
          files:Object.keys(saved.files).length};
      };
      await step('full_import_and_publication',async()=> {
        const id=await startImport(archive,contents);
        firstImport=await coldFinished(id);ownedOperation=null;
        firstBRelease=firstImport.release_id;
        return verifyImported(firstImport);
      });
      await step('B_real_session_and_plugin',verifyPage);
      migrationVerified=true;
      await step('B_stop_before_recovery',()=>stopHarness());
      await step('inflight_import_cancel',async()=> {
        const before=await api('/v1/releases'), id=await startImport(archive,contents);
        const progress=await wait('observable active extraction',async()=> {
          const state=await api('/v1/updates'), op=state.operation, p=state.offline_progress;
          if(op?.operation_id!==id)fail('operation_changed','Recovery import identity changed');
          if(['succeeded','failed','cancelled'].includes(op.phase))
            fail('INTERRUPTION_NOT_CAPTURED','Import ended before active extraction was observed');
          return p?.stage==='extract'&&p.completed>0&&p.total>p.completed&&
            op.owner_quiescent===false&&{operation:op,progress:p};
        },120000);
        // A second immediately preceding observation avoids counting a completed operation.
        const now=await api('/v1/updates');
        if(now.operation?.operation_id!==id||now.offline_progress?.stage!=='extract'||
           now.operation.owner_quiescent!==false||
           !(now.offline_progress.completed>0&&now.offline_progress.total>now.offline_progress.completed))
          fail('INTERRUPTION_NOT_CAPTURED','Extraction completed before Cancel admission');
        await record('inflight_before_cancel',progress);
        await api('/v1/updates',{action:'cancel',operation_id:id});
        const cancelled=await coldFinished(id,'cancelled');ownedOperation=null;
        const after=await api('/v1/releases');
        if(after.current_release!==before.current_release||after.current_release!==firstBRelease)
          fail('cancel_changed_selection','Cancelled candidate altered current release');
        return {operation:id,phase:cancelled.phase,owner_quiescent:cancelled.owner_quiescent,
          cleanup_pending:cancelled.cleanup_pending,current_release:after.current_release};
      });
      await step('normal_B_restart',async()=> {
        const before=await invoke('startup_status');
        const previous=instanceB;
        instanceB=await c.restartInstance({instance:previous,spec:specB});
        verifiedOwnership=false;
        if(!instanceB?.desktop?.evaluate||!Number.isInteger(instanceB.proc?.pid))
          fail('contract_invalid','restartInstance must return connected desktop and owned proc.pid');
        desktop=instanceB.desktop;
        const native=await verifyNative(), after=native.startup;
        if(after.instance_id===before.instance_id||after.agent_pid===before.agent_pid)
          fail('restart_not_observed','Need a new Agent instance/PID on the same B root');
        const catalog=await api('/v1/releases');
        if(catalog.current_release!==firstBRelease)fail('restart_selection_changed','B current release');
        return {before,after,catalog};
      });
      await step('retry_full_import_same_B',async()=> {
        const id=await startImport(archive,contents), op=await coldFinished(id);ownedOperation=null;
        return verifyImported(op);
      });
      await step('recovered_B_real_session_and_plugin',verifyPage);
      await step('recovered_B_stop',()=>stopHarness());
      await step('same_Agent_next_write',async()=> {
        const before=await invoke('startup_status'), config=await api('/v1/config');
        await api('/v1/config',{action:'set_harness_preferences',expected_revision:config.revision,
          harness_preferences:{...config.harness_preferences,open_browser:false}});
        const after=await invoke('startup_status');
        if(after.instance_id!==before.instance_id||after.agent_pid!==before.agent_pid)
          fail('agent_changed','Next write did not use the same recovered Agent');
        return {instance_id:after.instance_id,agent_pid:after.agent_pid};
      });
    }
    result={phase,status:'PASS',coverage:['native bridge','frozen rc2','real browser session','real Cordis apply'],
      packageTransactions:'EXTERNAL_NOT_COVERED',fullMigration:phase==='final'?'PASS':'NOT RUN',
      cancelRestartRecovery:phase==='final'?'PASS':'NOT RUN',crashRecovery:'NOT RUN',journal};
  } catch(e) {
    if(activePage?.page) {
      try {
        await record('failure_page',await evaluate(activePage.page,()=>({
          origin:location.origin,pathname:location.pathname,readyState:document.readyState,title:document.title,
          text:document.body?.innerText.slice(0,16000),
          rows:Array.from(document.querySelectorAll('[data-row-key]')).slice(0,60).map(row=>({
            key:row.getAttribute('data-row-key'),expanded:row.getAttribute('aria-expanded'),
            selected:row.getAttribute('aria-selected'),text:row.innerText.slice(0,300)})),
        }),null));
        const image=await activePage.page.cdp('Page.captureScreenshot',{format:'png'});
        const bytes=Buffer.from(image.data,'base64');
        if(bytes.length>8*1024*1024)fail('evidence_budget','active page screenshot');
        await fs.writeFile(path.join(out,stem+'-active-page.png'),bytes);
      } catch(diagnosticError) {
        await record('failure_page_error',{code:diagnosticError.code,message:diagnosticError.message});
      }
    }
    if (verifiedOwnership) {
      // Preserve the actual owned run before normal Stop changes its state.
      // Inspect only the synthetic profile and bounded tails of this Agent's logs.
      try {
        const snapshot = await api('/v1/harness',undefined,true);
        await record('failure_harness',snapshot);
        for (const stream of ['stdout','stderr']) {
          const name = snapshot['log_'+stream+'_name'];
          if (!/^[A-Za-z0-9._-]+$/.test(name||'')) fail('log_path_invalid',stream);
          const log = path.join(data,'logs',name);
          await ordinary(root,log,false);
          const handle = await fs.open(log,'r');
          try {
            const metadata = await handle.stat(), length = Math.min(metadata.size,65536);
            const bytes = Buffer.alloc(length);
            await handle.read(bytes,0,length,metadata.size-length);
            await fs.writeFile(path.join(out,stem+'-harness-'+stream+'.log'),safe(bytes.toString('utf8')));
            await record('failure_log',{stream,name,bytes:metadata.size,capturedBytes:length});
          } finally { await handle.close(); }
        }
        const manifestPath = path.join(home,'profiles/web/package.json');
        await ordinary(root,manifestPath,false);
        if ((await fs.stat(manifestPath)).size > 65536) fail('profile_budget','synthetic manifest');
        const manifest = JSON.parse(await fs.readFile(manifestPath,'utf8'));
        await record('failure_profile',{name:manifest.name,dsh:manifest.dsh,dependencies:manifest.dependencies});
      } catch (diagnosticError) {
        await record('failure_diagnostic_error',{code:diagnosticError.code,message:diagnosticError.message});
      }
    }
    const status=e.code==='INTERRUPTION_NOT_CAPTURED'?'NOT RUN':
      /^(contract_|BUDGET_|BROWSER_BLOCKED|DEADLINE|ABORTED)/.test(e.code||'')?'BLOCKED':'FAIL';
    await record(status,{code:e.code||'ERROR',message:e.message||String(e)});
    result={phase,status,stage,code:e.code||'ERROR',fullMigration:migrationVerified?'PASS':'NOT RUN',
      cancelRestartRecovery:'NOT RUN',crashRecovery:'NOT RUN',journal};
    throw Object.assign(e,{qaResult:result});
  } finally {
    if(verifiedOwnership) {
      if(ownedOperation) {
        try {
          try {await api('/v1/updates',{action:'cancel',operation_id:ownedOperation},true);}
          catch(e){await record('cleanup_cancel_response',{code:e.code,message:e.message});}
          const end=Date.now()+20000;let settled=null;
          while(Date.now()<end) {
            const op=(await api('/v1/updates',undefined,true)).operation;
            if(op?.operation_id===ownedOperation&&op.owner_quiescent===true&&!op.cleanup_pending&&
               ['prepared','succeeded','failed','cancelled'].includes(op.phase)){settled=op;break;}
            await delay(250);
          }
          if(!settled)fail('COLD_CLEANUP_TIMEOUT','Owned cold operation is not proven quiescent');
          await record('cold_cleanup',{operation:settled.operation_id,phase:settled.phase,
            owner_quiescent:settled.owner_quiescent,cleanup_pending:settled.cleanup_pending});
        } catch(e){cleanupOK=false;await record('cold_cleanup_failed',{code:e.code,message:e.message});}
      }
      try {
        await stopHarness(true);
        await record('cleanup',{harness:'stopped',processCleanup:'caller global finally owns Agent/GUI/browser'});
      } catch(e){cleanupOK=false;await record('harness_cleanup_failed',{code:e.code,message:e.message});}
    } else await record('cleanup',{harness:'not touched: QA ownership was not established'});
    const cleanupFailedAfterPass=!cleanupOK&&result?.status==='PASS';
    result={...result,cleanupOK,
      ...(cleanupFailedAfterPass?{status:'FAIL',code:'CLEANUP_FAILED'}:{}),
      cleanupScope:'Owned cold operation and Harness only; caller must prove Agent/GUI/browser cleanup'};
    await fs.writeFile(path.join(out,stem+'.result.json'),JSON.stringify(result,null,2));
    if(c.report&&typeof c.report==='object'&&!Array.isArray(c.report))c.report.businessQA=result;
    if(cleanupFailedAfterPass) fail('CLEANUP_FAILED','See cleanup evidence');
  }
  return result;
}
