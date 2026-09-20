import test from "node:test";
import assert from "node:assert/strict";
import { createUiTestLoader } from "./ui-test-loader.ts";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";

test("repair names only evidenced plugins and executes stop, disable and gated start in order", async () => {
  const loader = await createUiTestLoader();
  try {
    const { StartupRepair, startupRepairPlan, executeStartupRepair } = await loader.loadModule("/src/App.tsx");
    const snapshot = { startup: { available: true }, harnessRuntime: { harness: { state: "running", pid: 42 } },
      profiles: { api_version: "v1", active_profile: "web", disabled_plugins: [], manifests: [{ name: "web", bundles: ["archive", "upload", "consumer", "@deepseek-ai/core"] }],
        compatibility: { status: "needs_choice", source_profile: "web", checked_disabled_plugins: [], diagnosis: { repair_candidates: [
          { package: "archive", evidence: "activation_failure", reason: "codec failed" },
          { package: "upload", evidence: "replaces_official_entry", reason: "replaces built-in uploader" },
          { package: "consumer", evidence: "waiting_for_services", reason: "waiting" },
          { package: "@deepseek-ai/core", evidence: "activation_failure" },
        ] } } } };
    assert.deepEqual(startupRepairPlan(snapshot).map(row => row.package), ["archive", "upload"]);
    snapshot.profiles.compatibility.diagnosis.repair_candidates.push({ package: "archive", evidence: "activation_failure", reason: "duplicate" });
    assert.equal(startupRepairPlan(snapshot).length, 2);
    assert.deepEqual(startupRepairPlan({ ...snapshot, releases: { current_release: "new-release" } }), []);
    snapshot.profiles.compatibility.status = "passed";
    assert.deepEqual(startupRepairPlan(snapshot), []);
    snapshot.profiles.compatibility.status = "needs_choice";
    const calls: unknown[] = [];
    assert.equal(await executeStartupRepair(snapshot, async (_label, endpoint, body) => { calls.push([endpoint, body]); return true; }, "Repair"), true);
    assert.deepEqual(calls, [
      ["/v1/harness", { action: "stop" }],
      ["/v1/profiles", { action: "plugin_disable", profile: "web", package: "archive" }],
      ["/v1/profiles", { action: "plugin_disable", profile: "web", package: "upload" }],
      ["/v1/harness", { action: "start" }],
    ]);
    let attempts = 0;
    assert.equal(await executeStartupRepair(snapshot, async () => ++attempts < 2, "Repair"), false);
    assert.equal(attempts, 2, "No further changes or start after a failed disable");
    const markup = renderToStaticMarkup(createElement(StartupRepair, { snapshot, busyAction: null, runAction: async () => true }));
    assert.match(markup, /Disable recommended plugins and retry/);
    assert.match(markup, /<code>archive<\/code>/);
    assert.doesNotMatch(markup, /<code>consumer<\/code>/);
    snapshot.profiles.disabled_plugins.push("archive");
    assert.deepEqual(startupRepairPlan(snapshot), [], "Stale policy evidence cannot drive repair");
  } finally { await loader.close(); }
});
