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
