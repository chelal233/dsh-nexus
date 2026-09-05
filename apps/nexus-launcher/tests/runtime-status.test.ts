import assert from "node:assert/strict";
import test from "node:test";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";

async function loadRuntimeStatusPanel() {
  const vite = await createServer({
    root: process.cwd(),
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  const module = await vite.ssrLoadModule("/src/App.tsx");
  return { vite, RuntimeStatusPanel: module.RuntimeStatusPanel };
}

test("RuntimeStatusPanel renders verified tool metadata and escapes paths", async () => {
  const { vite, RuntimeStatusPanel } = await loadRuntimeStatusPanel();
  try {
    assert.equal(typeof RuntimeStatusPanel, "function");
    let checkCalls = 0;
    const markup = renderToStaticMarkup(createElement(RuntimeStatusPanel, {
      agentAvailable: true,
      state: {
        phase: "success",
        status: {
          api_version: "v1",
          tools: [
            {
              name: "git",
              available: true,
              version: "git version 2.45.1",
              source: "system",
              path: "C:\\Program Files\\Git\\cmd\\git.exe",
            },
            {
              name: "node",
              available: true,
              version: "v25.1.0",
              source: "nexus",
              path: "/opt/<node>&runtime/bin/node",
            },
            {
              name: "pnpm",
              available: false,
              reason: "corepack_shim_unverified",
            },
          ],
        },
        error: null,
      },
      onCheck: () => { checkCalls += 1; },
    }));

    assert.match(markup, /Runtime status/);
    assert.match(markup, /git version 2\.45\.1/);
    assert.match(markup, /system/);
    assert.match(markup, /C:\\Program Files\\Git\\cmd\\git\.exe/);
    assert.match(markup, /&lt;node&gt;&amp;runtime/);
    assert.doesNotMatch(markup, /<node>&runtime/);
    assert.match(markup, /Corepack shim could not be verified/);
    assert.equal(checkCalls, 0, "rendering must not trigger a runtime request");

    const reasonMarkup = renderToStaticMarkup(createElement(RuntimeStatusPanel, {
      agentAvailable: true,
      state: {
        phase: "success",
        status: {
          api_version: "v1",
          tools: [
            { name: "git", available: false, reason: "not_found" },
            { name: "node", available: true, version: "v25.1.0", source: "system", path: "/usr/bin/node" },
            { name: "pnpm", available: false, reason: "permission_denied" },
          ],
        },
        error: null,
      },
      onCheck: () => {},
    }));
    assert.match(reasonMarkup, /Runtime tool was not found/);
    assert.match(reasonMarkup, /This runtime could not be verified/);
  } finally {
    await vite.close();
  }
});

test("RuntimeStatusPanel renders loading and Agent unavailable states", async () => {
  const { vite, RuntimeStatusPanel } = await loadRuntimeStatusPanel();
  try {
    const loading = renderToStaticMarkup(createElement(RuntimeStatusPanel, {
      agentAvailable: true,
      state: { phase: "loading", status: null, error: null },
      onCheck: () => {},
    }));
    assert.match(loading, /Checking runtime/);
    assert.match(loading, /disabled/);

    const unavailable = renderToStaticMarkup(createElement(RuntimeStatusPanel, {
      agentAvailable: false,
      state: { phase: "success", status: { api_version: "v1", tools: [] }, error: null },
      onCheck: () => {},
    }));
    assert.match(unavailable, /Agent is unavailable/);
    assert.match(unavailable, /disabled/);
  } finally {
    await vite.close();
  }
});

test("RuntimeStatusPanel renders a retryable error without stale success", async () => {
  const { vite, RuntimeStatusPanel } = await loadRuntimeStatusPanel();
  try {
    const markup = renderToStaticMarkup(createElement(RuntimeStatusPanel, {
      agentAvailable: true,
      state: {
        phase: "error",
        status: null,
        error: "The Agent API is not responding",
      },
      onCheck: () => {},
    }));
    assert.match(markup, /Runtime status request failed/);
    assert.match(markup, /The Agent API is not responding/);
    assert.match(markup, /Retry/);
    assert.doesNotMatch(markup, /git version/);
  } finally {
    await vite.close();
  }
});
