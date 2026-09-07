import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("guide presents four explicit steps and launch explanation never substitutes stale inputs", async () => {
  const loader = await createUiTestLoader();
  try {
    const { GuideView, LaunchInputsPanel } = await loader.loadModule("/src/App.tsx");
    const runtime = { generation: 3, log_session_run_id: "run3", harness: { state: "running", pid: 42 } };
    const record = { generation: 3, run_id: "run3", fields: [{ name: "Program", value: "CURRENT_INPUT", source: "Resolved launch configuration" }] };
    const snapshot = {
      startup: { available: true }, endpointErrors: {}, status: {}, health: {}, state: {},
      config: { runtime: {}, harness_preferences: {}, launch_inputs: {
        next_launch: { fields: [{ name: "Program", value: "NEXT_INPUT", source: "Resolved launch configuration" }] }, running_launch: record,
      } }, harnessRuntime: runtime,
      harnessUi: { available: true, generation: 3, run_id: "run3", url: "http://127.0.0.1:4321/?token=DO_NOT_RENDER" },
      profiles: null, releases: { releases: [] }, updates: null, checkpoints: null, diagnostics: null, recovery: null,
    };
    const render = (value: typeof snapshot) => renderToStaticMarkup(createElement(LaunchInputsPanel, { snapshot: value }));
    const current = render(snapshot);
    assert.match(current, /NEXT_INPUT/); assert.match(current, /CURRENT_INPUT/); assert.match(current, /4321/);
    assert.doesNotMatch(current, /DO_NOT_RENDER/);
    const stale = render({ ...snapshot, harnessRuntime: { ...runtime, generation: 4 } });
    assert.match(stale, /NEXT_INPUT/); assert.doesNotMatch(stale, /CURRENT_INPUT|4321/);
    const guide = renderToStaticMarkup(createElement(GuideView, { snapshot: { ...snapshot, harnessRuntime: { harness: { state: "stopped" } } }, busyAction: null, runAction: async () => false, refresh: async () => {}, themeMode: "system", setThemeMode: () => {} }));
    for (const label of ["1 · Harness data directory", "2 · Harness version", "3 · Basic startup checks", "4 · Start Harness"]) assert.ok(guide.includes(label), label);
    assert.match(guide, /Existing files are not moved or deleted/);
    assert.match(guide, /Inherit upstream default/);
    assert.match(guide, /Run basic checks/);
  } finally { await loader.close(); }
});
