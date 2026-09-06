import assert from "node:assert/strict";
import test from "node:test";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";

const startup = { available: true, running: true };
const baseSnapshot = {
  startup,
  endpointErrors: {},
  status: {}, health: {}, state: {}, harnessRuntime: {}, harnessUi: {},
  profiles: {}, checkpoints: {}, releases: {}, updates: {}, diagnostics: {}, config: {}, recovery: {},
};
const props = {
  busyAction: null,
  credentialInvalidationPending: false,
  runAction: async () => true,
  refresh: async () => undefined,
  themeMode: "system",
  setThemeMode: () => undefined,
};

async function loadViews() {
  const vite = await createServer({ root: process.cwd(), appType: "custom", logLevel: "silent", server: { middlewareMode: true } });
  const app = await vite.ssrLoadModule("/src/App.tsx");
  return { vite, CompatibilitySummary: app.CompatibilitySummary, CompatibilityDialog: app.CompatibilityDialog, ProfilesView: app.ProfilesView, ProfilePlugins: app.ProfilePlugins, UpdatesView: app.UpdatesView, CheckpointsView: app.CheckpointsView };
}

test("profile hub collapses children; profile plugins show truthful inventory", async () => {
  const { vite, ProfilesView, ProfilePlugins } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      recovery: { manual_entry_available: true, harness_stop_required: false, harness: { state: "failed" } },
      profiles: { active_profile: "web", manifests: [{ name: "web", bundles: ["dsh-base"], plugins: [
        { package: "dsh-base", version: "1.0.0", builtin: true, removable: false },
        { package: "extra-plugin", version: "2.0.0", builtin: false, removable: true },
      ] }] },
    };
    const markup = renderToStaticMarkup(createElement(ProfilesView, { ...props, snapshot }));
    assert.match(markup, /Profile catalog/);
    assert.match(markup, /▸/);
    assert.ok(markup.indexOf("Open settings.yaml") < markup.indexOf("profile-row-toggle"));
    assert.ok(markup.indexOf("profile-row-toggle") < markup.indexOf("New profile name"));
    assert.ok(markup.indexOf("New profile name") < markup.indexOf(">Create profile<"));
    assert.doesNotMatch(markup, /Saved checkpoints/);
    assert.doesNotMatch(markup, /Plugin inventory/);
    const pluginsMarkup = renderToStaticMarkup(createElement(ProfilePlugins, { ...props, snapshot, profile: "web" }));
    assert.match(pluginsMarkup, /dsh-base/);
    assert.match(pluginsMarkup, /Built-in/);
    assert.match(pluginsMarkup, /extra-plugin/);
    assert.match(pluginsMarkup, /Removable/);
  } finally { await vite.close(); }
});

test("cold confirmation renders exact version, destination, effects, and explicit actions", async () => {
  const { vite, UpdatesView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      config: { runtime: { source: "official", mode: "system", node: { path: "C:\\node.exe", ownership: "system" } } },
      updates: { update: { state: "running" }, operation: {
        operation_id: "cold-7", phase: "awaiting_confirmation", tag: "v1.2.3", progress_percent: 25,
        confirmation: "sha256:plan", supply_plan: {
          supply_plan_id: "sha256:plan", destination_root: "C:\\Nexus\\runtimes",
          node: { version: "24.20.0", disposition: "install_system", path: "C:\\Program Files\\nodejs\\node.exe" },
          pnpm: { version: "11.7.0", disposition: "install_system", path: "C:\\pnpm\\pnpm.exe" },
        },
      } },
      releases: { releases: [] },
    };
    const markup = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot }));
    assert.match(markup, /24\.20\.0/);
    assert.match(markup, /C:\\Nexus\\runtimes/);
    assert.match(markup, /install_system/);
    assert.match(markup, /Confirm exact plan/);
    assert.match(markup, /Cancel/);
    assert.match(markup, /test an isolated profile first/);
    assert.match(markup, /working profile is not started automatically/);
  } finally { await vite.close(); }
});

test("compatibility summary identifies the checked release, projection, and disabled plugin", async () => {
  const { vite, CompatibilitySummary } = await loadViews();
  try {
    const snapshot = { ...baseSnapshot, profiles: { compatibility: {
      status: "isolated", source_profile: "desktop", effective_profile: "nexus-projection", release_id: "rc1",
      disabled: [{ package: "third-party-plugin", reason: "missing startup API" }],
    } } };
    const markup = renderToStaticMarkup(createElement(CompatibilitySummary, { ...props, snapshot }));
    assert.match(markup, /Startup compatibility check/);
    assert.match(markup, /desktop/);
    assert.match(markup, /nexus-projection/);
    assert.match(markup, /rc1/);
    assert.match(markup, /third-party-plugin/);
    assert.match(markup, /missing startup API/);
    assert.match(markup, /not every runtime feature/);
  } finally { await vite.close(); }
});

