import assert from "node:assert/strict";
import test from "node:test";
import { operationSummaries, operationRetryCommand } from "../src/operation-status.ts";
import { offlineArchivePathValid, offlinePackageCommand, refreshEditableDraft } from "../src/settings-state.ts";
import { actionNoticeKey, invalidatesHarnessCredentials } from "../src/control-state.ts";

test("offline commands use absolute archive paths, retain draft selections, and invalidate credentials only for import", () => {
  for (const path of ["D:/Offline/harness.tar.gz", String.raw`\\server\share\package.tar.gz`, " C:/Offline/package.TAR.GZ "]) assert.equal(offlineArchivePathValid(path), true, path);
  for (const path of ["relative.tar.gz", "C:relative.tar.gz", "D:/wrong.zip", "D:/bad\npath.tar.gz"]) assert.equal(offlineArchivePathValid(path), false, path);
  assert.deepEqual(offlinePackageCommand("offline_import", " D:/package.tar.gz "), { action: "offline_import", archive_path: "D:/package.tar.gz" });
  assert.deepEqual(offlinePackageCommand("offline_export", " D:/package.tar.gz ", "old-slot"), { action: "offline_export", archive_path: "D:/package.tar.gz", release_id: "old-slot" });
  assert.deepEqual(refreshEditableDraft({ value: "old-slot", dirty: true }, "new-current"), { value: "old-slot", dirty: true });
  assert.equal(invalidatesHarnessCredentials("/v1/updates", "offline_import"), true);
  assert.equal(invalidatesHarnessCredentials("/v1/updates", "offline_export"), false);
  for (const action of ["offline_import", "offline_export"]) assert.match(actionNoticeKey("/v1/updates", action), /request accepted/);
});

test("offline export completion and retry remain distinct from slot installation", () => {
  const operation = { operation_id: "offline", kind: "offline_export", archive_path: "D:/saved.tar.gz", release_id: "old-slot", phase: "succeeded" };
  const summary = operationSummaries({ updates: { operation } })[0];
  assert.equal(summary.title, "Offline package export"); assert.equal(summary.status, "Completed"); assert.equal(summary.archivePath, operation.archive_path);
  const cleanup = operationSummaries({ updates: { operation: { ...operation, cleanup_pending: true, cleanup_error: "cleanup denied" } } })[0];
  assert.equal(cleanup.status, "Package exported; cleanup required"); assert.equal(cleanup.archivePath, operation.archive_path); assert.match(cleanup.error, /cleanup denied/);
  assert.equal(operationSummaries({ updates: { operation: { ...operation, kind: "offline_import" } } })[0].status, "Verification pending");
  assert.deepEqual(operationRetryCommand(operation, "official", "portable"), { action: "offline_export", archive_path: operation.archive_path, release_id: "old-slot" });
  assert.deepEqual(operationRetryCommand({ ...operation, kind: "offline_import" }, "official", "portable"), { action: "offline_import", archive_path: operation.archive_path });
  assert.equal(operationRetryCommand({ ...operation, kind: "future_operation" }, "official", "portable"), null);
  assert.deepEqual(operationRetryCommand({ tag: "tag" }, "official", "portable"), { action: "switch", tag: "tag", source: "official", mode: "portable" });
});
