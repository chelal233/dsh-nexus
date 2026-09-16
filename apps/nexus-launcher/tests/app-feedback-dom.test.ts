import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";
import { emit } from "./desktop-mocks.ts";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("App aggregates completions, repeats errors, and serializes same-turn actions", async () => {
  const dom=new JSDOM('<!doctype html><div id="root"></div>',{url:"http://localhost/"});
  dom.window.matchMedia=()=>({matches:false,addEventListener(){},removeEventListener(){}});
  dom.window.scrollTo=()=>{};dom.window.HTMLElement.prototype.scrollIntoView=()=>{};
  const bindings={window:dom.window,document:dom.window.document,navigator:dom.window.navigator,HTMLElement:dom.window.HTMLElement,Node:dom.window.Node,Event:dom.window.Event,CustomEvent:dom.window.CustomEvent,IS_REACT_ACT_ENVIRONMENT:true};
  const previous=new Map(Object.keys(bindings).map(key=>[key,Object.getOwnPropertyDescriptor(globalThis,key)]));
  for(const [key,value] of Object.entries(bindings)) Object.defineProperty(globalThis,key,{configurable:true,writable:true,value});
  let root,loader,act;
  const errors=[];const oldError=console.error;console.error=(...args)=>{errors.push(args.join(" "));oldError(...args);};
  try {
    const react=await import("react");act=react.act;const {createRoot}=await import("react-dom/client");
    const fixtures={
      "/v1/health":{status:"ok",degraded:false},"/v1/state":{state:"running"},
      "/v1/harness":{state:"stopped"},"/v1/profiles":{active_profile:"web",profiles:["web"]},
      "/v1/releases":{current_release:"slot",releases:[{id:"slot",version:"v1"}]},
      "/v1/config":{},"/v1/updates":{operation:{operation_id:"export",kind:"offline_export",phase:"verifying",release_id:"slot",archive_path:"C:/fixture.tar.gz"}},
      "/v1/checkpoints":{last_capture:{id:"snapshot",state:"running"}},
    };
    let postCount=0,releasePost;
    mockIPC((command,payload)=>{
      if(command==="startup_status") return {available:true,running:true};
      if(command==="proxy_request") {
        if(payload.method==="GET") return structuredClone(fixtures[payload.path]??{});
        assert.equal(payload.path,"/v1/profiles");assert.equal(payload.body.action,"open_terminal");postCount++;
        return new Promise(resolve=>{releasePost=()=>resolve({});});
      }
      return null;
    },{shouldMockEvents:true});
    loader=await createUiTestLoader();const {App}=await loader.loadModule("/src/App.tsx");
    root=createRoot(document.getElementById("root"));await act(async()=>root.render(react.createElement(App)));
    fixtures["/v1/updates"].operation.phase="succeeded";
    fixtures["/v1/checkpoints"].last_capture.state="failed";
    await act(async()=>document.querySelector('[aria-label="Refresh launcher status"]').click());
    const notice=document.querySelector(".toast-warning");assert.ok(notice);
    assert.match(notice.textContent,/Offline package export.*Completed/s);assert.match(notice.textContent,/Snapshot capture.*Failed/s);
    await act(async()=>emit("nexus-native-error","Repeated native failure"));
    let alert=document.querySelector('.toast-error');assert.ok(alert);
    await act(async()=>alert.querySelector('[aria-label="Dismiss notice"]').click());
    assert.equal(document.querySelector('.toast-error'),null);
    await act(async()=>emit("nexus-native-error","Repeated native failure"));
    assert.ok(document.querySelector('.toast-error'));assert.ok(document.querySelector('.toast-warning'));
    assert.equal(errors.filter(message=>/same key|unique.*key/i.test(message)).length,0);
    await act(async()=>{await emit("nexus-tray-action","terminal");await emit("nexus-tray-action","terminal");});
    assert.equal(postCount,1,"one pending action owns the synchronous gate");
    await act(async()=>releasePost());
    await act(async()=>emit("nexus-tray-action","terminal"));assert.equal(postCount,2,"completion releases the gate");
    await act(async()=>releasePost());
    const {localeFromLanguage}=await loader.loadModule("/src/i18n.ts");
    for(const language of ["zh-TW","zh-HK","zh-Hant","zh_MO","zh-yue","zh-cmn","zh-XX","yue-HK"]) assert.equal(localeFromLanguage(language),"en");
    for(const language of ["zh-CN","zh-SG","zh-Hans","zh"]) assert.equal(localeFromLanguage(language),"zh");
    const {UpdatesView}=await loader.loadModule("/src/App.tsx");
    const {renderToStaticMarkup}=await import("react-dom/server");
    for(const actionPending of [false,true]) {
      const html=renderToStaticMarkup(react.createElement(UpdatesView,{
        snapshot:{startup:{available:true},health:{},config:{},releases:{},profiles:{},updates:{operation:{operation_id:"cancel-target",phase:"verifying"}},lifecycleBusy:true},
        busyAction:"Operation in progress",actionPending,runAction:async()=>true,refresh:async()=>{},
      }));
      const cancel=html.match(/<button[^>]*>Cancel<\/button>/)?.[0];assert.ok(cancel);
      assert.equal(cancel.includes("disabled"),actionPending,"ongoing Agent operation permits cancel; pending local request disables it");
    }
  } finally {
    if(root) await act(async()=>root.unmount());if(loader)await loader.close();
    clearMocks();dom.window.close();console.error=oldError;
    for(const [key,descriptor] of previous) {if(descriptor)Object.defineProperty(globalThis,key,descriptor);else delete globalThis[key];}
  }
});