test("failed compatibility offers explicit plugin choices and retry without claiming success", async () => {
  const { vite, CompatibilitySummary } = await loadViews();
  try {
    const snapshot = { ...baseSnapshot,
      recovery: { harness_stop_required: false, harness: { state: "stopped" } },
      releases: { current_release: "old", releases: [{ id: "target" }] },
      profiles: { compatibility: { status: "needs_choice", source_profile: "desktop", release_id: "target",
        error: "Unclassified plugin startup error", disabled: [], candidates: [{ package: "third-party", reason: "Not identified as faulty; optional isolation for troubleshooting" }],
      } },
    };
    const markup = renderToStaticMarkup(createElement(CompatibilitySummary, { ...props, snapshot }));
    assert.match(markup, /Choose how to handle plugin errors/);
    assert.match(markup, /type="checkbox"/);
    assert.doesNotMatch(markup, /checked=""/);
    assert.match(markup, /Select all third-party plugins/);
    assert.match(markup, /Save disabled plugins/);
    assert.match(markup, /Retry version switch/);
    assert.match(markup, /not confirmed faults/);
    assert.doesNotMatch(markup, /Startup check passed|Effective isolated profile/);
  } finally { await vite.close(); }
});

test("saved isolation is visible and reversible before another check", async () => {
  const { vite, CompatibilitySummary } = await loadViews();
  try {
    const snapshot = { ...baseSnapshot,
      recovery: { harness_stop_required: false, harness: { state: "stopped" } },
      profiles: { active_profile: "desktop", disabled_plugins: ["third-party"] },
    };
    const markup = renderToStaticMarkup(createElement(CompatibilitySummary, { ...props, snapshot }));
    assert.match(markup, /Saved plugin choices; effective on next check/);
    assert.match(markup, /Restore plugin on next check/);
    assert.match(markup, /third-party/);
    assert.doesNotMatch(markup, /Startup check passed/);
  } finally { await vite.close(); }
});

test("checkpoint fixtures show legacy truth and pending retry or abort", async () => {
  const { vite, CheckpointsView } = await loadViews();
  try {
    const snapshot = { ...baseSnapshot, recovery: { harness_stop_required: false, harness: { state: "stopped" } }, checkpoints: {
      checkpoints: [{ id: "legacy-1", profile: "web", created_at_unix: 1, state: { profile: "web" } }],
      pending_restore: { checkpoint_id: "cp-2", snapshot_id: "snap-2", ticket_id: "ticket-2", state: "materialization_pending", retryable: true, abortable: true, error: "install failed" },
      healthy_capture_error: "snapshot store busy",
    } };
    const markup = renderToStaticMarkup(createElement(CheckpointsView, { ...props, snapshot }));
    assert.match(markup, /Legacy metadata only/);
    assert.match(markup, /materialization pending/i);
    assert.match(markup, /install failed/);
    assert.match(markup, /Retry/);
    assert.match(markup, /Abort/);
    assert.match(markup, /snapshot store busy/);
  } finally { await vite.close(); }
});


test("compatibility provenance distinguishes switch checks, startup cache reuse, and old records", async () => {
  const { vite, CompatibilitySummary } = await loadViews();
  try {
    const report = { status: "isolated", source_profile: "desktop", release_id: "rc1", checked_at_unix: 1788670000, trigger: "version_switch", last_trigger: "startup", last_used_at_unix: 1788670200, cache_reused: true, disabled: [] };
    const render = (compatibility: object) => renderToStaticMarkup(createElement(CompatibilitySummary, { ...props, snapshot: { ...baseSnapshot, profiles: { compatibility } } }));
    const cached = render(report);
    assert.match(cached, /During version switch/);
    assert.match(cached, /Before startup or restart/);
    assert.match(cached, /Reused previous check result/);
    assert.match(cached, /Checked at/);
    assert.match(cached, /Last used/);
    assert.doesNotMatch(cached, /New check result/);
    assert.match(render({ ...report, cache_reused: false, last_trigger: "version_switch" }), /New check result/);
    const legacy = render({ status: "isolated", disabled: [] });
    assert.match(legacy, /Legacy record: trigger not recorded/);
    assert.doesNotMatch(legacy, /New check result/);
  } finally { await vite.close(); }
});


