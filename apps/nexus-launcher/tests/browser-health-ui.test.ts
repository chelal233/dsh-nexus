import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("browser failure remains visible while the host runs and groups affected consumers", async () => {
  const loader = await createUiTestLoader();
  try {
    const { BrowserHealth } = await loader.loadModule("/src/App.tsx");
    const snapshot = { harnessRuntime: { harness: { state: "running", pid: 42 }, generation: 1, log_session_run_id: "run-a" }, harnessUi: { available: true, generation: 1, run_id: "run-a", browser_health: {
      state: "blocked", missing_core: ["sessions"], entries: [
        { name: "session-controller", state: "pending", missing: ["fileUpload"] },
        { name: "chat", state: "pending", missing: ["sessions"] },
        { name: "archive", state: "pending", missing: ["sessions"] },
      ],
    } } };
    const render = () => renderToStaticMarkup(createElement(BrowserHealth, { snapshot, runAction: async () => true, openProfiles: () => {} }));
    const markup = render();
    assert.match(markup, /Client startup failed/);
    assert.match(markup, /<li><code>chat<\/code><\/li>/);
    assert.match(markup, /<li><code>archive<\/code><\/li>/);
    assert.match(markup, /Go to profile management/);
    assert.match(markup, /Retry Harness startup/);
    assert.match(markup, /sessions · Waiting plugins: 2/);
    assert.match(markup, /fileUpload/);
    assert.match(markup, /not confirmed faulty/);
    assert.match(markup, /Stop, profile selection and dependency repair remain available/);
    snapshot.harnessUi.browser_health = { state: "unverified", missing_core: [], entries: [] };
    const unknown = renderToStaticMarkup(createElement(BrowserHealth, { snapshot }));
    assert.match(unknown, /Startup not yet verified/);
    assert.doesNotMatch(unknown, /Startup checks passed/);
    snapshot.harnessUi.browser_health.state = "checking";
    assert.match(render(), /Checking client plugins/);
    snapshot.harnessUi.browser_health.state = "limited";
    assert.match(render(), /required core services are missing/);
    snapshot.harnessUi.browser_health.state = "active";
    assert.match(render(), /Startup checks passed/);
    Object.assign(snapshot, { profiles: { api_version: "v1", disabled_plugins: [], active_profile: "web", compatibility: { checked_disabled_plugins: [], effective_profile: "web", diagnosis: { level: "limited" } } } });
    assert.match(render(), /Startup checks found limited functionality/);
    assert.doesNotMatch(render(), /Recommended recovery|Recheck and prepare repair/);
    snapshot.harnessRuntime.log_session_run_id = "run-b";
    assert.doesNotMatch(render(), /Startup checks passed/);
    assert.match(render(), /Startup not yet verified/);
  } finally { await loader.close(); }
});

test("the startup check dialog includes the current client failure and repair entry", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CompatibilityDialog } = await loader.loadModule("/src/App.tsx");
    const markup = renderToStaticMarkup(createElement(CompatibilityDialog, {
      snapshot: { harnessRuntime: { harness: { state: "running", pid: 42 }, generation: 1, log_session_run_id: "run" },
        harnessUi: { available: true, generation: 1, run_id: "run", browser_health: { state: "blocked", entries: [{ name: "failed-package", state: "import_failed", missing: [] }] } },
        startup: { available: true }, profiles: {}, recovery: {} },
      busyAction: null, pending: false, onClose: () => {}, onRepair: () => {}, runAction: async () => true,
    }));
    assert.match(markup, /Client startup failed/);
    assert.match(markup, /failed-package/);
    assert.match(markup, /Open the startup log/);
  } finally { await loader.close(); }
});

