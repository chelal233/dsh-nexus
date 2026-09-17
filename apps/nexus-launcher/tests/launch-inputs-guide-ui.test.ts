import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("guide presents three focused steps and launch explanation never substitutes stale inputs", async () => {
  const loader = await createUiTestLoader();
  try {
    const { GuideView, LaunchInputsPanel } = await loader.loadModule("/src/App.tsx");
    const runtime = {
      generation: 3,
      log_session_run_id: "run3",
      harness: { state: "running", pid: 42 },
    };
    const record = {
      generation: 3,
      run_id: "run3",
      fields: [
        { name: "Program", value: "CURRENT_INPUT", source: "Resolved launch configuration" },
      ],
    };
    const snapshot = {
      startup: { available: true },
      endpointErrors: {},
      status: {},
      health: {},
      state: {},
      config: {
        runtime: {},
        harness_preferences: {},
        launch_inputs: {
          next_launch: {
            fields: [
              { name: "Program", value: "NEXT_INPUT", source: "Resolved launch configuration" },
            ],
          },
          running_launch: record,
        },
      },
      harnessRuntime: runtime,
      harnessUi: {
        available: true,
        generation: 3,
        run_id: "run3",
        url: "http://127.0.0.1:4321/?token=DO_NOT_RENDER",
      },
      profiles: null,
      releases: { releases: [] },
      updates: null,
      checkpoints: null,
      diagnostics: null,
      recovery: null,
    };
    const render = (value: typeof snapshot) =>
      renderToStaticMarkup(createElement(LaunchInputsPanel, { snapshot: value }));
    const current = render(snapshot);
    assert.match(current, /NEXT_INPUT/);
    assert.match(current, /CURRENT_INPUT/);
    assert.match(current, /4321/);
    assert.doesNotMatch(current, /DO_NOT_RENDER/);
    const stale = render({ ...snapshot, harnessRuntime: { ...runtime, generation: 4 } });
    assert.match(stale, /NEXT_INPUT/);
    assert.doesNotMatch(stale, /CURRENT_INPUT|4321/);
    const guide = renderToStaticMarkup(
      createElement(GuideView, {
        snapshot: { ...snapshot, harnessRuntime: { harness: { state: "stopped" } } },
        busyAction: null,
        runAction: async () => false,
        refresh: async () => {},
        themeMode: "system",
        setThemeMode: () => {},
      }),
    );
    for (const label of ["Prepare", "Install Harness", "Check and start"])
      assert.ok(guide.includes(label), label);
    assert.doesNotMatch(guide, /Release slots|Start and use|Run basic checks/);
    assert.match(guide, /Inherit upstream default/);
    assert.match(guide, /Open Settings/);
    assert.match(guide, /<button(?![^>]*disabled)[^>]*>Choose version and install<\/button>/);
    assert.match(guide, /Use an already built directory/);
    assert.doesNotMatch(guide, /Data paths and the program source are managed in Settings/);
    const installing = renderToStaticMarkup(
      createElement(GuideView, {
        snapshot: {
          ...snapshot,
          config: {
            ...snapshot.config,
            external_harness: { root: "C:/prepared", version: "0.1.2-rc.1" },
          },
          harnessRuntime: { harness: { state: "stopped" } },
          updates: { operation: { operation_id: "installing", phase: "building" } },
        },
        busyAction: null,
        runAction: async () => false,
        refresh: async () => {},
        themeMode: "system",
        setThemeMode: () => {},
      }),
    );
    assert.match(installing, /An installation is already in progress; wait for it to finish\./);
    assert.match(installing, /Check and start/);
    assert.doesNotMatch(
      installing,
      /<button(?![^>]*disabled)[^>]*>Choose version and install<\/button>/,
    );
    assert.doesNotMatch(
      installing,
      /<button(?![^>]*disabled)[^>]*>Use an already built directory<\/button>/,
    );
  } finally {
    await loader.close();
  }
});
