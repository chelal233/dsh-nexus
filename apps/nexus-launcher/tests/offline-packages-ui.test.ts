import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("offline controls use full paths, honor gates, and show persisted export outcomes without slot errors", async () => {
  const loader = await createUiTestLoader();
  try {
    const { OfflinePackagePanel, UpdatesView, OperationStatusPanel, OfflineOperationStatus } = await loader.loadModule("/src/App.tsx");
    const snapshot = { startup: { available: true }, endpointErrors: {}, config: {}, releases: { current_release: "slot", releases: [{ id: "slot", version: "0.1.2-rc.1" }] }, updates: {}, recovery: {}, checkpoints: {} };
    const props = { snapshot, busyAction: null, runAction: async () => true, refresh: async () => {}, themeMode: "system", setThemeMode: () => {} };
    const renderPanel = (patch = {}) => renderToStaticMarkup(createElement(OfflinePackagePanel, { ...props, ...patch }));
    const html = renderPanel();
    for (const label of ["Package to import (full .tar.gz path)", "Export destination (full .tar.gz path)", "Version to export", "No dependency downloads or builds are needed", "Browse file", "Choose save location", "Read package contents", "Export contents", "Program and runtime", "Session history and attachments", "they do not authenticate the publisher", "Export does not change the selected version"]) assert.ok(html.includes(label), label);
    assert.match(html, /value="slot" selected=""/);
    assert.doesNotMatch(html.match(/<select[^>]*>/)?.[0] || "", /disabled/);
    for (const patch of [{ busyAction: "busy" }, { snapshot: { ...snapshot, startup: { available: false } } }, { snapshot: { ...snapshot, updates: { operation: { phase: "succeeded", cleanup_pending: true } } } }, { snapshot: { ...snapshot, updates: { operation: { phase: "verifying" } } } }]) {
      const blocked = renderPanel(patch);
      assert.match(blocked.match(/<select[^>]*>/)?.[0] || "", /disabled/);
      assert.ok((blocked.match(/<input[^>]*disabled=""/g) || []).length >= 2);
    }
    const operation = { operation_id: "export-durable", kind: "offline_export", archive_path: "D:/Offline/saved.tar.gz", release_id: "gone", phase: "succeeded", cleanup_pending: true, cleanup_error: "CLEANUP-FAILURE", owner_quiescent: true };
    const saved = { ...snapshot, releases: null, updates: { operation } };
    const result = renderToStaticMarkup(createElement(OfflinePackagePanel, { ...props, snapshot: saved }));
    assert.match(result, /View progress/); assert.doesNotMatch(result, /saved.tar.gz|Copy path|Clear finished record/);
    const modalResult = renderToStaticMarkup(createElement(OfflineOperationStatus, { ...props, snapshot: saved }));
    assert.match(modalResult, /The package was exported/); assert.match(modalResult, /CLEANUP-FAILURE/); assert.match(modalResult, /Retry cleanup/);
    const upstream = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: saved, embedded: true }));
    assert.doesNotMatch(upstream, /saved.tar.gz|CLEANUP-FAILURE|Exported package path/);
    assert.doesNotMatch(result, /Verification pending|its version slot is unavailable|Last installation/);
    const summary = renderToStaticMarkup(createElement(OperationStatusPanel, { snapshot: saved, onOpen: () => {} }));
    assert.match(summary, /Package exported; cleanup required/); assert.match(summary, /Recent activity/); assert.doesNotMatch(summary, /Exported package path|Copy path|Open operation details/);
    const failed = renderToStaticMarkup(createElement(OfflineOperationStatus, { ...props, snapshot: { ...snapshot, updates: { operation: { ...operation, phase: "failed", cleanup_pending: false, error: "PACKAGE-FAILURE" } } } }));
    assert.match(failed, /Retry offline operation/); assert.match(failed, /PACKAGE-FAILURE/); assert.doesNotMatch(failed, /Retry installation/);
    const imported = renderToStaticMarkup(createElement(OfflineOperationStatus, { ...props, snapshot: { ...snapshot, updates: { operation: { ...operation, kind: "offline_import", release_id: "slot", cleanup_pending: false, cleanup_error: null } } } }));
    assert.match(imported, /Harness stays stopped; run startup checks before starting it/);
    const recovery = renderToStaticMarkup(createElement(OfflineOperationStatus, { ...props, snapshot: { ...snapshot, updates: { operation: { ...operation, kind: "offline_import", credential_recovery_path: "C:/Nexus/run/credential-recovery-cold-1.json" } } } }));
    assert.match(recovery, /Credential recovery record/); assert.match(recovery, /credential-recovery-cold-1.json/); assert.match(recovery, /stop Harness before restoring/);
    const prepared = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: { ...snapshot, updates: { operation: { ...operation, kind: "cold_switch", warning: "rollback_health_required" } } }, embedded: true }));
    assert.match(prepared, /Version is ready/); assert.match(prepared, /then switch in Release slots/);
    const preparedSnapshot = { ...snapshot, updates: { update: { state: "prepared", release_id: "new-slot" }, operation: { operation_id: "prepare-only", kind: "cold_switch", phase: "prepared", release_id: "new-slot", owner_quiescent: true } } };
    const preparedView = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: preparedSnapshot, embedded: true }));
    assert.match(preparedView, /Prepared; awaiting manual confirmation/);
    assert.doesNotMatch(preparedView, /Succeeded|Verification pending/);
    const preparedPanel = renderPanel({ snapshot: preparedSnapshot });
    assert.doesNotMatch(preparedPanel.match(/<select[^>]*>/)?.[0] || "", /disabled/);
  } finally { await loader.close(); }
});

test("offline stage progress uses counts and keeps unknown compression totals indeterminate", async () => {
  const loader = await createUiTestLoader();
  try {
    const { OfflineOperationStatus } = await loader.loadModule("/src/App.tsx");
    const render = (progress: Record<string, unknown>) => renderToStaticMarkup(createElement(OfflineOperationStatus, {
      snapshot: { config: {}, updates: { operation: { operation_id: "export", kind: "offline_export", phase: "verifying" }, offline_progress: progress } },
      busyAction: null, runAction: async () => true,
    }));
    const copying = render({ stage: "copy_slot", completed: 25, total: 100 });
    assert.match(copying, /Copying Harness files/); assert.match(copying, /max="100" value="25"/); assert.match(copying, /25 \/ 100 files/);
    const compression = render({ stage: "compress", completed: 1048576, total: null, unit: "bytes" });
    assert.match(compression, /Written 1.0 MiB/); assert.doesNotMatch(compression.match(/<progress[^>]*>/)?.[0] || "", /value=/);
  } finally { await loader.close(); }
});
