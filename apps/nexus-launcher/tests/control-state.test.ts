import assert from "node:assert/strict";
import test from "node:test";

import {
  isLifecycleBusyError,
  lifecycleBusySnapshot,
  coldOperationIsTerminal,
  createLatestRequest,
  failClosedSnapshot,
  harnessControlGate,
  invalidatesHarnessCredentials,
  launcherContentMode,
  recoveryMutationGate,
  runtimeSettingsGate,
} from "../src/control-state.ts";

test("an unattached running Harness disables all lifecycle controls", () => {
  assert.deepEqual(harnessControlGate("running", undefined, false, true), {
    controlsDisabled: true,
    externallyManaged: true,
  });
  assert.deepEqual(harnessControlGate("running", 42, false, true), {
    controlsDisabled: false,
    externallyManaged: false,
  });
  assert.deepEqual(harnessControlGate("starting", undefined, false, true), {
    controlsDisabled: true,
    externallyManaged: false,
  });
  assert.deepEqual(harnessControlGate("starting", 42, false, true), {
    controlsDisabled: true,
    externallyManaged: false,
  });
  assert.deepEqual(harnessControlGate(undefined, undefined, false, true), {
    controlsDisabled: true,
    externallyManaged: false,
  });
  assert.deepEqual(harnessControlGate("invalid", 42, false, true), {
    controlsDisabled: true,
    externallyManaged: false,
  });
});

test("bridge failure clears every stale runtime and credential surface", () => {
  const empty = {
    startup: null,
    status: null,
    harnessRuntime: null,
    harnessUi: null,
    endpointErrors: {},
  };
  const cleared = failClosedSnapshot(empty);
  assert.notEqual(cleared, empty);
  assert.deepEqual(cleared, empty);
  assert.equal(cleared.startup, null);
  assert.equal(cleared.harnessRuntime, null);
  assert.equal(cleared.harnessUi, null);
});

test("bridge error renders no stale page content even when an old status existed", () => {
  assert.equal(launcherContentMode("bridge failed", false, true), "error");
  assert.equal(launcherContentMode(null, true, false), "loading");
  assert.equal(launcherContentMode(null, false, true), "content");
});

test("Harness and Agent lifecycle actions invalidate credentials before transport", () => {
  assert.equal(invalidatesHarnessCredentials("/v1/agent", "restart"), true);
  assert.equal(invalidatesHarnessCredentials("/v1/agent", "stop"), true);
  assert.equal(invalidatesHarnessCredentials("/v1/harness", "restart"), true);
  assert.equal(invalidatesHarnessCredentials("/v1/harness", "stop"), true);
  assert.equal(invalidatesHarnessCredentials("/v1/harness", "open"), false);
});

test("runtime settings gate mirrors stopped, idle, and cold cleanup prerequisites", () => {
  assert.equal(runtimeSettingsGate("running", 42, "idle", undefined, false, false).reason, "harness_not_stopped");
  assert.equal(runtimeSettingsGate("stopped", undefined, "running", undefined, false, false).reason, "update_active");
  assert.equal(runtimeSettingsGate("stopped", undefined, "idle", "installing", false, false).reason, "cold_active");
  assert.equal(runtimeSettingsGate("stopped", undefined, "idle", "failed", true, false).reason, "cleanup_pending");
  assert.deepEqual(runtimeSettingsGate("stopped", undefined, "idle", "failed", false, false), { disabled: false, reason: null });
});

test("latest request token rejects late refresh and cancelled responses", () => {
  const request = createLatestRequest();
  const first = request.begin();
  const second = request.begin();
  assert.equal(request.isCurrent(first), false);
  assert.equal(request.isCurrent(second), true);
  request.cancel();
  assert.equal(request.isCurrent(second), false);
});

test("cold terminal phases and recovery stopped gate are fail closed", () => {
  assert.equal(coldOperationIsTerminal("succeeded"), true);
  assert.equal(coldOperationIsTerminal("cancelled"), true);
  assert.equal(coldOperationIsTerminal("failed"), true);
  assert.equal(coldOperationIsTerminal("installing"), false);
  assert.deepEqual(recoveryMutationGate(false, "stopped", false), { disabled: false, reason: null });
  assert.deepEqual(recoveryMutationGate(true, "stopped", false), { disabled: true, reason: "stop_required" });
  assert.deepEqual(recoveryMutationGate(false, "running", false), { disabled: true, reason: "not_stopped" });
  assert.deepEqual(recoveryMutationGate(false, "failed", true), { disabled: true, reason: "busy" });
});


test("busy responses preserve only same-Agent catalogs and clear runtime credentials", () => {
  assert.equal(isLifecycleBusyError("Agent returned HTTP 409: NEXUS_LIFECYCLE_BUSY: switching"), true);
  assert.equal(isLifecycleBusyError("Agent returned HTTP 409: conflict"), false);
  assert.equal(isLifecycleBusyError("error sending request"), false);
  const previous = { profiles: { name: "desktop" }, releases: { id: "rc1" }, state: {}, harnessRuntime: {}, harnessUi: { url: "old-token" }, recovery: {}, updates: {} };
  const next = { ...previous, profiles: null, releases: null, updates: { progress: 50 } };
  const result = lifecycleBusySnapshot(next, previous, true);
  assert.equal(result.profiles, previous.profiles);
  assert.equal(result.releases, previous.releases);
  assert.equal(result.updates, next.updates);
  for (const key of ["state", "harnessRuntime", "harnessUi", "recovery"] as const) assert.equal(result[key], null);
  assert.equal(lifecycleBusySnapshot(next, previous, false).profiles, null);
  assert.equal(lifecycleBusySnapshot(next, previous, false).releases, null);
  for (const [path, action] of [["/v1/releases", "promote"], ["/v1/releases", "rollback"], ["/v1/updates", "switch"], ["/v1/updates", "confirm"]]) assert.equal(invalidatesHarnessCredentials(path, action), true);
});
