import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";
import { createUiTestLoader } from "./ui-test-loader.ts";

async function fixture(run: (context: any) => Promise<void>) {
  const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator,
    HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, Event: dom.window.Event, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const root = createRoot(document.getElementById("root")!);
  mockIPC(() => null);
  const loader = await createUiTestLoader();
  const click = async (label: string) => {
    const button = [...document.querySelectorAll("button")].find(button => button.textContent?.trim() === label);
    assert.ok(button, `Missing button: ${label}`);
    await React.act(async () => button.click());
  };
  try { await run({ React, root, loader, click }); }
  finally {
    await React.act(async () => root.unmount()); clearMocks(); await loader.close(); dom.window.close();
    for (const [key, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete (globalThis as any)[key];
    }
  }
}

test("startup timing panel shows measured stages without claiming client readiness", async () => fixture(async ({React, root, loader}) => {
  mockIPC((command, args) => {
    assert.equal(command, "proxy_request"); assert.equal(args.path, "/v1/harness/startup");
    return {operation_id:"current",phase:"compatibility",cancellable:true,stage_durations_ms:{checking:1250,compatibility:2400,spawning:-1,unknown:300}};
  });
  const { StartupOperationPanel } = await loader.loadModule("/src/App.tsx");
  await React.act(async () => root.render(React.createElement(StartupOperationPanel,{available:true,identity:"agent"})));
  assert.match(document.body.textContent!, /1.3 seconds/);
  assert.match(document.body.textContent!, /2.4 seconds/);
  assert.match(document.body.textContent!, /not client readiness/);
  assert.doesNotMatch(document.body.textContent!, /-0.0|unknown/);
}));

test("Desktop failures share diagnosis and navigate without changing the Web profile", async () => fixture(async ({React, root, loader, click}) => {
  const { HarnessDesktopPanel } = await loader.loadModule("/src/App.tsx");
  const repairs: string[] = [];
  const render = (state: any) => React.act(async () => root.render(React.createElement(HarnessDesktopPanel, {
    snapshot:{startup:{available:true},harnessRuntime:{harness:{state:"stopped"}}},busy:false,onRepair:(id:string)=>repairs.push(id),
    controller:{state,starting:false,stopping:false,active:state.phase==="launched",launch:()=>{},stop:()=>{},restart:()=>{}},
  })));
  await render({phase:"launched",audit:{state:"failed",error:"Cannot find package 'resolve.exports' imported from /Harness/profile.ts"}});
  assert.match(document.body.textContent!, /Required module is missing/);
  assert.match(document.body.textContent!, /not the selected Web profile/);
  await click("Inspect local dependencies"); assert.deepEqual(repairs,["installation"]);
  await click("Go to profile management"); assert.deepEqual(repairs,["installation","profile"]);
  await render({phase:"failed",detail:"Plugins waiting for services: workspaceRegistry"});
  assert.match(document.body.textContent!, /repair the provider rather than disabling the waiting consumer/);
  assert.match(document.body.textContent!, /workspaceRegistry/);
  await render({phase:"launched",audit:{state:"ready",error:"Cannot find package 'old'"}});
  assert.doesNotMatch(document.body.textContent!, /Required module is missing|Inspect local dependencies/);
}));

test("update dialog keeps notes, download consent, progress, retry and restart separate", async () => fixture(async ({React, root, loader, click}) => {
  const calls: any[] = []; let closed = 0;
  mockIPC((command, args) => { calls.push({command, args}); return {}; });
  const { DesktopUpdateDialog } = await loader.loadModule("/src/App.tsx");
  const render = (phase: string, extra = {}, stopped = true) => React.act(async () => root.render(React.createElement(DesktopUpdateDialog,
    {state: {enabled:true, phase, version:"0.2.0", ...extra}, needsHarnessStop:!stopped, onClose:()=>closed++})));
  await render("available");
  assert.match(document.body.textContent!, /0.2.0/); assert.equal(calls.length, 0);
  await click("View release notes"); assert.equal(calls.at(-1).command, "update_release_notes");
  await click("Confirm and download"); assert.deepEqual(calls.at(-1), {command:"update_download", args:{version:"0.2.0"}});
  await render("downloading", {percent:43}); assert.equal(document.querySelector("progress")?.value, 43);
  await click("Close"); assert.equal(closed, 1); assert.equal(calls.length, 2);
  await render("error", {error:"network unavailable"}); await click("Check again"); assert.equal(calls.at(-1).command, "update_check");
  await render("ready", {}, false);
  const install = [...document.querySelectorAll("button")].find(button => button.textContent === "Update and restart")!;
  assert.equal(install.disabled, true);
  await click("Restart later"); assert.equal(closed, 2); assert.equal(calls.length, 3);
  await render("ready"); await click("Update and restart"); assert.equal(calls.at(-1).command, "update_install");
}));

test("dependency repair requires a fresh preview and explicit confirmation and shows verification boundaries", async () => fixture(async ({React, root, loader, click}) => {
  const posts: any[] = []; let openCount = 0;
  const plan = {fingerprint:"fresh",root:"C:/release",entries:[{package:"example",importer:".",destination:"C:/release/node_modules/example",reason:"missing_link"}]};
  mockIPC((command, args) => {
    assert.equal(command, "proxy_request"); assert.equal(args.path, "/v1/dependencies");
    if (args.method === "POST") { posts.push(args.body); return {phase:"repaired",record:"C:/data/repair-record",preview:{...plan,entries:[]},startup_verified:false}; }
    return plan;
  });
  const { DependencyRepairPanel, ConfirmationHost } = await loader.loadModule("/src/App.tsx");
  const render = (release: string) => React.act(async () => root.render(React.createElement(React.Fragment, null,
    React.createElement(DependencyRepairPanel, {snapshot:{startup:{available:true}, releases:{current_release:release}, config:{}},busyAction:null,openWorkbench:()=>openCount++}),
    React.createElement(ConfirmationHost))));
  await render("one"); await click("Inspect local dependencies"); assert.match(document.body.textContent!, /example/);
  await render("two"); assert.doesNotMatch(document.body.textContent!, /example/);
  await click("Inspect local dependencies"); await click("Restore missing links"); assert.equal(posts.length, 0);
  await click("Confirm"); assert.deepEqual(posts, [{fingerprint:"fresh"}]);
  assert.match(document.body.textContent!, /Start the affected Harness mode/);
  assert.match(document.body.textContent!, /C:\/data\/repair-record/);
  await click("Workbench"); assert.equal(openCount, 1);
}));
