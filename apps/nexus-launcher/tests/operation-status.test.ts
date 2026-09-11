import assert from "node:assert/strict";
import test from "node:test";
import { advanceOperationNotices, operationSummaries, operationResponseNotice, operationNoticeKind, operationRetryCommand } from "../src/operation-status.ts";
import { updateSourcePayload, refreshEditableDraft, finishDraftSave } from "../src/settings-state.ts";
import { coldOperationIsTerminal } from "../src/control-state.ts";

test("a prepared release is terminal but awaits a manual switch instead of reporting completion", () => {
  const rows = phase => operationSummaries({ updates: { operation: { operation_id: "prepared-one", phase, release_id: "new-slot", progress_percent: 100 } } });
  const prepared = rows("prepared");
  assert.equal(prepared[0].status, "Prepared; awaiting manual confirmation");
  assert.equal(prepared[0].progress, undefined);
  assert.equal(coldOperationIsTerminal("prepared"), true);
  assert.equal(coldOperationIsTerminal("awaiting_confirmation"), false);
  let tracker = advanceOperationNotices(rows("building"), new Map());
  tracker = advanceOperationNotices(prepared, tracker.pending);
  assert.equal(tracker.notices.length, 1);
  assert.equal(tracker.notices[0].status, "Prepared; awaiting manual confirmation");
  assert.equal(advanceOperationNotices(prepared, tracker.pending).notices.length, 0);
  assert.equal(advanceOperationNotices(prepared, new Map()).notices.length, 0);
});

test("offline recovery opens its own controls and retries only the original selection", () => {
  const offline_contents = { runtime: false, profiles: ["web"], environment: false, sessions: false, plugins: false, credentials: false };
  for (const kind of ["offline_import", "offline_export"]) {
    const operation = { operation_id: "data-only", kind, archive_path: "D:/Offline/profile.tar.gz", phase: "failed", cleanup_pending: true, offline_contents };
    const summary = operationSummaries({ updates: { operation } })[0];
    assert.equal(summary.module, "versions");
    assert.equal(summary.anchor, "offline-packages");
    assert.deepEqual(operationRetryCommand(operation, "official", "portable"), { action: kind, archive_path: operation.archive_path, offline_contents });
  }
  assert.equal(operationRetryCommand({kind:"offline_export",archive_path:"D:/all.tar.gz"}, "official", "portable"), null);
});

test("operation toasts ignore saved result hydration but announce observed completion once", () => {
  const rows = (phase: string, ready: boolean) => operationSummaries({ startup: { available: true }, releases: ready ? { releases: [{ id: "slot" }] } : {}, updates: { operation: { operation_id: "one", phase, release_id: "slot" } } });
  let result = advanceOperationNotices(rows("succeeded", false), new Map());
  assert.equal(result.notices.length, 0);
  result = advanceOperationNotices(rows("succeeded", true), result.pending);
  assert.equal(result.notices.length, 0, "loading a saved release catalog must not replay completion");
  result = advanceOperationNotices(rows("building", true), result.pending);
  result = advanceOperationNotices(rows("succeeded", false), result.pending);
  assert.equal(result.notices.length, 0, "wait for catalog verification before announcing success");
  result = advanceOperationNotices(rows("succeeded", true), result.pending);
  assert.deepEqual(result.notices.map(item => item.status), ["Completed"]);
  result = advanceOperationNotices(rows("succeeded", false), result.pending);
  result = advanceOperationNotices(rows("succeeded", true), result.pending);
  assert.equal(result.notices.length, 0, "catalog refresh must not repeat the toast");
  result = advanceOperationNotices(rows("building", true), result.pending);
  result = advanceOperationNotices(rows("failed", true), result.pending);
  assert.deepEqual(result.notices.map(item => item.status), ["Failed"]);
});

