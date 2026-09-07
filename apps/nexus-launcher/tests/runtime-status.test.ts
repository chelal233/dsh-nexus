import assert from "node:assert/strict";
import test from "node:test";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

async function loadRuntimeStatusPanel() {
  const loader = await createUiTestLoader();
  const module = await loader.loadModule("/src/App.tsx");
  const i18n = await loader.loadModule("/src/i18n.ts");
  return {
    loader,
    RuntimeStatusPanel: module.RuntimeStatusPanel,
    runtimeStatusFromResponse: module.runtimeStatusFromResponse,
    createRuntimeStatusController: module.createRuntimeStatusController,
    I18nProvider: i18n.I18nProvider,
  };
}

test("RuntimeStatusPanel renders verified tool metadata and escapes paths", async () => {
  const { loader, RuntimeStatusPanel, I18nProvider } = await loadRuntimeStatusPanel();
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
    assert.match(markup, /System/);
    assert.match(markup, /C:\\Program Files\\Git\\cmd\\git\.exe/);
    assert.match(markup, /&lt;node&gt;&amp;runtime/);
    assert.doesNotMatch(markup, /<node>&runtime/);
    assert.match(markup, /Corepack shim could not be verified/);
    assert.equal(checkCalls, 0, "rendering must not trigger a runtime request");

    const chineseMarkup = renderToStaticMarkup(createElement(I18nProvider, { initialLocale: "zh" }, createElement(RuntimeStatusPanel, {
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
              path: "/opt/node/bin/node",
            },
            { name: "pnpm", available: false, reason: "not_found" },
          ],
        },
        error: null,
      },
      onCheck: () => {},
    })));
    assert.match(chineseMarkup, /来源: <code>系统<\/code>/);
    assert.match(chineseMarkup, /来源: <code>Nexus<\/code>/);

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
    await loader.close();
  }
});

test("RuntimeStatusPanel renders loading and Agent unavailable states", async () => {
  const { loader, RuntimeStatusPanel } = await loadRuntimeStatusPanel();
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
    await loader.close();
  }
});

test("RuntimeStatusPanel renders a retryable error state", async () => {
  const { loader, RuntimeStatusPanel } = await loadRuntimeStatusPanel();
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
    await loader.close();
  }
});

function validRuntimeTools(): Array<Record<string, unknown>> {
  return [
    { name: "pnpm", available: false, reason: "not_found" },
    { name: "node", available: true, version: "v25.1.0", source: "nexus", path: "/opt/node/bin/node" },
    { name: "git", available: true, version: "git version 2.45.1", source: "system", path: "C:\\Program Files\\Git\\cmd\\git.exe" },
  ];
}

function runtimePayload(tools: unknown[]): Record<string, unknown> {
  return { api_version: "v1", tools };
}

test("runtime status parser canonicalizes order and requires verified available metadata", async () => {
  const { loader, runtimeStatusFromResponse } = await loadRuntimeStatusPanel();
  try {
    assert.equal(typeof runtimeStatusFromResponse, "function");
    const parsed = runtimeStatusFromResponse(runtimePayload(validRuntimeTools()));
    assert.deepEqual(parsed.tools.map((tool: { name: string }) => tool.name), ["git", "node", "pnpm"]);
    assert.equal(parsed.tools[0].source, "system");
    assert.equal(parsed.tools[0].path, "C:\\Program Files\\Git\\cmd\\git.exe");
    assert.equal(parsed.tools[1].source, "nexus");

    for (const path of ["C:\\runtime\\git.exe", "\\\\server\\share\\git.exe", "/opt/git/bin/git"]) {
      const tools = validRuntimeTools();
      tools[2].path = path;
      const pathParsed = runtimeStatusFromResponse(runtimePayload(tools));
      assert.equal(pathParsed.tools[0].path, path);
    }
  } finally {
    await loader.close();
  }
});

test("runtime status parser rejects malformed, duplicate, missing, and non-absolute entries", async () => {
  const { loader, runtimeStatusFromResponse } = await loadRuntimeStatusPanel();
  try {
    assert.equal(typeof runtimeStatusFromResponse, "function");
    const malformed: Array<[string, unknown]> = [];

    const missingVersion = validRuntimeTools();
    delete missingVersion[2].version;
    malformed.push(["missing available version", runtimePayload(missingVersion)]);

    const missingSource = validRuntimeTools();
    delete missingSource[2].source;
    malformed.push(["missing available source", runtimePayload(missingSource)]);

    const missingPath = validRuntimeTools();
    delete missingPath[2].path;
    malformed.push(["missing available path", runtimePayload(missingPath)]);

    const relativePath = validRuntimeTools();
    relativePath[2].path = "bin/git.exe";
    malformed.push(["relative path", runtimePayload(relativePath)]);

    const driveRelativePath = validRuntimeTools();
    driveRelativePath[2].path = "C:git.exe";
    malformed.push(["drive-relative path", runtimePayload(driveRelativePath)]);

    const unknownSource = validRuntimeTools();
    unknownSource[2].source = "download";
    malformed.push(["unknown source", runtimePayload(unknownSource)]);

    const duplicateName = validRuntimeTools();
    duplicateName[0].name = "git";
    malformed.push(["duplicate name", runtimePayload(duplicateName)]);

    malformed.push(["missing tool", runtimePayload(validRuntimeTools().slice(1))]);
    malformed.push(["unknown tool", runtimePayload([...validRuntimeTools(), { name: "ruby", available: false }])]);
    malformed.push(["non-object item", runtimePayload([null, ...validRuntimeTools().slice(1)])]);

    for (const [label, value] of malformed) {
      assert.throws(
        () => runtimeStatusFromResponse(value),
        /Runtime status response is invalid\./,
        label,
      );
    }
  } finally {
    await loader.close();
  }
});

test("runtime status controller is manual, injectable, and clears old success on failure", async () => {
  const { loader, createRuntimeStatusController } = await loadRuntimeStatusPanel();
  try {
    assert.equal(typeof createRuntimeStatusController, "function");
    const requests: Array<[string, string]> = [];
    const events: Array<{ phase: string; status: unknown; error: string | null }> = [];
    let fail = false;
    const controller = createRuntimeStatusController(
      async (path: string, method: string) => {
        requests.push([method, path]);
        if (fail) throw new Error("The Agent API is not responding");
        return runtimePayload(validRuntimeTools());
      },
      (state: { phase: string; status: unknown; error: string | null }) => {
        events.push(state);
      },
    );

    assert.equal(controller.getState().phase, "idle");
    assert.deepEqual(requests, [], "render/setup must not request runtime status");
    assert.equal((await controller.check(false)).phase, "idle");
    assert.deepEqual(requests, [], "unavailable Agent must not call transport");

    const success = await controller.check(true);
    assert.equal(success.phase, "success");
    assert.deepEqual(requests, [["GET", "/v1/runtime"]]);
    assert.deepEqual(events.map((state) => state.phase), ["loading", "success"]);
    assert.equal((events[0].status), null);
    assert.ok(success.status);

    fail = true;
    const failure = await controller.check(true);
    assert.equal(failure.phase, "error");
    assert.equal(failure.status, null, "failed refresh must clear prior success");
    assert.equal(controller.getState().status, null);
    assert.deepEqual(requests, [["GET", "/v1/runtime"], ["GET", "/v1/runtime"]]);
    assert.deepEqual(events.map((state) => state.phase), ["loading", "success", "loading", "error"]);
    assert.equal(events[2].status, null);
    assert.equal(events[3].status, null);
  } finally {
    await loader.close();
  }
});
