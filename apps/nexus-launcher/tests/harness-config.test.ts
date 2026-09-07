import assert from "node:assert/strict";
import test from "node:test";

import { createUiTestLoader } from "./ui-test-loader.ts";

test("Harness config keeps legacy direct arguments unchanged", async () => {
  const loader = await createUiTestLoader();
  try {
    const { harnessDraftFromConfig } = await loader.loadModule("/src/App.tsx");
    assert.deepEqual(
      harnessDraftFromConfig({
        harness: {
          program: "deepseek-harness.exe",
          args: ["--profile", "web"],
        },
      }),
      {
        mode: "direct",
        program: "deepseek-harness.exe",
        entry: "",
        args: "--profile\nweb",
        workingDir: "",
        readinessUrl: "",
        timeout: "",
        readinessTokenRequired: false,
        readinessUrlRedacted: false,
        argsRedacted: false,
        replaceRedactedArgs: false,
      },
    );
  } finally {
    await loader.close();
  }
});

test("Node candidates expose entry separately while preserving additional args", async () => {
  const loader = await createUiTestLoader();
  try {
    const { harnessCandidates, harnessConfigPayloadFromDraft, harnessDraftFromConfig, isLoopbackReadinessTarget } = await loader.loadModule("/src/App.tsx");
    const [candidate] = harnessCandidates({
      api_version: "v1",
      candidates: [{
        id: "node:one",
        mode: "node",
        program: "node.exe",
        entry: "dist/index.js",
        args: ["--port", "3080"],
        working_dir: "C:/dsh",
        source: "configured",
        display_name: "deepseek-harness",
        version: "rc.1",
        readiness_timeout_secs: 30,
        readiness_token_required: true,
      }],
    });
    assert.equal(candidate.mode, "node");
    assert.equal(candidate.entry, "dist/index.js");
    assert.deepEqual(candidate.args, ["--port", "3080"]);
    assert.equal(candidate.readinessTimeout, "30");
    assert.equal(candidate.readinessTokenRequired, true);

    const draft = harnessDraftFromConfig({
      harness_readiness_url_redacted: true,
      harness: {
        mode: "node",
        program: "node.exe",
        entry: "dist/index.js",
        args: ["--port", "3080"],
        readiness_url: "tcp://127.0.0.1:3080",
        readiness_token_required: true,
      },
    });
    assert.equal(draft.mode, "node");
    assert.equal(draft.entry, "dist/index.js");
    assert.equal(draft.args, "--port\n3080");
    assert.equal(draft.readinessUrlRedacted, true);
    assert.equal(draft.readinessTokenRequired, true);
    assert.deepEqual(harnessConfigPayloadFromDraft(draft), {
      mode: "node",
      program: "node.exe",
      entry: "dist/index.js",
      args: ["--port", "3080"],
      args_are_additional: true,
      working_dir: null,
      readiness_url: "tcp://127.0.0.1:3080",
      readiness_timeout_secs: null,
      readiness_token_required: true,
    });
    assert.equal(isLoopbackReadinessTarget("http://127.0.0.1:3080/health"), true);
    assert.equal(isLoopbackReadinessTarget("HTTP://127.0.0.1:3080/health"), true);
    assert.equal(isLoopbackReadinessTarget("http://127.0.0.1:3080?token=secret"), true);
    assert.equal(isLoopbackReadinessTarget("http://127.0.0.1:3080/#fragment"), false);
    assert.equal(isLoopbackReadinessTarget("http://127.0.0.1:0/health"), false);
    assert.equal(isLoopbackReadinessTarget("tcp://127.0.0.1:3080"), true);
    assert.equal(isLoopbackReadinessTarget("tcp://127.0.0.1"), false);
    assert.equal(isLoopbackReadinessTarget("tcp://127.0.0.1:3080/health"), false);
  } finally {
    await loader.close();
  }
});
