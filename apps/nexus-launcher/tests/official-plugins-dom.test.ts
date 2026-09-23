import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";

test("official plugin controls honor upstream protection and operate while Harness is stopped", async () => {
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
    const {OfficialPlugins}=await loader.loadModule("/src/App.tsx");
    const {I18nProvider}=await loader.loadModule("/src/i18n.ts");
    const calls:any[]=[];
    const bundles=[{name:"official-optional",optional:true,installed:false,enabled:false,removable:false,rows:[]},{name:"third-party",version:"1.2.3",repository:"git+https://github.com/example/plugin.git",meta:{error:"Invalid plugin icon"},optional:false,installed:true,enabled:true,removable:true,rows:[]},{name:"protected",optional:false,installed:true,enabled:true,removable:false,readOnlyReason:"management-required",rows:[]}];
    mockIPC((_command,args:any)=>{const body=typeof args.body==="string"?JSON.parse(args.body):args.body;calls.push(body);return {profile:"web",bundles,result:body?.action==="disable"?{application:"restart-required"}:null};});
    const props={profile:"web",snapshot:{profiles:{active_profile:"desktop"},recovery:{harness:{state:"stopped"}}},busyAction:null,refresh:async()=>{}};
    await React.act(async()=>root.render(React.createElement(I18nProvider,{initialLocale:"en"},React.createElement(OfficialPlugins,props))));
    assert.ok(document.body.textContent?.includes("v1.2.3"));
    assert.ok(document.body.textContent?.includes("Metadata warning"));
    assert.equal(document.querySelector('a[aria-label="third-party GitHub"]')?.getAttribute('href'), "https://github.com/example/plugin");
    await React.act(async()=>[...document.querySelectorAll<HTMLButtonElement>('button.official-plugin-title')].find(x=>x.textContent?.includes("third-party"))!.click());
    assert.ok(document.body.textContent?.includes("Invalid plugin icon"));
    assert.ok([...document.querySelectorAll('button')].some(x=>x.textContent==="Remove"));
    await React.act(async()=>document.querySelector<HTMLButtonElement>('[aria-label="Close"]')?.click());
    assert.ok(document.body.textContent?.includes("Current selection remains unchanged"));
    assert.ok(calls.every(x=>x?.profile==="web"));
    const switches=[...document.querySelectorAll<HTMLButtonElement>('.official-plugin-row [role="switch"]')];
    assert.equal(switches.length,3);assert.equal(switches.find(x=>x.getAttribute("aria-label")==="protected")!.disabled,true);
    await React.act(async()=>switches.find(x=>x.getAttribute("aria-label")==="third-party")!.click());
    assert.ok(document.body.textContent?.includes("next time Harness starts"));
    assert.ok(calls.some(x=>x?.action==="disable"&&x.package==="third-party"));
    await React.act(async()=>root.render(React.createElement(I18nProvider,{initialLocale:"en"},React.createElement(OfficialPlugins,{...props,snapshot:{...props.snapshot,recovery:{harness:{state:"running"}}}}))));
    assert.ok([...document.querySelectorAll<HTMLButtonElement>('.official-plugin-row [role="switch"]')].every(x=>x.disabled));
  } finally {
    await React.act(async () => root.unmount());
    await loader.close(); clearMocks(); dom.window.close();
    if (oldTZ === undefined) delete process.env.TZ; else process.env.TZ = oldTZ;
    for (const [key, descriptor] of old) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete (globalThis as any)[key]; }
  }
});
