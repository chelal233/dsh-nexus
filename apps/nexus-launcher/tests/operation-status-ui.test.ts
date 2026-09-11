import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("completed history stays out of global banners while required recovery remains", async () => {
  const loader = await createUiTestLoader();
  try {
    const { OperationStatusPanel, ToastNotice, requiresErrorBanner } = await loader.loadModule("/src/App.tsx");
    const snapshot = { startup: { available: true }, endpointErrors: {}, releases: { releases: [{ id: "slot" }] }, updates: { operation: { operation_id: "done", phase: "succeeded", release_id: "slot" } } };
    const render = (value: unknown, attentionOnly = true) => renderToStaticMarkup(createElement(OperationStatusPanel, { snapshot: value, attentionOnly, onOpen: () => {} }));
    assert.equal(render(snapshot), "");
    assert.match(render(snapshot, false), /Completed/);
    assert.match(render({ ...snapshot, checkpoints: { pending_restore: { checkpoint_id: "restore-me", state: "failed", error: "RAW-RESTORE" } } }), /RAW-RESTORE/);
    for (const code of ["config_revision_conflict", "harness_preflight_blocked", "patch_invalid", "harness_start_paused"]) assert.equal(requiresErrorBanner(code), true);
    assert.equal(requiresErrorBanner("open_path_failed"), false);
    const toast = renderToStaticMarkup(createElement(ToastNotice, { message: "RAW-OPEN-ERROR", kind: "error", onDetails: () => {} }));
    assert.match(toast, /role="alert"/); assert.match(toast, /RAW-OPEN-ERROR/); assert.match(toast, /Open operation details/);
  } finally { await loader.close(); }
});

test("cleanup scan is non-error progress, blocks duplicate previews, and retains raw failures", async () => {
  const loader = await createUiTestLoader();
  try {
    const { SpaceMaintenancePanel } = await loader.loadModule("/src/App.tsx");
    const render = (scan: Record<string, unknown>) => renderToStaticMarkup(createElement(SpaceMaintenancePanel, {
      snapshot: { startup: { available: true }, maintenance: { preview_scan: scan } }, busyAction: null,
    }));
    const running = render({ state: "running", operation_id: "scan-a", wait_message: "original deadline message" });
    assert.match(running, /role="status"[^>]*>Cleanup preview is scanning in the background/);
    assert.match(running, /original deadline message/);
    assert.doesNotMatch(running, /class="form-error"/);
    assert.match(running, /<button[^>]*disabled=""[^>]*>Working…<\/button>/);
    assert.match(running, /<button(?![^>]*disabled)[^>]*>Refresh saved result<\/button>/);
    const failed = render({ state: "failed", error: "RAW-PREVIEW-FAILURE" });
    assert.match(failed, /class="form-error" role="alert">RAW-PREVIEW-FAILURE/);
    assert.match(failed, /<button(?![^>]*disabled)[^>]*>Preview cleanup<\/button>/);
  } finally { await loader.close(); }
});

test("recovery controls remain visible without profiles and obey Agent, Harness and busy gates", async () => {
  const loader = await createUiTestLoader();
  try {
    const { ProfilesView, OperationStatusPanel, CheckpointsView, UpdatesView } = await loader.loadModule("/src/App.tsx");
    const snapshot = { startup: { available: true }, endpointErrors: {}, config: {}, profiles: { manifests: [] }, recovery: { harness: { state: "stopped" }, harness_stop_required: false }, checkpoints: { pending_restore: { checkpoint_id: "UNSELECTED-PROFILE-CHECKPOINT", state: "materialization_pending", error: "RESTORE-FAILURE", retryable: true, abortable: true } } };
    const props = { snapshot, busyAction: null, runAction: async () => true, refresh: async () => {}, themeMode: "system", setThemeMode: () => {} };
    const render = (patch = {}) => renderToStaticMarkup(createElement(ProfilesView, { ...props, ...patch }));
    const html = render();
    assert.match(html, /id="restore-status"/); assert.match(html, /UNSELECTED-PROFILE-CHECKPOINT/); assert.match(html, /RESTORE-FAILURE/);
    const button = (markup: string, label: string) => markup.match(new RegExp('<button[^>]*>' + label + '<\/button>'))?.[0] || "";
    for (const label of ["Retry", "Abort"]) { assert.ok(button(html, label), label); assert.doesNotMatch(button(html, label), /disabled/); }
    for (const patch of [{ busyAction: "other operation" }, { snapshot: { ...snapshot, startup: { available: false } } }, { snapshot: { ...snapshot, recovery: { harness: { state: "running" }, harness_stop_required: true } } }]) {
      const gated = render(patch); for (const label of ["Retry", "Abort"]) assert.match(button(gated, label), /disabled/);
    }
    const summary = renderToStaticMarkup(createElement(OperationStatusPanel, { snapshot, onOpen: () => {} }));
    assert.match(summary, /Recovery required/); assert.match(summary, /RESTORE-FAILURE/); assert.match(summary, /Resolve issue/);
    assert.doesNotMatch(summary, /<progress/);
    const pendingInventory = renderToStaticMarkup(createElement(CheckpointsView, { ...props, snapshot: { ...snapshot, checkpoints: { inventory_refresh_pending: true, snapshots: [] } }, embedded: true }));
    assert.match(pendingInventory, /Snapshot inventory refreshes after capture finishes/);
    assert.doesNotMatch(pendingInventory, /No snapshots reported/);
    const pendingVersion = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: { ...snapshot, lifecycleBusy: true, releases: { releases: [] }, updates: { operation: { operation_id: "done", phase: "succeeded", release_id: "slot" } } }, embedded: true }));
    assert.match(pendingVersion, /Verification pending/);
    assert.doesNotMatch(pendingVersion, /The task reports completion, but its version slot is unavailable/);
  } finally { await loader.close(); }
});
