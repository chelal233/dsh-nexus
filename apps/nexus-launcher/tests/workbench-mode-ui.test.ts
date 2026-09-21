import assert from 'node:assert/strict';
import test from 'node:test';
import { JSDOM } from 'jsdom';
import { createUiTestLoader } from './ui-test-loader.ts';

test('one Harness switches mode while idle and locks to the running mode', async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: 'http://localhost/' });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(k => [k, Object.getOwnPropertyDescriptor(globalThis, k)]));
  for (const [k,v] of Object.entries(bindings)) Object.defineProperty(globalThis,k,{configurable:true,writable:true,value:v});
  let phase = 'idle'; let launchPhase = 'preparing'; let stopFails = false; let supported = true; let profileOpens = 0; const calls = [];
  dom.window.nexusDesktop = { async invoke(command) { calls.push(command); if(command === 'harness_desktop_capability') return {supported,release:supported?'fixture':'web-only'}; if(command === 'harness_desktop_restart') { if(stopFails) throw Error('stop failed'); phase=launchPhase; } if(command === 'harness_desktop_start') phase=launchPhase; if(command === 'harness_desktop_stop') { if(stopFails) throw Error('stop failed'); phase='stopped'; } return {phase,stage:'runtime',startedAt:Date.now()-5000}; }, listen() { return () => {}; } };
  let root, loader, act;
  try {
    const React = await import('react'); act = React.act;
    const {createRoot} = await import('react-dom/client');
    loader = await createUiTestLoader(); const {OverviewView} = await loader.loadModule('/src/App.tsx');
    root = createRoot(document.getElementById('root'));
    const snapshot = {startup:{available:true},status:{running:true},health:{status:'ok'},state:{state:{lifecycle:'running'}},harnessRuntime:{harness:{state:'stopped'}},profiles:{active_profile:'web',profiles:['web']},checkpoints:{checkpoints:[]},config:{},releases:{current_release:'fixture'},harnessUi:null};
    const render = async () => act(async () => root.render(React.createElement(OverviewView,{snapshot,busyAction:null,openProfiles:()=>{profileOpens++;},runAction:async()=>true})));
    await render();
    const profileButton = () => document.querySelector('.workbench-profile-bar button');
    assert.match(document.querySelector('.workbench-profile-bar').textContent,/web/);
    await act(async()=>profileButton().click()); assert.equal(profileOpens,1);
    const primary = () => document.querySelectorAll('.harness-mode-content .button.primary');
    assert.equal(primary().length,1);
    assert.equal(document.querySelectorAll('.harness-mode-picker input').length,2);
    assert.equal(document.body.textContent.includes('Agent maintenance'),false);
    await act(async()=>document.querySelector('input[value="desktop"]').click());
    assert.equal(primary().length,1);
    assert.match(primary()[0].textContent,/Start Harness/);
    assert.equal(profileButton(),null);
    assert.match(document.querySelector('.workbench-profile-bar').textContent,/desktop profile/);
    snapshot.harnessRuntime.harness.state='running'; await render();
    assert.equal(document.querySelector('input[value="web"]').checked,true);
    assert.equal(document.querySelector('.harness-mode-picker').disabled,true);
    snapshot.harnessRuntime={harness:{state:'running',pid:20},generation:1,log_session_run_id:'run'};
    snapshot.harnessUi={available:true,url:'http://127.0.0.1:1234/',generation:1,run_id:'run',browser_health:{state:'checking'}};
    for(const state of ['checking','unverified','blocked','limited','active']) {
      snapshot.harnessUi.browser_health.state=state;await render();
      const open=[...document.querySelectorAll('button')].find(b=>b.textContent==='Open in system browser');
      assert.ok(open);assert.equal(open.disabled,state!=='active',state);
    }
    snapshot.harnessUi.run_id='old';await render();
    assert.equal([...document.querySelectorAll('button')].some(b=>b.textContent==='Open in system browser'),false);
    snapshot.harnessRuntime.harness.state='stopped';delete snapshot.harnessRuntime.harness.pid; await render();
    assert.equal(document.querySelector('input[value="desktop"]').checked,true);
    supported=false; snapshot.releases.current_release='web-only'; await render();
    assert.equal(document.querySelector('input[value="desktop"]'),null);
    assert.equal(document.querySelector('input[value="web"]').checked,true);
    supported=true; snapshot.releases.current_release='fixture'; await render();
    assert.equal(document.querySelector('input[value="desktop"]').checked,true);
    await act(async()=>primary()[0].click());
    assert.equal(calls.filter(c=>c==='harness_desktop_start').length,1);
    assert.equal(document.querySelector('.harness-mode-picker').disabled,true);
    assert.equal(primary().length,0);
    assert.match(document.querySelector('.desktop-phase').textContent,/Preparing/);
    assert.match(document.querySelector('.harness-desktop-content').textContent,/Preparing offline dependencies/);
    assert.match(document.querySelector('.harness-desktop-content').textContent,/Elapsed time:/);
    assert.equal(document.body.textContent.includes('Close the official window'),false);
    assert.equal(profileButton(),null);
    const cancel=[...document.querySelectorAll('.harness-desktop-content button')].find(button=>button.textContent==='Cancel startup');
    assert.ok(cancel); assert.equal(cancel.disabled,false);
    await act(async()=>cancel.click());
    assert.ok(calls.includes('harness_desktop_stop'));
    assert.equal(document.querySelector('.harness-mode-picker').disabled,false);
    launchPhase='launched';
    await act(async()=>primary()[0].click());
    const action=text=>[...document.querySelectorAll('.harness-desktop-content button')].find(button=>button.textContent===text);
    assert.ok(action('Close')); assert.ok(action('Restart'));
    const before=calls.length;
    await act(async()=>action('Restart').click());
    assert.deepEqual(calls.slice(before),['harness_desktop_restart']);
    stopFails=true;
    const failedBefore=calls.length;
    await act(async()=>action('Restart').click());
    assert.deepEqual(calls.slice(failedBefore),['harness_desktop_restart']);
    assert.match(document.body.textContent,/stop failed/);
    stopFails=false;
    await act(async()=>action('Close').click());
    assert.match(document.querySelector('.desktop-phase').textContent,/Not running/);
  } finally {
    if(root) await act(async()=>root.unmount()); if(loader) await loader.close(); dom.window.close();
    for(const [k,d] of previous) { if(d) Object.defineProperty(globalThis,k,d); else delete globalThis[k]; }
  }
});