test("current host startup warnings remain visible with an active browser and never reuse old logs", async () => {
 const loader = await createUiTestLoader();
 try {
  const { BrowserHealth } = await loader.loadModule("/src/App.tsx");
  const runtime = {state:"running",pid:42,started_at_unix:100};
  const snapshot = {harnessRuntime:{harness:runtime,generation:1,log_session_run_id:"a"},
   harnessUi:{available:true,generation:1,run_id:"a",browser_health:{state:"active",entries:[],missing_core:[]}},
   recovery:{harness:runtime,log_tail:[{content:"dsh: warning: 1 entry did not activate\nui-task-board (addon): Error: task-board ledger is already owned by process 29864"}]}};
  const render=()=>renderToStaticMarkup(createElement(BrowserHealth,{snapshot}));
  assert.match(render(),/Startup checks found limited functionality/);
  assert.doesNotMatch(render(),/Client startup failed|Recommended recovery/);
  snapshot.recovery.harness={...runtime,pid:41};
  assert.match(render(),/Startup checks passed/);
 } finally {await loader.close();}
});

test("native skipped bundles keep an active client limited without trusting a stale process or unverified browser", async () => {
  const loader = await createUiTestLoader();
  try {
    const { BrowserHealth } = await loader.loadModule("/src/App.tsx");
    const runtime = {state:"running",pid:42,started_at_unix:100};
    const snapshot = {harnessRuntime:{harness:runtime,generation:1,log_session_run_id:"a"},
      harnessUi:{available:true,generation:1,run_id:"a",browser_health:{state:"active",entries:[],missing_core:[]}},
      recovery:{harness:runtime,log_tail:[{content:'dsh: skipping profile bundle "legacy-addon": export removed'}]}};
    const render = () => renderToStaticMarkup(createElement(BrowserHealth,{snapshot}));
    assert.match(render(), /Startup checks found limited functionality/);
    assert.match(render(), /legacy-addon<\/strong>: export removed/);
    assert.doesNotMatch(render(), /Client startup failed|Recommended recovery/);
    snapshot.harnessUi.browser_health.state="checking";
    assert.match(render(), /Checking client plugins/);
    assert.doesNotMatch(render(), /Startup checks passed/);
    snapshot.recovery.harness={...runtime,pid:41};
    assert.doesNotMatch(render(), /legacy-addon/);
    snapshot.harnessUi.browser_health.state="active";
    assert.match(render(), /Startup checks passed/);
  } finally { await loader.close(); }
});

test("committed skips survive log growth and remain bound to the current UI generation", async () => {
  const loader = await createUiTestLoader();
  try {
    const { BrowserHealth } = await loader.loadModule("/src/App.tsx");
    const runtime = { state: "running", pid: 42, started_at_unix: 100 };
    const snapshot = { harnessRuntime: { harness: runtime, generation: 2, log_session_run_id: "new-run" },
      harnessUi: { available: true, generation: 2, run_id: "new-run", browser_health: { state: "active", entries: [],
        host_skipped_bundles: { entries: [{ package: "legacy-addon", reason: "export removed" }], truncated: false } } },
      recovery: { harness: runtime, log_tail: [{ content: "only later output remains" }] } };
    const render = () => renderToStaticMarkup(createElement(BrowserHealth, { snapshot }));
    assert.match(render(), /Startup checks found limited functionality/);
    assert.match(render(), /legacy-addon<\/strong>: export removed/);
    snapshot.harnessUi.browser_health.state = "checking";
    assert.match(render(), /Checking client plugins/);
    assert.doesNotMatch(render(), /Startup checks passed/);
    snapshot.harnessUi.browser_health.state = "active";
    snapshot.harnessUi.run_id = "old-run";
    assert.doesNotMatch(render(), /legacy-addon|Startup checks passed/);
    snapshot.harnessUi.run_id = "new-run";
    snapshot.harnessUi.generation = 1;
    assert.doesNotMatch(render(), /legacy-addon|Startup checks passed/);
    snapshot.harnessUi.generation = 2;
    snapshot.harnessUi.browser_health.host_skipped_bundles.entries = [];
    snapshot.recovery.log_tail = [{ content: 'dsh: skipping profile bundle "late-background": after readiness' }];
    assert.match(render(), /Startup checks passed/);
    assert.doesNotMatch(render(), /late-background/);
  } finally { await loader.close(); }
});
