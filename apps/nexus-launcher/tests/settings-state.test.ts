import assert from "node:assert/strict";
import test from "node:test";
import { refreshEditableDraft, finishDraftSave, replacementArgumentRows, harnessFailureKeys, diagnosticExportResult, homePreferencesPayload, launchInputMatches } from "../src/settings-state.ts";
import { createFailureNoticeTracker } from "../src/control-state.ts";
import { cleanupSelectedIds, cleanupGroups, toggleCleanupGroup } from "../src/settings-state.ts";
import { offlineImportDefaults, offlinePackageCommand, releasePromotionCommand } from "../src/settings-state.ts";

test("manual version recovery requires explicit consent and forwards the bound confirmation", () => {
  assert.equal(releasePromotionCommand("new", "revision-bound-token", false), null);
  assert.deepEqual(releasePromotionCommand("new", "revision-bound-token", true), { action: "promote", id: "new", rollback_confirmation: "revision-bound-token" });
  assert.deepEqual(releasePromotionCommand("new", null, false), { action: "promote", id: "new" });
});

test("import preview never opts into credentials or replacement on the user's behalf", () => {
  const available = { runtime: false, profiles: ["web"], configuration: true, plugins: true, environment: true, sessions: true, credentials: true, credential_policy: "replace" };
  const selected = offlineImportDefaults(available);
  assert.equal(selected.credentials, false); assert.equal(selected.credential_policy, "preserve");
  assert.equal(selected.runtime, false); assert.deepEqual(selected.profiles, ["web"]);
  const optedIn = { ...selected, credentials: true, credential_policy: "replace" };
  const request = { ...offlinePackageCommand("offline_import", " C:\\import.tar.gz "), offline_contents: optedIn };
  assert.equal(request.archive_path, "C:\\import.tar.gz");
  assert.equal(request.offline_contents.credential_policy, "replace");
  assert.equal(offlineImportDefaults(available).credentials, false, "another preview resets consent");
});

test("cleanup tree assigns each item once to its nearest directory root", () => {
  const groups=cleanupGroups([{path:"C:\\Nexus"},{path:"C:\\Nexus\\logs"}], [
    {id:"log",path:"c:/nexus/logs/a.log"}, {id:"slot",path:"C:/Nexus/releases/a"},
    {id:"outside",path:"C:/Nexus-extra/a"},
  ]);
  assert.deepEqual(groups.map(group=>group.items.map(item=>item.id)), [["slot"],["log"],["outside"]]);
});

test("category selection excludes protected items and preserves other categories", () => {
  const items=[{id:"old",eligible:true},{id:"active",eligible:false},{id:"unknown"}];
  assert.deepEqual(toggleCleanupGroup(["elsewhere"],items,true),["elsewhere","old"]);
  assert.deepEqual(toggleCleanupGroup(["elsewhere","old"],items,true),["elsewhere","old"]);
  assert.deepEqual(toggleCleanupGroup(["elsewhere","old"],items,false),["elsewhere"]);
});

test("cleanup item IDs cannot carry a selection into a different preview", () => {
  const selection = { previewId: "preview-a", ids: ["item-0"] };
  // The first item can name a completely different directory after a scan.
  assert.deepEqual(cleanupSelectedIds(selection, "preview-b"), []);
  assert.deepEqual(cleanupSelectedIds(selection, undefined), []);
  assert.deepEqual(cleanupSelectedIds(selection, ""), []);
  assert.deepEqual(cleanupSelectedIds(selection, "preview-a"), ["item-0"], "refreshing the same preview preserves the choice");
});

test("cleanup selection stays invalid after a failed preview request loads a newer saved result", async () => {
  const selection = { previewId: "preview-a", ids: ["item-0"] };
  let status = { preview: { preview_id: "preview-a", items: [{ id: "item-0", name: "old-slot" }] } };
  const savedResult = { preview: { preview_id: "preview-b", items: [{ id: "item-0", name: "different-slot" }] } };
  try {
    await Promise.reject(new Error("scan wait deadline exceeded"));
  } catch {
    status = await Promise.resolve(savedResult);
  }
  assert.deepEqual(cleanupSelectedIds(selection, status.preview.preview_id), []);
  // A deliberate new selection becomes valid only for the newly displayed preview.
  assert.deepEqual(cleanupSelectedIds({ previewId: status.preview.preview_id, ids: ["item-0"] }, status.preview.preview_id), ["item-0"]);
});

