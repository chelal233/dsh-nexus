import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("plugin verification remains available without a report in paused recovery", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CompatibilityDialog, ProfilesView } = await loader.loadModule("/src/App.tsx");
    for (const state of ["stopped", "running"]) {
      const snapshot = { startup: { available: true }, profiles: { api_version: "v1", active_profile: "repair", manifests: [{ name: "repair", bundles: [] }] }, recovery: { paused: true, harness: { state }, harness_stop_required: state === "running" } };
      const html = renderToStaticMarkup(createElement(CompatibilityDialog, {
        snapshot, busyAction: null, pending: false, onClose: () => {}, runAction: async () => true,
      }));
      const button = [...html.matchAll(/<button\b[^>]*>[\s\S]*?<\/button>/g)].find(match => match[0].includes(">Verify plugins</button>"));
      assert.ok(button);
      assert.equal(/\bdisabled=/.test(button[0]), state === "running");
      const profiles = renderToStaticMarkup(createElement(ProfilesView, { snapshot, busyAction: null, runAction: async () => true }));
      assert.match(profiles, /<button[^>]*aria-expanded="false"/);
    }
  } finally { await loader.close(); }
});
