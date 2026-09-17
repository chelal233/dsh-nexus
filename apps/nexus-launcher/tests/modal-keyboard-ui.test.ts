import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("modal keyboard loop includes textareas and links, skips hidden controls, and restores focus", async () => {
  const dom = new JSDOM('<button id="trigger">Open</button><div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator, HTMLElement: dom.window.HTMLElement, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { value, configurable: true, writable: true });
  dom.window.HTMLElement.prototype.getClientRects = function () { return this.closest('[hidden]') ? [] as any : [{}] as any; };
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const loader = await createUiTestLoader();
  const { Modal } = await loader.loadModule("/src/App.tsx");
  const root = createRoot(document.getElementById("root")!);
  let closed = false;
  document.getElementById("trigger")!.focus();
  try {
    for (const tag of ["textarea", "a"]) {
      await React.act(async () => root.render(React.createElement(Modal, { title: "Details", onClose: () => { closed = true; } },
        React.createElement(tag, { id: "last", ...(tag === "a" ? { href: "#help" } : {}) }),
        React.createElement("div", { hidden: true }, React.createElement("button", { id: "hidden" }, "Hidden")))));
      const close = document.querySelector('.modal-header button') as HTMLElement;
      close.focus();
      close.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }));
      assert.equal(document.activeElement?.id, "last");
      document.activeElement!.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
      assert.equal(document.activeElement, close);
    }
    document.activeElement!.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    assert.equal(closed, true);
    await React.act(async () => root.unmount());
    assert.equal(document.activeElement?.id, "trigger");
  } finally {
    await React.act(async () => root.unmount());
    await loader.close(); dom.window.close();
    for (const [key, value] of previous) value ? Object.defineProperty(globalThis, key, value) : delete globalThis[key];
  }
});
