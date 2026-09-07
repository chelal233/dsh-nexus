import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("offline controls use full paths, honor gates, and show persisted export outcomes without slot errors", async () => {
  const loader = await createUiTestLoader();
  try {
    const { OfflinePackagePanel, UpdatesView, OperationStatusPanel } = await loader.loadModule("/src/App.tsx");
    const snapshot = { startup: { available: true }, endpointErrors: {}, config: {}, releases: { current_release: "slot", releases: [{ id: "slot", version: "0.1.2-rc.1" }] }, updates: {}, recovery: {}, checkpoints: {} };
    const props = { snapshot, busyAction: null, runAction: async () => true, refresh: async () => {}, themeMode: "system", setThemeMode: () => {} };
    const renderPanel = (patch = {}) => renderToStaticMarkup(createElement(OfflinePackagePanel, { ...props, ...patch }));
    const html = renderPanel();
    for (const label of ["Package to import (full .tar.gz path)", "Export destination (full .tar.gz path)", "Version to export", "version probes will run", "they do not authenticate the publisher", "Export does not change the selected version"]) assert.ok(html.includes(label), label);
    assert.match(html, /value="slot" selected=""/);
    assert.doesNotMatch(html.match(/<select[^>]*>/)?.[0] || "", /disabled/);
    for (const patch of [{ busyAction: "busy" }, { snapshot: { ...snapshot, startup: { available: false } } }, { snapshot: { ...snapshot, updates: { operation: { phase: "succeeded", cleanup_pending: true } } } }, { snapshot: { ...snapshot, updates: { operation: { phase: "verifying" } } } }]) {
      const blocked = renderPanel(patch);
      assert.match(blocked.match(/<select[^>]*>/)?.[0] || "", /disabled/);
      assert.equal((blocked.match(/<input[^>]*disabled=""/g) || []).length, 2);
    }
    const operation = { operation_id: "export-durable", kind: "offline_export", archive_path: "D:/Offline/saved.tar.gz", release_id: "gone", phase: "succeeded", cleanup_pending: true, cleanup_error: "CLEANUP-FAILURE", owner_quiescent: true };
    const saved = { ...snapshot, releases: null, updates: { operation } };
    const result = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: saved, embedded: true }));
    assert.match(result, /Exported package path/); assert.match(result, /D:\/Offline\/saved.tar.gz/); assert.match(result, /The package was exported. Temporary-file cleanup still needs attention/); assert.match(result, /CLEANUP-FAILURE/); assert.match(result, /Retry cleanup/);
    assert.doesNotMatch(result, /Verification pending|its version slot is unavailable|Last installation/);
    const summary = renderToStaticMarkup(createElement(OperationStatusPanel, { snapshot: saved, onOpen: () => {} }));
    assert.match(summary, /Package exported; cleanup required/); assert.match(summary, /Exported package path/);
    const failed = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: { ...snapshot, updates: { operation: { ...operation, phase: "failed", cleanup_pending: false, error: "PACKAGE-FAILURE" } } }, embedded: true }));
    assert.match(failed, /Retry offline operation/); assert.match(failed, /PACKAGE-FAILURE/); assert.doesNotMatch(failed, /Retry installation/);
    const imported = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot: { ...snapshot, updates: { operation: { ...operation, kind: "offline_import", release_id: "slot", cleanup_pending: false, cleanup_error: null } } }, embedded: true }));
    assert.match(imported, /Harness stays stopped; run startup checks before starting it/);
  } finally { await loader.close(); }
});
