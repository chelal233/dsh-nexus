import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

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
    assert.match(summary, /Recovery required/); assert.match(summary, /RESTORE-FAILURE/); assert.match(summary, /Open operation details/);
    assert.doesNotMatch(summary, /<progress/);
    const pendingInventory = renderToStaticMarkup(createElement(CheckpointsView, { ...props, snapshot: { ...snapshot, checkpoints: { inventory_refresh_pending: true, snapshots: [] } }, embedded: true }));
    assert.match(pendingInventory, /Snapshot inventory refreshes after capture finishes/);
    assert.doesNotMatch(pendingInventory, /No snapshots reported/);
    const pendingVersion = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: { ...snapshot, lifecycleBusy: true, releases: { releases: [] }, updates: { operation: { operation_id: "done", phase: "succeeded", release_id: "slot" } } }, embedded: true }));
    assert.match(pendingVersion, /Verification pending/);
    assert.doesNotMatch(pendingVersion, /The task reports completion, but its version slot is unavailable/);
  } finally { await loader.close(); }
});
