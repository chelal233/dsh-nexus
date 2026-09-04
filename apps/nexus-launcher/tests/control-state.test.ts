import assert from "node:assert/strict";
import test from "node:test";

import {
  failClosedSnapshot,
  harnessControlGate,
  invalidatesHarnessCredentials,
  launcherContentMode,
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
