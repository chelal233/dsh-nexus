import assert from 'node:assert/strict';
import test from 'node:test';
import { JSDOM } from 'jsdom';
import { createUiTestLoader } from './ui-test-loader.ts';

test('one Harness switches mode while idle and locks to the running mode', async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: 'http://localhost/' });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(k => [k, Object.getOwnPropertyDescriptor(globalThis, k)]));
  for (const [k,v] of Object.entries(bindings)) Object.defineProperty(globalThis,k,{configurable:true,writable:true,value:v});
  let phase = 'idle'; let supported = true; let profileOpens = 0; const calls = [];
  dom.window.nexusDesktop = { async invoke(command) { calls.push(command); if(command === 'harness_desktop_capability') return {supported,release:supported?'fixture':'web-only'}; if(command === 'harness_desktop_start') phase='preparing'; if(command === 'harness_desktop_stop') phase='stopped'; return {phase,stage:'runtime',startedAt:Date.now()-5000}; }, listen() { return () => {}; } };
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
    assert.equal(profileButton().disabled,false);
    await act(async()=>profileButton().click()); assert.equal(profileOpens,2);
    snapshot.harnessRuntime.harness.state='running'; await render();
    assert.equal(document.querySelector('input[value="web"]').checked,true);
    assert.equal(document.querySelector('.harness-mode-picker').disabled,true);
    snapshot.harnessRuntime.harness.state='stopped'; await render();
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
    assert.equal(profileButton().disabled,true);
    const cancel=[...document.querySelectorAll('.harness-desktop-content button')].find(button=>button.textContent==='Cancel startup');
    assert.ok(cancel); assert.equal(cancel.disabled,false);
    await act(async()=>cancel.click());
    assert.ok(calls.includes('harness_desktop_stop'));
    assert.equal(document.querySelector('.harness-mode-picker').disabled,false);
  } finally {
    if(root) await act(async()=>root.unmount()); if(loader) await loader.close(); dom.window.close();
    for(const [k,d] of previous) { if(d) Object.defineProperty(globalThis,k,d); else delete globalThis[k]; }
  }
});
