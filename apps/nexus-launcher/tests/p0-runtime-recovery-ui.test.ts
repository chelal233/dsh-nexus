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
  return { vite, ProfilesView: app.ProfilesView, ProfilePlugins: app.ProfilePlugins, UpdatesView: app.UpdatesView, CheckpointsView: app.CheckpointsView };
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
  const { vite, UpdatesView } = await loadViews();
  try {
    const snapshot = { ...baseSnapshot, profiles: { compatibility: {
      status: "isolated", source_profile: "desktop", effective_profile: "nexus-projection", release_id: "rc1",
      disabled: [{ package: "third-party-plugin", reason: "missing startup API" }],
    } } };
    const markup = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot }));
    assert.match(markup, /Startup compatibility check/);
    assert.match(markup, /desktop/);
    assert.match(markup, /nexus-projection/);
    assert.match(markup, /rc1/);
    assert.match(markup, /third-party-plugin/);
    assert.match(markup, /missing startup API/);
    assert.match(markup, /not every runtime feature/);
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