test("busy cold switch keeps cancellation enabled while other mutations are disabled", async () => {
  const { vite, UpdatesView } = await loadViews();
  try {
    const snapshot = { ...baseSnapshot, lifecycleBusy: true, updates: { update: { state: "running" }, operation: { operation_id: "cold-1", phase: "verifying", progress_percent: 80 } } };
    const markup = renderToStaticMarkup(createElement(UpdatesView, { ...props, busyAction: "Operation in progress", snapshot }));
    assert.match(markup, /<button(?![^>]*disabled)[^>]*>Cancel<\/button>/);
    assert.match(markup, /<button[^>]*disabled[^>]*>Save update source<\/button>/);
  } finally { await vite.close(); }
});


test("check details live in a dialog and actual startup failure overrides preflight success", async () => {
 const { vite, CompatibilityDialog, ProfilesView } = await loadViews();
 try {
  const snapshot = { ...baseSnapshot, harnessRuntime: { harness: { state: "failed" } }, recovery: { log_tail: [{ stream: "stderr", content: "task-board ledger is already owned", truncated: false }] }, profiles: { compatibility: { status: "passed", trigger: "profile_switch", last_trigger: "profile_switch", source_profile: "desktop", release_id: "rc1" } } };
  const page = renderToStaticMarkup(createElement(ProfilesView, { ...props, snapshot }));
  assert.doesNotMatch(page, /Startup compatibility check|task-board ledger/);
  const dialog = renderToStaticMarkup(createElement(CompatibilityDialog, { ...props, snapshot, pending: false, onClose() {} }));
  assert.match(dialog, /role="dialog"/);
  assert.match(dialog, /Harness startup failed/);
  assert.match(dialog, /task-board ledger/);
  assert.match(dialog, /During profile switch/);
  const busy = renderToStaticMarkup(createElement(CompatibilityDialog, { ...props, snapshot, pending: true, onClose() {} }));
  assert.match(busy, /check continues in the background/);
  assert.doesNotMatch(busy, /Startup check passed/);
  const earlyFailure = renderToStaticMarkup(createElement(CompatibilityDialog, { ...props, snapshot: { ...baseSnapshot, startup: { ...startup, harness_startup_error: "Node entry missing" } }, pending: false, onClose() {} }));
  assert.match(earlyFailure, /Node entry missing/);
 } finally { await vite.close(); }
});


test("plugin rows expose movable installed bundles, locked roots and read-only projections", async () => {
 const { vite, ProfilePlugins } = await loadViews();
 try {
  const bundles = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app", "third-party"];
  const manifest = { name: "desktop", bundles, plugins: bundles.map((packageName, i) => ({ package: packageName, builtin: i < 2, removable: i === 2 })) };
  const snapshot = { ...baseSnapshot, recovery: { harness: { state: "stopped" } }, profiles: { active_profile: "desktop", manifests: [manifest] } };
  const markup = renderToStaticMarkup(createElement(ProfilePlugins, { ...props, snapshot, profile: "desktop" }));
  assert.equal((markup.match(/draggable="false"/g) || []).length, 2);
  assert.equal((markup.match(/draggable="true"/g) || []).length, 1);
  assert.match(markup, /Fixed load position/);
  assert.match(markup, /Removable/);
  const generated = renderToStaticMarkup(createElement(ProfilePlugins, { ...props, profile: "desktop", snapshot: { ...snapshot, profiles: { ...snapshot.profiles, manifests: [{ ...manifest, source_profile: "original" }] } } }));
  assert.match(generated, /source profile original/);
  assert.doesNotMatch(generated, /draggable="true"/);
  const running = renderToStaticMarkup(createElement(ProfilePlugins, { ...props, profile: "desktop", snapshot: { ...snapshot, recovery: { harness: { state: "running" } } } }));
  assert.doesNotMatch(running, /draggable="true"/);
 } finally { await vite.close(); }
});


test("update progress keeps stage and terminal errors visible without ownership internals", async () => {
  const { vite, UpdatesView } = await loadViews();
  try {
    const render = (operation: object) => renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: { ...baseSnapshot, updates: { update: { state: "idle", error: "older failure" }, operation } } }));
    const running = render({ operation_id: "cold-private-id", tag: "v1", phase: "cloning", progress_percent: 10, owner_quiescent: false });
    assert.match(running, /Current stage/);
    assert.match(running, /v1/);
    assert.doesNotMatch(running, /Owner quiescent|No update error reported|cold-private-id|older failure/);
    const failed = render({ operation_id: "cold-private-id", phase: "failed", error: "Clone connection failed", cleanup_pending: true });
    assert.match(failed, /role="alert"/);
    assert.match(failed, /Clone connection failed/);
    assert.match(failed, /Retry cleanup/);
    const completed = render({ operation_id: "cold-private-id", phase: "succeeded", progress_percent: 100 });
    assert.match(completed, /Current stage/);
    assert.doesNotMatch(completed, /older failure/);
  } finally { await vite.close(); }
});
