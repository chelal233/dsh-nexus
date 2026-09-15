import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { actionCallback } from "./ab-actions.mjs";

// Exercise the actual refresh callback, with observable state writes and controlled IPC.
export async function compareRefresh(original, current, esbuild, load) {
  const callback = /  const refresh = useCallback\(async \(\) => \{[\s\S]*?\n  \}, \[t\]\);/;
  const emit = (source) =>
    esbuild.transformSync(source, { loader: "tsx", minifyWhitespace: true }).code;
  const sources = [original, current].map((root) =>
    readFileSync(path.join(root, "App.tsx"), "utf8"),
  );
  for (const source of sources) assert.ok(callback.test(source), "Refresh callback must be found");
  assert.equal(
    emit(sources[0].replace(callback, "").replace(actionCallback, "")),
    emit(sources[1].replace(callback, "").replace(actionCallback, "")),
    "Add probes before changing other App behavior",
  );
  const modules = await Promise.all(
    [original, current].map(async (root) =>
      Object.assign(
        {},
        await load(root, "control-state.ts"),
        await load(root, "json-values.ts"),
        await load(root, "display-format.ts"),
        await load(root, "harness-session.ts"),
      ),
    ),
  );
  async function observe(index, scenario, coalesced, invalidating) {
    const source = sources[index];
    const constants = source.slice(
      source.indexOf("const emptySnapshot:"),
      source.indexOf("function storedTheme"),
    );
    assert.ok(constants.includes("const endpointMap:"));
    const trace = [];
    let snapshot = {
      health: { instance_id: "agent", data_root_id: "root" },
      harnessUi: { available: true },
    };
    let unblock;
    const gate = new Promise((resolve) => {
      unblock = resolve;
    });
    let requests = 0;
    const context = vm.createContext({
      ...modules[index],
      useCallback: (fn) => fn,
      t: (value) => value,
      refreshInFlight: { current: null },
      refreshPending: { current: false },
      harnessPollState: { current: undefined },
      credentialInvalidation: { current: invalidating ? { previousSessionKey: "1:old" } : null },
      commandStartupStatus: async () => {
        trace.push(["startup"]);
        if (++requests === 1) await gate;
        if (scenario === "bridge-error") throw "bridge unavailable";
        return { available: scenario !== "offline", data_root_id: "root" };
      },
      proxyRequest: async (route) => {
        trace.push(["GET", route]);
        if (scenario === "endpoint-error" && route.endsWith("config")) throw "config failed";
        if (scenario.startsWith("busy") && route.endsWith("harness"))
          throw "NEXUS_LIFECYCLE_BUSY: fixture";
        if (route.endsWith("health"))
          return {
            instance_id: scenario === "busy-new-owner" ? "new" : "agent",
            data_root_id: "root",
          };
        if (route.endsWith("harness"))
          return {
            harness: { state: "running", pid: 42 },
            generation: 2,
            log_session_run_id: "new",
          };
        if (route.endsWith("harness/ui")) return { available: true, generation: 2, run_id: "new" };
        return scenario === "null-payload" ? null : { api_version: "v1" };
      },
      setSnapshot: (value) => {
        snapshot = typeof value === "function" ? value(snapshot) : value;
        trace.push(["snapshot", structuredClone(snapshot)]);
      },
      ...Object.fromEntries(
        [
          "setLoading",
          "setNotice",
          "setBridgeError",
          "setAgentUnavailable",
          "setCredentialInvalidationPending",
        ].map((name) => [name, (value) => trace.push([name, value])]),
      ),
    });
    vm.runInContext(
      emit(constants + source.match(callback)[0] + "\nglobalThis.run = refresh;"),
      context,
    );
    const first = context.run();
    const queued = coalesced ? [context.run(), context.run()] : [];
    unblock();
    await Promise.all([first, ...queued]);
    assert.equal(requests, coalesced ? 2 : 1, "Concurrent refreshes must coalesce");
    return {
      trace,
      snapshot,
      poll: context.harnessPollState.current,
      credentials: context.credentialInvalidation.current,
      pending: context.refreshPending.current,
      idle: context.refreshInFlight.current === null,
    };
  }
  let cases = 0;
  for (const scenario of [
    "online",
    "offline",
    "bridge-error",
    "endpoint-error",
    "busy",
    "busy-new-owner",
    "null-payload",
  ])
    for (const coalesced of [false, true])
      for (const invalidating of [false, true]) {
        assert.deepEqual(
          structuredClone(await observe(1, scenario, coalesced, invalidating)),
          structuredClone(await observe(0, scenario, coalesced, invalidating)),
          scenario,
        );
        cases++;
      }
  return cases;
}
