import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("diagnostic and recovery export controls require a live Agent and idle controls", async () => {
  const loader = await createUiTestLoader();
  try {
    const { DiagnosticsView } = await loader.loadModule("/src/App.tsx");
    for (const [available, busy] of [[true, false], [false, false], [true, true]]) {
      const html = renderToStaticMarkup(createElement(DiagnosticsView, {
        snapshot: { startup: { available }, diagnostics: { bundles: [] } },
        busyAction: busy ? "operation" : null, runAction: async () => {}, refresh: async () => {},
      }));
      const buttons = [...html.matchAll(/<button\b[^>]*>[\s\S]*?<\/button>/g)].filter(match => match[0].includes("Export diagnostics"));
      assert.equal(buttons.length, 2, "diagnostics and embedded recovery each offer one export action");
      for (const button of buttons) assert.equal(/\bdisabled=/.test(button[0]), !available || busy);
    }
  } finally { await loader.close(); }
});
