import assert from "node:assert/strict";
import test from "node:test";

import { createServer } from "vite";

test("Harness config keeps legacy direct arguments unchanged", async () => {
  const vite = await createServer({
    root: process.cwd(),
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  try {
    const { harnessDraftFromConfig } = await vite.ssrLoadModule("/src/App.tsx");
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
        argsRedacted: false,
        replaceRedactedArgs: false,
      },
    );
  } finally {
    await vite.close();
  }
});

test("Node candidates expose entry separately while preserving additional args", async () => {
  const vite = await createServer({
    root: process.cwd(),
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  try {
    const { harnessCandidates, harnessDraftFromConfig } = await vite.ssrLoadModule("/src/App.tsx");
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
      }],
    });
    assert.equal(candidate.mode, "node");
    assert.equal(candidate.entry, "dist/index.js");
    assert.deepEqual(candidate.args, ["--port", "3080"]);
    assert.equal(candidate.readinessTimeout, "30");

    const draft = harnessDraftFromConfig({
      harness: {
        mode: "node",
        program: "node.exe",
        entry: "dist/index.js",
        args: ["--port", "3080"],
      },
    });
    assert.equal(draft.mode, "node");
    assert.equal(draft.entry, "dist/index.js");
    assert.equal(draft.args, "--port\n3080");
  } finally {
    await vite.close();
  }
});
