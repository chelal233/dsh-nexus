import { test } from "node:test";
import assert from "node:assert/strict";
import { apiErrorInfo, recoverableNoop, errorWithExplanation, workspaceFailureKind, workspaceRepairTarget } from "../src/api-errors.ts";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("workspace recovery covers all modules and does not mistake transient failures for damaged files", () => {
  assert.equal(workspaceRepairTarget("/v1/profiles"), "profiles");
  for (const route of ["releases", "updates"]) assert.equal(workspaceRepairTarget(`/v1/${route}`), "versions");
  for (const route of ["checkpoints", "diagnostics", "maintenance", "recovery", "state"]) assert.equal(workspaceRepairTarget(`/v1/${route}`), "maintenance");
  assert.equal(workspaceRepairTarget("/v1/config"), "settings");
  assert.equal(workspaceFailureKind(apiErrorInfo('expected `,` or `]` at line 15 column 9')), "invalid_data");
  assert.equal(workspaceFailureKind(apiErrorInfo({kind:"permission_denied",message:"denied"})), "permission_denied");
  assert.equal(workspaceFailureKind(apiErrorInfo({retryable:true,message:"unavailable"})), "unavailable");
  assert.equal(workspaceFailureKind(apiErrorInfo("unexplained failure")), "other");
});

test("ambiguous local failures preserve raw errors without speculative repair advice", async () => {
  const loader = await createUiTestLoader();
  try {
    const { explainBackendError } = await loader.loadModule("/src/App.tsx");
    for (const raw of [
      "Harness was killed after the graceful stop timeout",
      "Harness readiness timed out",
      "Offline package: EISDIR: illegal operation on a directory, lstat 'C:'",
    ]) assert.equal(explainBackendError(raw, (key: string, values?: { message: string }) =>
      values ? key.replace("{message}", values.message) : key), `Backend error: ${raw}`);
  } finally { await loader.close(); }
});
test("structured error behavior survives translated messages and does not reinterpret another code", () => {
  assert.equal(recoverableNoop({code:"harness_already_running",message:"已在运行"}),true);
  assert.equal(recoverableNoop({code:"harness_spawn_failed",message:"not configured"}),false);
  assert.equal(apiErrorInfo({code:"config_revision_conflict",message:"changed",actions:["reload_config",7]}).actions[0],"reload_config");
});

test("external source capacity failure has an actionable translation key", async () => {
  const loader = await createUiTestLoader();
  try {
    const { explainBackendError } = await loader.loadModule("/src/App.tsx");
    const raw = "External source protection history is full (1 MiB). Keep the current source or choose a previously confirmed directory, then save again. Existing directory protection is retained.";
    const key = "External source protection history is full. Keep the current source or choose a previously confirmed directory, then save again. Existing directory protection is retained.";
    assert.equal(explainBackendError(raw, (value: string) => { assert.equal(value,key); return "已译：保留当前来源或选择已确认的目录"; }),"已译：保留当前来源或选择已确认的目录");
  } finally { await loader.close(); }
});
test("an explanation always preserves original diagnostic text", () => {
  const raw = "Error: native build failed\n at example.ts:12\ngyp ERR! find Python";
  assert.ok(errorWithExplanation(raw,"Dependency build failed","Original error").endsWith(raw));
  assert.equal(errorWithExplanation(raw,raw,"Original error"),raw);
});
