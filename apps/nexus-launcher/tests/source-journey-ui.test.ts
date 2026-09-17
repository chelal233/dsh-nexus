import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";
import {
  externalHarnessRoot,
  hasHarnessSource,
  needsHarnessInstall,
  startupRepairTarget,
} from "../src/control-state.ts";
const button = (html: string, label: string) =>
  [...html.matchAll(/<button([^>]*)>([\s\S]*?)<\/button>/g)].find(
    (m) => m[2].replace(/<[^>]+>/g, "") === label,
  );
test("preflight tool failures lead to runtime repair and interrupted installation to maintenance", () => {
  for (const id of ["node", "npm", "pnpm", "node_program"])
    assert.deepEqual(startupRepairTarget(id), { module: "settings", section: "runtime" });
  assert.deepEqual(startupRepairTarget("installation"), { module: "maintenance" });
  assert.deepEqual(startupRepairTarget("unknown_future_check"), {
    module: "settings",
    section: "harness",
  });
});
test("source-aware gates keep custom startup separate from DSH terminal", () => {
  const external = { external_harness: { root: "E:/external" } };
  assert.equal(externalHarnessRoot(external), "E:/external");
  assert.equal(hasHarnessSource(external, { releases: [] }), true);
  assert.equal(needsHarnessInstall(external, { releases: [] }), false);
  assert.equal(hasHarnessSource({}, { current_release: "slot-a" }), true);
  assert.equal(hasHarnessSource({ harness: { program: "custom.exe" } }, { releases: [] }), false);
  assert.equal(
    needsHarnessInstall({ harness: { program: "custom.exe" } }, { releases: [] }),
    false,
  );
  assert.equal(needsHarnessInstall({}, {}), false);
  assert.equal(needsHarnessInstall({ harness: null }, { releases: [] }), true);
  assert.equal(startupRepairTarget("profile").module, "profiles");
  assert.equal(startupRepairTarget("runtime").section, "runtime");
  assert.equal(startupRepairTarget("recovery_mode").module, "maintenance");
});
test("real views expose fresh external entrypoints and explicit source semantics", async () => {
  const loader = await createUiTestLoader();
  try {
    const {
      HarnessTerminalButton,
      OverviewView,
      GuideView,
      UpdatesView,
      HarnessSourcePanel,
      BasicStartupCheck,
      CanaryPanel,
    } = await loader.loadModule("/src/App.tsx");
    const snapshot: any = {
      startup: { available: true },
      endpointErrors: {},
      status: {},
      health: {},
      state: {},
      config: { revision: "r2", external_harness: { root: "E:/external", version: "0.1.2" } },
      harnessRuntime: { harness: { state: "stopped" } },
      harnessUi: {},
      profiles: { active_profile: "web", profiles: ["web"] },
      releases: { releases: [] },
      updates: {},
      checkpoints: {},
      diagnostics: {},
      recovery: {},
    };
    const props = {
      snapshot,
      busyAction: null,
      runAction: async () => false,
      refresh: async () => {},
      themeMode: "system",
      setThemeMode: () => {},
      openSettings: () => {},
      openWorkbench: () => {},
    };
    const render = (component: any, extra: any = {}) =>
      renderToStaticMarkup(createElement(component, { ...props, ...extra }));
    const overview = render(OverviewView);
    assert.ok(overview.includes("E:/external"));
    const terminal = button(render(HarnessTerminalButton), "Open DSH terminal");
    assert.ok(terminal);
    assert.doesNotMatch(terminal[1], /disabled/);
    assert.match(
      button(render(HarnessTerminalButton, { busyAction: "running" }), "Open DSH terminal")![1],
      /disabled/,
    );
    assert.match(
      button(
        render(HarnessTerminalButton, { snapshot: { ...snapshot, startup: { available: false } } }),
        "Open DSH terminal",
      )![1],
      /disabled/,
    );
    const guide = render(GuideView);
    for (const label of [
      "Prepare",
      "Install Harness",
      "Check and start",
      "Open Settings",
      "Open Workbench",
    ])
      assert.ok(guide.includes(label), label);
    assert.equal(button(guide, "Start"), undefined);
    assert.equal(button(overview, "Open DSH terminal"), undefined);
    const release = { id: "slot-a", status: "ready", path: "E:/slot-a" };
    const updates = render(UpdatesView, {
      snapshot: { ...snapshot, releases: { releases: [release] } },
    });
    assert.ok(updates.includes("Prepare this version slot"));
    assert.ok(updates.includes("does not change the active external program source"));
    assert.ok(
      render(UpdatesView, {
        snapshot: { ...snapshot, config: {}, releases: { releases: [release] } },
      }).includes("Switch to this version"),
    );
    assert.ok(render(HarnessSourcePanel).includes("Discard changes"));
    assert.ok(
      render(BasicStartupCheck, {
        disabled: false,
        initialResult: {
          ready: false,
          checks: [{ id: "profile", status: "blocked", message: "Bad profile" }],
        },
        onRepair: () => {},
      }).includes("Open the relevant repair page"),
    );
    const running = render(CanaryPanel, {
      snapshot: { ...snapshot, harnessRuntime: { harness: { state: "running", pid: 12 } } },
    });
    assert.ok(running.includes("Stop Harness for diagnostics"));
    assert.ok(running.includes("Return to Workbench"));
    assert.match(button(running, "Run isolated diagnostic")![1], /disabled/);
    assert.doesNotMatch(button(render(CanaryPanel), "Run isolated diagnostic")![1], /disabled/);
  } finally {
    await loader.close();
  }
});
