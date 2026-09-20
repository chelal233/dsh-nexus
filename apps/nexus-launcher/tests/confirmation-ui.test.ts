import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("custom confirmation cancels on Escape, confirms explicitly and blocks the background; busy overlay releases on completion", async () => {
  const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const cancellations: unknown[] = [];
  let startupPhase = "compatibility";
  dom.window.nexusDesktop = {
    async invoke(_command: string, args: { method: string; body: unknown }) {
      if (args.method === "POST") cancellations.push(args.body);
      return { phase: cancellations.length ? "cancelled" : startupPhase, operation_id: "startup-test", cancellable: !cancellations.length };
    },
  };
  const loader = await createUiTestLoader();
  const { ConfirmationHost, confirmAction, BusyOverlay, StartupOperationPanel, CompatibilitySummary } = await loader.loadModule("/src/App.tsx");
  const container = document.getElementById("root")!;
  const root = createRoot(container);
  try {
    await React.act(async () => root.render(React.createElement(ConfirmationHost)));
    let answer: Promise<boolean>;
    await React.act(async () => { answer = confirmAction("Remove this plugin?"); });
    assert.equal(container.inert, true);
    assert.match(document.body.textContent!, /Remove this plugin/);
    await React.act(async () => { document.querySelector('.modal-card')!.dispatchEvent(new dom.window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); });
    assert.equal(await answer!, false);
    assert.equal(container.inert, false);
    await React.act(async () => { answer = confirmAction("Confirm removal"); });
    const buttons = [...document.querySelectorAll('button')];
    await React.act(async () => { (buttons.find(b => b.textContent === 'Confirm') as HTMLButtonElement).click(); });
    assert.equal(await answer!, true);
    await React.act(async () => root.render(React.createElement(BusyOverlay, { label: "Saving" })));
    assert.match(document.querySelector('[role="timer"]')!.textContent!, /0s/);
    assert.ok(document.querySelector('progress:not([value])'));
    await React.act(async () => { await new Promise(resolve => setTimeout(resolve, 1100)); });
    assert.match(document.querySelector('[role="timer"]')!.textContent!, /1s/);
    assert.equal(container.inert, true);
    await React.act(async () => { document.querySelector('.modal-card')!.dispatchEvent(new dom.window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); });
    assert.ok(document.querySelector('[role="dialog"]'));
    assert.equal(document.querySelector('.modal-header button'), null);
    await React.act(async () => root.render(React.createElement(BusyOverlay, { label: null })));
    assert.equal(document.querySelector('[role="dialog"]'), null);
    assert.equal(container.inert, false);
    startupPhase = "failed";
    await React.act(async () => root.render(React.createElement(BusyOverlay, { label: "Saving plugin choices" },
      React.createElement(StartupOperationPanel, { available: true, identity: "old-failure" }))));
    assert.doesNotMatch(document.body.textContent!, /Startup preparation failed/);
    assert.match(document.querySelector('[role="timer"]')!.textContent!, /0s/);
    startupPhase = "compatibility";
    await React.act(async () => root.render(React.createElement(BusyOverlay, { label: "Starting Harness" },
      React.createElement(StartupOperationPanel, { available: true, identity: "test" }))));
    const cancel = [...document.querySelectorAll('button')].find(button => button.textContent === 'Cancel startup') as HTMLButtonElement;
    assert.ok(cancel);
    assert.ok(cancel.closest('[role="dialog"]'));
    assert.equal(container.inert, true);
    await React.act(async () => cancel.click());
    assert.deepEqual(cancellations, [{ action: 'cancel', operation_id: 'startup-test' }]);
    const promotions: unknown[] = [];
    dom.window.nexusDesktop = { async invoke(_command: string, args: { body: unknown }) {
      assert.deepEqual(args.body, { action: "promote", id: "target", inspect_only: true });
      return { rollback_confirmation: "fresh-confirmation" };
    }};
    await React.act(async () => root.render(React.createElement(React.Fragment, null,
      React.createElement(ConfirmationHost), React.createElement(CompatibilitySummary, {
        snapshot: { recovery: { harness: { state: "stopped" } }, releases: { releases: [{id: "target"}] },
          profiles: { compatibility: { status: "needs_choice", source_profile: "web", release_id: "target", trigger: "version_switch", candidates: [] } } },
        busyAction: null, runAction: async (_label: string, _path: string, command: unknown) => { promotions.push(command); return true; }
      }))));
    await React.act(async () => { ([...document.querySelectorAll('button')].find(b => b.textContent === 'Retry version switch') as HTMLButtonElement).click(); });
    assert.equal(promotions.length, 0);
    assert.match(document.querySelector('[role="dialog"]')!.textContent!, /no verified rollback/);
    await React.act(async () => { ([...document.querySelectorAll('button')].find(b => b.textContent === 'Confirm') as HTMLButtonElement).click(); });
    assert.deepEqual(promotions, [{ action: "promote", id: "target", rollback_confirmation: "fresh-confirmation" }]);

  } finally {
    await React.act(async () => root.unmount());
    await loader.close(); dom.window.close();
    for (const [key, descriptor] of previous) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key); }
  }
});
