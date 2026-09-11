import assert from "node:assert/strict";
import test from "node:test";
import { githubRefKind, preferencesDraft, preferencesPayload } from "../src/harness-preferences.ts";
test("legacy and structured patches merge once with structured metadata preserved", () => {
  const entry={source:"C:/patch.yml",enabled:false,sha256:"a".repeat(64)};
  const draft=preferencesDraft({patches:[" C:/patch.yml ","C:/other.yml"],patch_entries:[entry]});
  assert.deepEqual(draft.patch_entries,[entry,{source:"C:/other.yml",enabled:true}]);
  assert.deepEqual(preferencesPayload(draft).value,{patch_entries:draft.patch_entries});
});

test("blank preferences inherit while false and ephemeral port remain explicit", () => {
  const draft = preferencesDraft({ port: 0, open_browser: false, telemetry_disabled: true });
  draft.home = "   ";
  assert.deepEqual(preferencesPayload(draft), { value: { open_browser: false, telemetry_disabled: true, port: 0 } });
  draft.open_browser = "";
  draft.port = "";
  draft.telemetry_disabled = "";
  assert.deepEqual(preferencesPayload(draft), { value: {} });
});

test("blank patch rows are omitted and explicit GitHub ref metadata is retained", () => {
  const draft = preferencesDraft({});
  draft.patch_entries = [{ source: " ", enabled: true }, { source: "", enabled: false }];
  assert.deepEqual(preferencesPayload(draft), { value: {} });
  draft.patch_entries = [{ source: "https://github.com/a/b/blob/main/config.yml", enabled: true, github_ref_kind: "branch", github_ref_name: "feature/x", github_file_path: "配置/extra patch.yml" }];
  assert.deepEqual(preferencesPayload(draft).value.patch_entries, draft.patch_entries);
});
test("GitHub permalink infers commit and explicit ref selection is preserved", () => {
  const entry = { source: `https://github.com/a/b/blob/${"a".repeat(40)}/a.yml`, enabled: true };
  assert.equal(githubRefKind(entry), "commit");
  assert.equal(githubRefKind({ ...entry, github_ref_kind: "tag" }), "tag");
  assert.equal(githubRefKind({ source: "https://github.com/a/b/blob/main/a.yml", enabled: true }), "branch");
});

test("preference paths are pointers, preserve patch order and never add a profile", () => {
  const draft = preferencesDraft({ home: "D:\\Harness 数据", patches: ["D:\\a.yml", "D:\\b.yml"], profile: "web" });
  draft.patch_entries.push({ source: " D:\\c.yml ", enabled: false });
  draft.patch_entries.reverse();
  assert.deepEqual(preferencesPayload(draft), { value: { home: "D:\\Harness 数据", patch_entries: [{ source: "D:\\c.yml", enabled: false }, { source: "D:\\b.yml", enabled: true }, { source: "D:\\a.yml", enabled: true }] } });
});

test("invalid numbers cannot become silent zero or unbounded overrides", () => {
  for (const port of ["-1", "65536", "1.5", "abc", "1e3"]) {
    assert.ok(preferencesPayload({ ...preferencesDraft({}), port }).error);
  }
  for (const context_window of ["0", "-1", "1.5", "9007199254740992"]) {
    assert.ok(preferencesPayload({ ...preferencesDraft({}), context_window }).error);
  }
});
