import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";

test("offline repair exposes a damaged non-current profile without launching plugins", async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator, HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, Event: dom.window.Event, CustomEvent: dom.window.CustomEvent, IS_REACT_ACT_ENVIRONMENT: true };
  const old = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  const oldTZ = process.env.TZ; process.env.TZ = "Asia/Shanghai";
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { value, configurable: true, writable: true });
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const root = createRoot(document.getElementById("root")!);
  mockIPC(() => ({}));
  const loader = await createUiTestLoader();
  try {
    const {OfflineProfileRepair}=await loader.loadModule("/src/App.tsx");
    const {I18nProvider}=await loader.loadModule("/src/i18n.ts");
    const calls:any[]=[];
    mockIPC((_command,args:any)=>{
      const body=typeof args.body==="string"?JSON.parse(args.body):args.body; calls.push({path:args.path,...body});
      if(body.action==="list") return {profiles:["broken"]};
      return {profile:"broken",files:[{file:"package.json",content:"{broken",fingerprint:"test",error:"expected JSON at line 1"},{file:"cordis.patch.yml",content:null,fingerprint:"none"}],backups:[],startup_verified:false};
    });
    const props={snapshot:{profiles:{active_profile:"web",official_plugin_management:true},recovery:{harness:{state:"stopped"}}},busyAction:null,refresh:async()=>{}};
    await React.act(async()=>root.render(React.createElement(I18nProvider,{initialLocale:"en"},React.createElement(OfflineProfileRepair,props))));
    assert.ok(document.body.textContent?.includes("broken"));
    assert.ok(document.body.textContent?.includes("expected JSON at line 1"));
    assert.equal(document.querySelector('textarea')?.value,"{broken");
    assert.ok(document.body.textContent?.includes("Back up, save and check"));
    assert.ok(calls.some(x=>x.action==="inspect"&&x.profile==="broken"));
    assert.ok(calls.every(x=>x.path==="/v1/profile-repair"));
    await React.act(async()=>[...document.querySelectorAll<HTMLButtonElement>('button')].find(x=>x.textContent==="Check configuration again")!.click());
    assert.equal(calls.filter(x=>x.action==="inspect").length,2);
  } finally {
    await React.act(async () => root.unmount());
    await loader.close(); clearMocks(); dom.window.close();
    if (oldTZ === undefined) delete process.env.TZ; else process.env.TZ = oldTZ;
    for (const [key, descriptor] of old) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete (globalThis as any)[key]; }
  }
});