test("operation summaries prioritize unresolved failures and never treat cleanup or missing slots as success", () => {
  const snapshot = { startup: { available: true }, releases: { releases: [] }, updates: {
    install_operation: { operation_id: "normal", phase: "succeeded", release_id: "missing", cleanup_pending: true, error: "original error", cleanup_error: "cleanup error" },
    operation: { operation_id: "cold", phase: "building", progress_percent: 61 },
  }, checkpoints: { pending_restore: { checkpoint_id: "restore", state: "committed", retryable: true }, last_capture: { id: "capture", state: "succeeded" } },
  maintenance: { result: { preview_id: "clean", state: "completed", items: [{ state: "failed", error: "cannot delete" }] } } };
  const rows = operationSummaries(snapshot);
  assert.deepEqual(rows.map(row => row.status), ["Cleanup required", "Recovery required", "Partially failed", "In progress", "Completed"]);
  assert.match(rows[0].error, /original error\ncleanup error/);
  assert.equal(rows.find(row => row.title === "Cold switch")?.progress, 61);
  assert.equal(rows.find(row => row.title === "Snapshot capture")?.progress, undefined);
  assert.equal(operationSummaries({ ...snapshot, updates: { install_operation: { operation_id: "normal", phase: "succeeded", release_id: "missing" } } })[0].status, "Installed version unavailable");
  assert.deepEqual(operationSummaries(JSON.parse(JSON.stringify(snapshot))), rows);
  assert.match(operationResponseNotice("/v1/checkpoints", { restored: false, pending_restore: {} })!, /requires attention/);
  assert.match(operationResponseNotice("/v1/updates", { install_operation: { cleanup_pending: true } })!, /incomplete/);
  assert.equal(operationResponseNotice("/v1/checkpoints", { restored: true }), undefined);
});

test("source editing preserves custom build and verify configuration across polling and failed saves", () => {
  const saved = { source: "old", ref_name: "custom-ref", git_program: "git-custom", build_program: "build-custom", build_args: ["one", "two"], verify_program: "verify-custom", verify_args: ["check"], timeout_secs: 987 };
  const draft = { value: " new ", dirty: true };
  assert.deepEqual(refreshEditableDraft(draft, "external-update"), draft);
  assert.deepEqual(finishDraftSave(draft, false), draft);
  assert.deepEqual(updateSourcePayload(draft.value), { source: "new" });
  assert.deepEqual(refreshEditableDraft(finishDraftSave(draft, true), "new"), { value: "new", dirty: false });
  assert.equal(saved.source, "old");
});

test("completed install waits for a current available catalog and notice icons reflect acceptance or recovery", () => {
  const snapshot = { startup: { available: true }, releases: { releases: [{ id: "slot" }] }, updates: { operation: { operation_id: "done", phase: "succeeded", release_id: "slot" } } };
  assert.equal(operationSummaries(snapshot)[0].status, "Completed");
  for (const patch of [{ releases: null }, { releases: {} }, { lifecycleBusy: true }, { startup: { available: false } }, { endpointErrors: { "/v1/releases": "unavailable" } }]) {
    assert.equal(operationSummaries({ ...snapshot, ...patch })[0].status, "Verification pending");
    assert.equal(operationSummaries({ ...snapshot, ...patch })[0].progress, undefined);
  }
  assert.equal(operationSummaries({ ...snapshot, lifecycleBusy: true, releases: { releases: [] } })[0].status, "Verification pending");
  assert.equal(operationSummaries({ ...snapshot, releases: { releases: [] } })[0].status, "Installed version unavailable");
  assert.equal(operationNoticeKind("complete"), "success");
  assert.equal(operationNoticeKind("Installation request accepted. Follow the current stage below to confirm completion."), "info");
  assert.equal(operationNoticeKind("Cancellation requested. Waiting for cleanup to finish."), "info");
  assert.equal(operationNoticeKind(operationResponseNotice("/v1/checkpoints", { pending_restore: {} })!), "warning");
  assert.equal(operationNoticeKind(operationResponseNotice("/v1/updates", { operation: { cleanup_pending: true } })!), "warning");
  assert.equal(operationNoticeKind("complete", { manual: true }), "warning");
  assert.equal(operationNoticeKind("complete", { manual: false }), "success");
});
