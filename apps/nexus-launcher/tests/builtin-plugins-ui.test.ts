import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("built-in plugin controls preserve independently updated dependencies and only install when absent", async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator, HTMLElement: dom.window.HTMLElement, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  let root, loader;
  try {
    const React = await import("react");
    const { createRoot } = await import("react-dom/client");
    dom.window.nexusDesktop = { invoke: async () => ({}), listen: () => () => {} };
    loader = await createUiTestLoader();
    const { MarketplaceSettings } = await loader.loadModule("/src/App.tsx");
    root = createRoot(document.getElementById("root")!);
    const catalog = (present: boolean, disabled = false) => ({ api_version: "v1", active_profile: "web", disabled_plugins: disabled ? ["dshmarket"] : [], manifests: [{ name: "web", bundles: present && !disabled ? ["dshmarket"] : [], plugins: present ? [{ package: "dshmarket", version: "9.4.2" }] : [] }] });
    for (const scenario of ["absent", "enabled-newer", "disabled-newer", "dependency-only", "external-install-race"]) {
      const present = ["enabled-newer", "disabled-newer", "dependency-only"].includes(scenario);
      const disabled = scenario === "disabled-newer";
      const profiles = catalog(present, disabled);
      if (scenario === "dependency-only") profiles.manifests[0].bundles = [];
      const market = { profile: "web", scope: "C:/test/profiles/web", installed: scenario === "enabled-newer", provider: "dsh-market", status: "ready" };
      const posts: Array<{path: string; body: Record<string, unknown>}> = [];
      dom.window.nexusDesktop = {
        async invoke(command, args) {
          assert.equal(command, "proxy_request");
          if (args.method === "POST") { posts.push({path: args.path, body: args.body}); return market; }
          return args.path === "/v1/profiles" ? (scenario === "external-install-race" ? catalog(true) : profiles) : market;
        }, listen() { return () => {}; },
      };
      await React.act(async () => root.render(React.createElement(MarketplaceSettings, {
        key: scenario, snapshot: { profiles, startup: { available: true }, harnessRuntime: { harness: { state: "stopped" } } }, busyAction: null,
        refresh: async () => {}, runAction: async (_title, path, body) => { posts.push({path, body}); return true; },
      })));
      const button = document.querySelector('.builtin-plugin-actions button') as HTMLButtonElement;
      assert.ok(button);
      if (scenario === "dependency-only") {
        assert.equal(button.disabled, true, "an existing dependency with unknown load state must not be reinstalled");
        continue;
      }
      assert.equal(button.disabled, false);
      await React.act(async () => button.click());
      if (scenario === "absent") assert.deepEqual(posts, [{path: "/v1/market", body: {profile: "web", scope: market.scope, provider: "dsh-market"}}]);
      else if (scenario === "external-install-race") {
        assert.deepEqual(posts, []);
        assert.match(document.body.textContent!, /Plugin state changed/);
      } else {
        assert.deepEqual(posts, [{path: "/v1/profiles", body: {action: disabled ? "plugin_enable" : "plugin_disable", profile: "web", package: "dshmarket"}}]);
        assert.match(document.body.textContent!, /9\.4\.2/);
      }
    }
  } finally {
    if (root) { const { act } = await import("react"); await act(async () => root.unmount()); }
    await loader?.close(); dom.window.close();
    for (const [key, descriptor] of previous) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key]; }
  }
});
