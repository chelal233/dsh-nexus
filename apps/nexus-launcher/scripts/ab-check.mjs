import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { compareRefresh } from "./ab-refresh.mjs";
import { compareActions } from "./ab-actions.mjs";

const app = fileURLToPath(new URL("../", import.meta.url));
const require = createRequire(import.meta.url);
const esbuild = createRequire(require.resolve("vite/package.json"))("esbuild");
const hash = (value) => createHash("sha256").update(value).digest("hex");
const [mode, directory] = process.argv.slice(2);
assert.ok(
  ["capture", "compare"].includes(mode) && directory,
  "Usage: node scripts/ab-check.mjs capture|compare <baseline-directory>",
);
const baseline = path.resolve(directory);
const current = path.join(app, "src");

function inventory(root) {
  return Object.fromEntries(
    readdirSync(root, { recursive: true, withFileTypes: true })
      .filter((entry) => entry.isFile())
      .map((entry) => path.join(entry.parentPath, entry.name))
      .sort()
      .map((file) => [path.relative(root, file).replaceAll("\\", "/"), hash(readFileSync(file))]),
  );
}

if (mode === "capture") {
  mkdirSync(baseline); // An existing baseline must never be silently replaced.
  cpSync(current, path.join(baseline, "src"), { recursive: true });
  writeFileSync(
    path.join(baseline, "baseline.json"),
    JSON.stringify(
      {
        capturedAt: new Date().toISOString(),
        files: inventory(path.join(baseline, "src")),
      },
      null,
      2,
    ),
  );
  console.log(`Captured immutable A source: ${baseline}`);
} else {
  const original = path.join(baseline, "src");
  const manifest = JSON.parse(readFileSync(path.join(baseline, "baseline.json"), "utf8"));
  assert.deepEqual(inventory(original), manifest.files, "A baseline was modified");
  const before = inventory(original),
    after = inventory(current);
  const changed = [...new Set([...Object.keys(before), ...Object.keys(after)])].filter(
    (file) => before[file] !== after[file],
  );
  const covered = new Set(["app-types.ts", "agent-bridge.ts", "harness-session.ts", "App.tsx"]);
  assert.ok(
    changed.every((file) => covered.has(file)),
    `Add A/B probes for changes outside this scope: ${changed.filter((file) => !covered.has(file))}`,
  );

  async function load(root, file, globals = {}) {
    const built = await esbuild.build({
      entryPoints: [path.join(root, file)],
      bundle: true,
      write: false,
      platform: "node",
      format: "cjs",
      logLevel: "silent",
      plugins: [
        {
          name: "controlled-native-bridge",
          setup(build) {
            build.onResolve({ filter: /^\.\/desktop$/ }, () => ({
              path: "core",
              namespace: "ab",
            }));
            build.onLoad({ filter: /.*/, namespace: "ab" }, () => ({
              contents: "export const invoke = (...args) => globalThis.__invoke(...args);",
            }));
          },
        },
      ],
    });
    const context = vm.createContext({ module: { exports: {} }, ...globals });
    vm.runInContext(built.outputFiles[0].text, context);
    return context.module.exports;
  }

  const a = await load(original, "harness-session.ts");
  const b = await load(current, "harness-session.ts");
  let sessionCases = 0;
  for (const state of ["running", "stopped", "detached", "failed", undefined, 1])
    for (const pid of [undefined, 0, -1, 42, "42", null, NaN])
      for (const generation of [undefined, 0, 1, "1", null, NaN])
        for (const runId of [undefined, "run", "", " ", 0, true, null])
          for (const recovered of [false, true])
            for (const pending of [false, true]) {
              const runtime = {
                harness: { state, pid },
                generation,
                log_session_run_id: runId,
                log_session_launch_pending: recovered,
              };
              const cases = [
                { available: true, generation, run_id: runId },
                { available: false, generation, run_id: runId },
                { available: true, generation: 99, run_id: runId },
                { available: true, generation, run_id: "other" },
                {},
                null,
                [],
                "invalid",
              ];
              for (const ui of cases) {
                const snapshot = { harnessRuntime: runtime, harnessUi: ui };
                const observe = (module) => [
                  module.harnessUiMatchesRuntime(runtime, ui, pending),
                  module.harnessSessionKey(snapshot),
                  ...[undefined, "0:run", "1:run"].map((previous) =>
                    module.credentialInvalidationCanSettle(snapshot, previous),
                  ),
                ];
                assert.deepEqual(observe(b), observe(a), `Session case ${sessionCases}`);
                sessionCases++;
              }
            }
  for (const runtime of [null, undefined, [], "invalid", {}, { state: "running", pid: 42 }]) {
    assert.equal(
      b.harnessUiMatchesRuntime(runtime, null),
      a.harnessUiMatchesRuntime(runtime, null),
    );
    sessionCases++;
  }

  async function bridgeTrace(root, native, health, fails) {
    const trace = [];
    const result = { accepted: true };
    const failure = { code: "agent_unavailable", status: 503, message: "fixture failure" };
    const module = await load(root, "agent-bridge.ts", {
      window: native ? { nexusDesktop: {} } : {},
      __invoke: async (...args) => {
        trace.push(["invoke", ...args]);
        if (fails) throw failure;
        return result;
      },
      fetch: async (...args) => {
        trace.push(["fetch", ...args]);
        return {
          ok: !fails,
          json: async () => (args[0] === "/agent/v1/health" ? health : fails ? failure : result),
        };
      },
    });
    for (const call of [
      () => module.commandStartupStatus(),
      () => module.proxyRequest("/v1/health"),
      () => module.proxyRequest("/v1/harness", "POST", { action: "restart" }),
    ]) {
      try {
        trace.push(["result", await call()]);
      } catch (error) {
        trace.push(["error", error]);
      }
    }
    return structuredClone(trace);
  }
  let bridgeCases = 0;
  for (const native of [false, true])
    for (const fails of [false, true])
      for (const health of [
        null,
        {},
        { status: "ok", data_root_id: "root", instance_id: "agent" },
        { status: "shutting_down" },
        { status: "ok", degraded: true, read_only: true },
      ]) {
        assert.deepEqual(
          await bridgeTrace(current, native, health, fails),
          await bridgeTrace(original, native, health, fails),
        );
        bridgeCases++;
      }

  // A type-only change must also emit exactly the same executable JavaScript.
  for (const file of ["app-types.ts", "agent-bridge.ts"]) {
    const emit = (root) =>
      esbuild.transformSync(readFileSync(path.join(root, file), "utf8"), {
        loader: "ts",
        minifyWhitespace: true,
      }).code;
    assert.equal(
      emit(current),
      emit(original),
      `Executable code changed in type-only file ${file}`,
    );
  }
  const refreshCases = changed.includes("App.tsx")
    ? await compareRefresh(original, current, esbuild, load)
    : 0;
  const actions = changed.includes("App.tsx")
    ? await compareActions(original, current, esbuild, load)
    : null;
  const report = {
    status: "passed",
    baseline: hash(JSON.stringify(before)),
    candidate: hash(JSON.stringify(after)),
    changed,
    sessionCases,
    bridgeCases,
    refreshCases,
    actions,
    typeOnlyJavaScriptIdentical: true,
    scope:
      "Local JSON state and controlled transport equivalence; not installer or real Harness acceptance",
  };
  writeFileSync(path.join(baseline, "comparison.json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report, null, 2));
  esbuild.stop();
}
