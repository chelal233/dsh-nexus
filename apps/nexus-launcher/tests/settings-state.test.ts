import assert from "node:assert/strict";
import test from "node:test";
import { refreshEditableDraft, finishDraftSave, replacementArgumentRows, harnessFailureKeys, diagnosticExportResult, homePreferencesPayload, launchInputMatches } from "../src/settings-state.ts";
import { createFailureNoticeTracker } from "../src/control-state.ts";

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
