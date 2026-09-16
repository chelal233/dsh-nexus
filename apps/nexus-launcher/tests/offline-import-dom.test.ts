import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";
import { createUiTestLoader } from "./ui-test-loader.ts";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";

test("successful imports explain retained credential values in both interface languages", async () => {
  const loader = await createUiTestLoader();
  try {
    const { OfflineOperationStatus } = await loader.loadModule("/src/App.tsx");
    const { I18nProvider } = await loader.loadModule("/src/i18n.ts");
    const warning = "Some incoming configuration values were not applied to preserve your local credentials. To use the package values, review the credential conflict option before importing again.";
    const render = (locale, operation) => renderToStaticMarkup(createElement(I18nProvider, { initialLocale: locale },
      createElement(OfflineOperationStatus, { snapshot: { startup: { available: true }, endpointErrors: {}, config: {},
        updates: { operation: { operation_id: "import-notice", kind: "offline_import", phase: "succeeded", offline_contents: { runtime: false }, warning, ...operation } } },
        busyAction: null, runAction: async () => true })));
    assert.match(render("en", {}), /Some incoming configuration values were not applied/);
    assert.match(render("zh", {}), /为保留本地凭据，部分包内配置值未被应用/);
    for (const operation of [{ warning: null }, { phase: "failed" }, { kind: "offline_export" }]) {
      assert.doesNotMatch(render("en", operation), /Some incoming configuration values were not applied/);
    }
  } finally { await loader.close(); }
});

test("reading a credential-bearing package never selects credentials without user consent", async () => {
  const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator,
    HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  let root;
  let loader;
  let act;
  try {
    const react = await import("react"); act = react.act;
    const { createRoot } = await import("react-dom/client");
    const requests = [];
    mockIPC((command, payload) => {
      if (command === "choose_local_path") return "C:/Fixtures/credentials.tar.gz";
      assert.equal(command, "proxy_request");
      assert.equal(payload.path, "/v1/updates");
      assert.equal(payload.body.action, "offline_inspect");
      requests.push(payload.body);
      // A manifest may advertise credentials and even a replacement policy;
      // neither is permission to select or replace receiver credentials.
      return { version: "v-source", contents: { runtime: false, profiles: ["web"], configuration: true,
        environment: true, plugins: false, sessions: false, credentials: true, credential_policy: "replace" } };
    });
    loader = await createUiTestLoader();
    const { OfflinePackagePanel } = await loader.loadModule("/src/App.tsx");
    const submitted = [];
    root = createRoot(document.getElementById("root"));
    await act(async () => root.render(react.createElement(OfflinePackagePanel, {
      snapshot: { startup: { available: true }, endpointErrors: {}, config: {}, updates: {}, profiles: {},
        releases: { current_release: "receiver", releases: [{ id: "receiver", version: "v-receiver" }] } },
      busyAction: null, runAction: async (...args) => { submitted.push(args); return true; },
    })));
    const pane = () => document.querySelector('[role="tabpanel"]:not([hidden])');
    const button = label => {
      const result = [...pane().querySelectorAll("button")].find(item => item.textContent === label);
      assert.ok(result, label); assert.equal(result.disabled, false, label); return result;
    };
    const fieldset = () => {
      const field = pane().querySelector("fieldset"); assert.ok(field);
      assert.equal(field.querySelector("legend").textContent, "Choose contents to import"); return field;
    };
    const credentialBox = () => {
      const label = [...fieldset().querySelectorAll("label")].find(item => item.textContent === "Account credentials and .env");
      assert.ok(label); return label.querySelector('input[type="checkbox"]');
    };
    await act(async () => button("Browse file").click());
    await act(async () => button("Read package contents").click());
    assert.equal(requests.length, 1);
    assert.equal(credentialBox().disabled, false);
    assert.equal(credentialBox().checked, false);
    assert.equal(fieldset().querySelector('[role="alert"]'), null);
    assert.doesNotMatch(fieldset().textContent, /Program and runtime|v-source/);

    await act(async () => credentialBox().click());
    assert.equal(credentialBox().checked, true);
    assert.match(fieldset().querySelector('[role="alert"]').textContent, /not encrypted.*Replacing credentials/);
    const policy = fieldset().querySelector("select"); assert.equal(policy.value, "preserve");
    await act(async () => { policy.value = "replace"; policy.dispatchEvent(new dom.window.Event("change", { bubbles: true })); });
    assert.equal(policy.value, "replace");

    await act(async () => button("Read package contents").click());
    assert.equal(requests.length, 2);
    assert.equal(credentialBox().checked, false, "a new preview must revoke previous consent");
    assert.equal(fieldset().querySelector("select"), null);
    await act(async () => button("Import package").click());
    assert.equal(submitted.length, 1);
    const [title, path, command] = submitted[0];
    assert.equal(title, "Offline package import"); assert.equal(path, "/v1/updates");
    assert.equal(command.offline_contents.credentials, false);
    assert.equal(command.offline_contents.credential_policy, "preserve");
  } finally {
    if (root) await act(async () => root.unmount());
    if (loader) await loader.close();
    clearMocks(); dom.window.close();
    for (const [key, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key];
    }
  }
});
