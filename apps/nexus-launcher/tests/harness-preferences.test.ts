import assert from "node:assert/strict";
import test from "node:test";
import { preferencesDraft, preferencesPayload } from "../src/harness-preferences.ts";

test("blank preferences inherit while false and ephemeral port remain explicit", () => {
  const draft = preferencesDraft({ port: 0, open_browser: false, telemetry_disabled: true });
  draft.home = "   ";
  assert.deepEqual(preferencesPayload(draft), { value: { open_browser: false, telemetry_disabled: true, port: 0 } });
  draft.open_browser = "";
  draft.port = "";
  draft.telemetry_disabled = "";
  assert.deepEqual(preferencesPayload(draft), { value: {} });
});

test("preference paths are pointers, preserve patch order and never add a profile", () => {
  const draft = preferencesDraft({ home: "D:\\Harness 数据", patches: ["D:\\a.yml", "D:\\b.yml"], profile: "web" });
  draft.patches += "\n  \n D:\\c.yml ";
  assert.deepEqual(preferencesPayload(draft), { value: { home: "D:\\Harness 数据", patches: ["D:\\a.yml", "D:\\b.yml", "D:\\c.yml"] } });
});

test("invalid numbers cannot become silent zero or unbounded overrides", () => {
  for (const port of ["-1", "65536", "1.5", "abc", "1e3"]) {
    assert.ok(preferencesPayload({ ...preferencesDraft({}), port }).error);
  }
  for (const context_window of ["0", "-1", "1.5", "9007199254740992"]) {
    assert.ok(preferencesPayload({ ...preferencesDraft({}), context_window }).error);
  }
});
