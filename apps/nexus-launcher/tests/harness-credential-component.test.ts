import assert from "node:assert/strict";
import test from "node:test";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("Overview removes stale Harness credentials while a restart POST is deferred", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CheckpointsView, OverviewView, credentialInvalidationCanSettle, isRecoverableNoopError } = await loader.loadModule("/src/App.tsx");
    assert.equal(isRecoverableNoopError("Harness is already running"), true);
    assert.equal(isRecoverableNoopError("Harness is already stopped"), true);
    assert.equal(isRecoverableNoopError("Harness is already running; no lifecycle change was made."), true);
    assert.equal(isRecoverableNoopError("启动失败"), false, "no-op matching stays on the raw backend message before i18n");
    const snapshot = {
      startup: { available: true },
      endpointErrors: {},
      status: {},
      health: {},
      state: {},
      harnessRuntime: {
        harness: { state: "running", pid: 42 },
        generation: 7,
        log_session_run_id: "run-a",
      },
      harnessUi: {
        available: true,
        generation: 7,
        run_id: "run-a",
        url: "http://127.0.0.1:3080/?token=old-token",
        token: "old-token",
      },
      profiles: null,
      checkpoints: null,
      releases: null,
      updates: null,
      diagnostics: null,
      config: null,
    };
    const render = (credentialInvalidationPending: boolean) =>
      renderToStaticMarkup(
        createElement(OverviewView, {
          snapshot,
          busyAction: credentialInvalidationPending ? "Harness restart" : null,
          credentialInvalidationPending,
          runAction: async () => {},
          refresh: async () => {},
          themeMode: "system",
          setThemeMode: () => {},
        }),
      );

    const before = render(false);
    assert.match(before, /old-token/);
    assert.doesNotMatch(before, /<iframe/);
    assert.match(before, /Harness authentication requires a system browser/);

    let releasePost!: () => void;
    const deferredPost = new Promise<void>((resolve) => {
      releasePost = resolve;
    });
    let pending = false;
    const request = (async () => {
      pending = true;
      await deferredPost;
      pending = false;
    })();
    await Promise.resolve();
    assert.equal(pending, true);
    const during = render(pending);
    assert.doesNotMatch(during, /old-token/);
    assert.doesNotMatch(during, /<iframe/);
    assert.equal(
      credentialInvalidationCanSettle(snapshot, "7:run-a"),
      false,
      "a failed POST refresh cannot re-enable the same credential session",
    );
    assert.equal(
      credentialInvalidationCanSettle({
        ...snapshot,
        harnessRuntime: {
          harness: { state: "running", pid: 43 },
          generation: 8,
          log_session_run_id: "run-b",
        },
        harnessUi: {
          ...snapshot.harnessUi,
          generation: 8,
          run_id: "run-b",
          token: "new-token",
        },
      }, "7:run-a"),
      true,
      "a fresh runtime/UI session releases the fail-closed gate",
    );
    assert.equal(
      credentialInvalidationCanSettle({
        ...snapshot,
        harnessRuntime: {
          harness: { state: "running" },
          generation: 8,
          log_session_run_id: "run-recovered",
          log_session_launch_pending: true,
        },
        harnessUi: {
          ...snapshot.harnessUi,
          generation: 8,
          run_id: "run-recovered",
          token: "recovered-token",
        },
      }, "7:run-a"),
      true,
      "a recovered PID-less runtime may publish only its post-boundary credential",
    );
    assert.equal(
      credentialInvalidationCanSettle({
        ...snapshot,
        harnessRuntime: {
          harness: { state: "running" },
          generation: 8,
          log_session_run_id: "run-recovered",
          log_session_launch_pending: false,
        },
        harnessUi: {
          ...snapshot.harnessUi,
          generation: 8,
          run_id: "run-recovered",
          token: "unsafe-token",
        },
      }, "7:run-a"),
      false,
      "a PID-less runtime without a fresh boundary remains credential-closed",
    );
    assert.equal(
      credentialInvalidationCanSettle({
        ...snapshot,
        harnessRuntime: { harness: { state: "stopped", pid: null } },
        harnessUi: null,
      }, "7:run-a"),
      true,
      "a positively stopped runtime also releases the gate",
    );
    assert.equal(
      credentialInvalidationCanSettle({ ...snapshot, harnessRuntime: null, harnessUi: null }, "7:run-a"),
      false,
      "an endpoint failure is not evidence of a process boundary",
    );

    const checkpointMarkup = renderToStaticMarkup(
      createElement(CheckpointsView, {
        snapshot: {
          ...snapshot,
          checkpoints: {
            checkpoints: [{ id: "cp-a", profile: "web", release: "harness-a" }],
          },
        },
        busyAction: null,
        credentialInvalidationPending: false,
        runAction: async () => {},
        refresh: async () => {},
        themeMode: "system",
        setThemeMode: () => {},
      }),
    );
    assert.match(checkpointMarkup, /only Harness profile\/release selection/);
    assert.match(checkpointMarkup, /Agent lifecycle and Harness runtime are never saved or restored/);

    releasePost();
    await request;
  } finally {
    await loader.close();
  }
});

test("Harness web panel uses the system browser for token sessions and preserves safe iframe fallback", async () => {
  const loader = await createUiTestLoader();
  try {
    const { HarnessWebPanel } = await loader.loadModule("/src/App.tsx");
    const baseSnapshot = {
      startup: { available: true },
      endpointErrors: {},
      status: {},
      health: {},
      state: {},
      harnessRuntime: { harness: { state: "running", pid: 42 }, generation: 7, log_session_run_id: "run-a" },
      harnessUi: { available: true, generation: 7, run_id: "run-a", url: "http://127.0.0.1:3080/", token: "session-token" },
      profiles: null,
      checkpoints: null,
      releases: null,
      updates: null,
      diagnostics: null,
      recovery: null,
      config: null,
    };
    const render = (snapshot: typeof baseSnapshot, credentialInvalidationPending = false) =>
      renderToStaticMarkup(createElement(HarnessWebPanel, {
        snapshot,
        credentialInvalidationPending,
        busyAction: null,
        runAction: async () => {},
      }));

    const tokenMarkup = render(baseSnapshot);
    assert.doesNotMatch(tokenMarkup, /<iframe/, "token sessions must not load the authenticated page in the iframe");
    assert.match(tokenMarkup, /Harness authentication requires a system browser/);
    assert.match(tokenMarkup, /Open in system browser/);
    assert.doesNotMatch(tokenMarkup, /session-token/);

    const iframeMarkup = render({ ...baseSnapshot, harnessUi: { ...baseSnapshot.harnessUi, token: undefined } });
    assert.match(iframeMarkup, /<iframe/);
    assert.match(iframeMarkup, /http:\/\/127\.0\.0\.1:3080\//);

    const invalidatedMarkup = render(baseSnapshot, true);
    assert.doesNotMatch(invalidatedMarkup, /<iframe/);
    assert.match(invalidatedMarkup, /Harness authentication requires a system browser/);
    assert.match(invalidatedMarkup, /Open in system browser/);
    assert.match(invalidatedMarkup, /disabled/);
  } finally {
    await loader.close();
  }
});
