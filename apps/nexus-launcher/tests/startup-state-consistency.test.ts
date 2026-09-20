import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("client verification follows the current run and uses bounded foreground polling", async () => {
  const loader = await createUiTestLoader();
  try {
    const { clientCheckPending, launcherPollDelay, clientStartupLabel, OverviewView } = await loader.loadModule("/src/App.tsx");
    const snapshot = { harnessRuntime: { harness: { state: "running", pid: 42 }, generation: 1, log_session_run_id: "new" },
      harnessUi: { available: true, generation: 1, run_id: "old", browser_health: { state: "active" } },
      releases: { current_release: "release-b" }, profiles: { api_version: "v1", active_profile: "web", disabled_plugins: [], compatibility: { checked_disabled_plugins: [], effective_profile: "web", release_id: "release-a", diagnosis: { level: "limited" } } } };
    assert.equal(clientCheckPending(snapshot), true, "A previous run must not end verification");
    snapshot.harnessUi.run_id = "new";
    snapshot.harnessUi.browser_health.state = "checking";
    assert.equal(clientCheckPending(snapshot), true);
    snapshot.harnessUi.browser_health.state = "active";
    assert.equal(clientCheckPending(snapshot), false);
    assert.equal(clientStartupLabel(snapshot, (key: string) => key, true), "Ready");
    snapshot.profiles.compatibility.release_id = "release-b";
    assert.equal(clientStartupLabel(snapshot, (key: string) => key, true), "Ready with warnings");
    snapshot.profiles.compatibility.diagnosis.activation = {entries:[{package:'dsh-automation',reason:'webServer is unavailable'}]};
    const warning = renderToStaticMarkup(createElement(OverviewView, {snapshot:{...snapshot,startup:{available:true},status:{},state:{}},busyAction:null,openProfiles:()=>{},runAction:async()=>true}));
    assert.match(warning,/dsh-automation/);
    assert.match(warning,/webServer is unavailable/);
    assert.match(warning,/Service and plugin details/);
    snapshot.profiles.disabled_plugins.push("changed-policy");
    assert.equal(clientStartupLabel(snapshot, (key: string) => key, true), "Ready");
    assert.equal(launcherPollDelay("client_pending", 0, false), 1000);
    assert.equal(launcherPollDelay("client_pending", 45, false), 8000);
    assert.equal(launcherPollDelay("client_pending", 0, true), 8000);
    assert.equal(launcherPollDelay("running", 0, false), 8000);
    const markup = renderToStaticMarkup(createElement(OverviewView, {
      snapshot: { ...snapshot, startup: { available: true }, status: {}, state: {},
        harnessRuntime: { harness: { state: "stopped" } }, harnessUi: { message: "Harness is Stopped; a current authentication token is not available" },
        checkpoints: { checkpoints: [{ created_at_unix: 1789876000 }] }, updates: { update: { state: "prepared" } } },
      busyAction: null, credentialInvalidationPending: false, runAction: async () => true,
    }));
    assert.match(markup, /Latest checkpoint:/);
    assert.doesNotMatch(markup, /Prepared, awaiting|Harness is Stopped/);
    assert.match(markup, /Start Harness/);
    assert.doesNotMatch(markup, /Authentication metadata|No token observed/, "Stopped mode omits empty authentication diagnostics");
  } finally { await loader.close(); }
});