test("setup home change preserves preferences and empty input inherits", () => {
  const saved = { home: "D:/old", port: 0, open_browser: false };
  assert.deepEqual(homePreferencesPayload(saved, " E:/new "), { ...saved, home: "E:/new" });
  assert.deepEqual(homePreferencesPayload(saved, "  "), { ...saved, home: null });
  assert.equal(saved.home, "D:/old");
});

test("current launch inputs are hidden across stop, restart and stale response races", () => {
  const record = { generation: 3, run_id: "three" };
  const runtime = { generation: 3, log_session_run_id: "three", harness: { state: "running" } };
  assert.equal(launchInputMatches(record, runtime), true);
  assert.equal(launchInputMatches(record, { ...runtime, harness: { state: "stopped" } }), false);
  assert.equal(launchInputMatches(record, { ...runtime, generation: 4 }), false);
  assert.equal(launchInputMatches(record, { ...runtime, log_session_run_id: "new" }), false);
  assert.equal(launchInputMatches({}, runtime), false);
});

test("diagnostic export requires a usable path and preserves success when folder reveal fails", () => {
  for (const export_path of [undefined, null, "", "  ", 42, "bad\npath"]) {
    assert.throws(() => diagnosticExportResult({ export_path }));
  }
  assert.deepEqual(diagnosticExportResult({ export_path: "C:/Nexus/diagnostics.json" }), { path: "C:/Nexus/diagnostics.json", manual: false });
  assert.deepEqual(diagnosticExportResult({ export_path: "C:/Nexus/diagnostics.json", reveal_error: "Explorer unavailable" }), { path: "C:/Nexus/diagnostics.json", manual: true });
});

test("runtime polling preserves edits, failed save retains them, successful save refreshes", () => {
  const draft = { value: { node: "C:/edited/node.exe", source: "npmmirror" }, dirty: true };
  const server = { node: "C:/server/node.exe", source: "official" };
  let current = draft;
  for (let poll = 0; poll < 3; poll++) current = refreshEditableDraft(current, { ...server });
  assert.equal(current, draft);
  assert.equal(finishDraftSave(current, false), draft);
  current = finishDraftSave(current, true);
  assert.deepEqual(refreshEditableDraft(current, server), { value: server, dirty: false });
});

test("sensitive argument replacement starts a complete editable list without placeholders", () => {
  const hidden = [{ key: "--token", value: "[REDACTED]" }];
  const replacement = replacementArgumentRows(hidden, true);
  assert.deepEqual(replacement, []);
  replacement.push({ key: "--port", value: "8080" });
  assert.deepEqual(replacementArgumentRows(replacement, false), replacement);
  assert.deepEqual(replacementArgumentRows(replacement, true), []);
  assert.equal(hidden[0].value, "[REDACTED]", "original server data is not modified");
});

test("nested Harness running-to-failed notifies once and another run can fail again", () => {
  const tracker = createFailureNoticeTracker();
  const snapshot = (state: string, run = "one", updated = 1) => ({
    generation: 2, log_session_run_id: run, harness: { state, updated_at_unix: updated },
  });
  assert.equal(tracker.observe(harnessFailureKeys(snapshot("running"))), false);
  assert.equal(tracker.observe(harnessFailureKeys(snapshot("failed"))), true);
  assert.equal(tracker.observe(harnessFailureKeys(snapshot("failed", "one", 2))), false);
  assert.equal(tracker.observe([]), false);
  assert.equal(tracker.observe(harnessFailureKeys(snapshot("failed"))), false);
  assert.equal(tracker.observe(harnessFailureKeys(snapshot("running", "two"))), false);
  assert.equal(tracker.observe(harnessFailureKeys(snapshot("failed", "two"))), true);
  const history = createFailureNoticeTracker();
  assert.equal(history.observe(harnessFailureKeys(snapshot("failed"))), false);
});
