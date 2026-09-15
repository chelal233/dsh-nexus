import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import vm from "node:vm";

export const actionCallback =
  /  const runAction = useCallback\([\s\S]*?\n    \[busyAction, refresh, snapshot, t\],\n  \);/;

// Run the real callback with identical transport results and record ordered UI effects.
export async function compareActions(original, current, esbuild, load) {
  const implementations = await Promise.all(
    [original, current].map(async (root) => {
      const callback = readFileSync(path.join(root, "App.tsx"), "utf8").match(actionCallback)?.[0];
      assert.ok(callback, "Action callback must be found");
      const code = esbuild.transformSync(callback + "\nglobalThis.run = runAction;", {
        loader: "tsx",
      }).code;
      const helpers = Object.assign(
        {},
        ...(await Promise.all(
          [
            "control-state.ts",
            "json-values.ts",
            "display-format.ts",
            "harness-session.ts",
            "api-errors.ts",
            "operation-status.ts",
            "settings-state.ts",
          ].map((file) => load(root, file)),
        )),
      );
      return { code: new vm.Script(code), helpers };
    }),
  );
  const base = {
    startup: { available: true },
    health: {},
    recovery: {},
    config: {},
    releases: { current_release: "fixture" },
    harnessRuntime: {
      harness: { state: "running", pid: 42 },
      generation: 1,
      log_session_run_id: "run",
    },
    harnessUi: { available: true, generation: 1, run_id: "run" },
  };
  const snapshots = [
    base,
    { ...base, health: { degraded: true } },
    { ...base, startup: null },
    { ...base, startup: { available: false, message: "offline" } },
    { ...base, lifecycleBusy: true },
    { ...base, releases: {} },
    { ...base, releases: {}, recovery: { paused: true } },
    { ...base, config: { external_harness: { root: "C:/fixture/harness" } } },
  ];
  const responses = [
    {},
    { request: { state: "running" } },
    { request: { state: "completed", http_status: 202 } },
    { request: { state: "completed", http_status: 200 } },
    { operation: { phase: "failed", error: "fixture" } },
    { install_operation: { phase: "running" } },
    { export_path: "C:/fixture/report.zip" },
    { export_path: "C:/fixture/report.zip", reveal_error: "manual" },
  ];
  const errors = [
    "already running",
    "Harness is not configured",
    "",
    null,
    { code: "harness_preflight_blocked", message: "blocked", preflight: {} },
    {
      code: "harness_preflight_blocked",
      message: "blocked",
      preflight: {
        api_version: "v1",
        ready: false,
        paused: false,
        checked_at_unix: 1,
        checks: [{ id: "profile", status: "blocked", reason: "fixture", next: "repair" }],
      },
    },
    { code: "config_revision_conflict", message: "changed", actions: ["retry", 7] },
    { code: "harness_start_paused", message: "paused" },
    ...[
      "harness_start_cancelled",
      "harness_operation_busy",
      "harness_already_running",
      "harness_already_stopped",
      "harness_unattached",
      "harness_not_configured",
      "other",
    ].map((code) => ({ code, message: "Harness is not configured" })),
  ];
  const outcomes = [
    ...responses.map((value) => ({ fail: false, value })),
    ...errors.map((value) => ({ fail: true, value })),
  ];
  const commands = Object.entries({
    harness: ["start", "restart", "stop", "unknown", null, 1, ["start"]],
    agent: ["start", "restart", "stop"],
    profiles: ["select", "compatibility_check"],
    releases: ["promote", "rollback"],
    updates: ["switch", "confirm", "offline_import", "retry", "cancel", "clear_finished"],
    diagnostics: ["export"],
    config: ["save"],
    recovery: ["enter"],
  }).flatMap(([route, actions]) => actions.map((action) => [`/v1/${route}`, { action }]));

  async function observe(implementation, fixture) {
    const { code, helpers } = implementation;
    const {
      route,
      body,
      outcome,
      snapshot,
      preview = false,
      busy = null,
      inFlight = false,
      refreshFails = false,
    } = fixture;
    const trace = [];
    let currentSnapshot = structuredClone(snapshot);
    const context = vm.createContext({
      ...helpers,
      useCallback: (callback) => callback,
      t: (key, values) => (values ? `${key} ${JSON.stringify(values)}` : key),
      snapshot: currentSnapshot,
      busyAction: busy,
      isBrowserPreview: preview,
      actionInFlight: { current: inFlight },
      credentialInvalidation: { current: null },
      flushSync: (callback) => {
        trace.push(["flushSync"]);
        callback();
      },
      postAction: async (...args) => {
        trace.push(["POST", ...args]);
        if (outcome.fail) throw structuredClone(outcome.value);
        return structuredClone(outcome.value);
      },
      invoke: async (...args) => {
        trace.push(["invoke", ...args]);
        if (outcome.fail) throw structuredClone(outcome.value);
        return structuredClone(outcome.value);
      },
      refresh: async () => {
        trace.push(["refresh"]);
        if (refreshFails) throw "refresh failed";
      },
      setSnapshot: (update) => {
        currentSnapshot = typeof update === "function" ? update(currentSnapshot) : update;
        trace.push(["setSnapshot", structuredClone(currentSnapshot)]);
      },
      ...Object.fromEntries(
        [
          "setError",
          "setNotice",
          "setBusyAction",
          "setActiveModule",
          "setCheckOpen",
          "setBasicCheckError",
          "setBasicCheckResult",
          "setCheckPending",
          "setCredentialInvalidationPending",
          "setErrorGuidance",
        ].map((name) => [name, (...args) => trace.push([name, ...args])]),
      ),
    });
    code.runInContext(context);
    const first = context.run("Action", route, structuredClone(body));
    // The UI has not rerendered yet: only the immediate ref can block this duplicate.
    const second = context.run("Action", route, structuredClone(body));
    const results = await Promise.allSettled([first, second]);
    return structuredClone({
      trace,
      results,
      currentSnapshot,
      inFlight: context.actionInFlight.current,
      credentials: context.credentialInvalidation.current,
    });
  }
  let cases = 0;
  const coverage = new Set();
  async function compare(fixture) {
    const a = await observe(implementations[0], fixture);
    const b = await observe(implementations[1], fixture);
    assert.deepEqual(
      b,
      a,
      JSON.stringify({ route: fixture.route, body: fixture.body, case: cases }),
    );
    for (const [event, value] of a.trace) {
      coverage.add(event);
      if (event === "setBasicCheckResult" && value?.api_version === "v1")
        coverage.add("valid-preflight");
      if (event === "setNotice" && value?.startsWith("Version-slot operation accepted"))
        coverage.add("external-harness");
    }
    cases++;
  }
  for (const [route, body] of commands)
    for (const snapshot of snapshots)
      for (const outcome of outcomes) {
        await compare({ route, body, snapshot, outcome });
      }
  for (const [route, body] of commands)
    for (const extra of [
      { preview: true },
      { busy: "Other action" },
      { inFlight: true },
      { refreshFails: true },
    ])
      await compare({ route, body, snapshot: base, outcome: outcomes[0], ...extra });
  assert.ok(
    coverage.has("flushSync") &&
      coverage.has("POST") &&
      coverage.has("invoke") &&
      coverage.has("setErrorGuidance"),
  );
  assert.ok(coverage.has("valid-preflight") && coverage.has("external-harness"));
  return {
    cases,
    compared: "Ordered state writes, transport calls, duplicate requests, return values and errors",
  };
}
