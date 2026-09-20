import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("update dialog requires download confirmation, shows progress, and defers installation until verified", async () => {
  const dom = new JSDOM('<!doctype html><div id="root"></div>', { url: "http://localhost/" });
  dom.window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
  dom.window.scrollTo = () => {};
  dom.window.HTMLElement.prototype.scrollIntoView = () => {};
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator,
    HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, Event: dom.window.Event,
    CustomEvent: dom.window.CustomEvent, IS_REACT_ACT_ENVIRONMENT: true };
  const previous = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  const listeners = new Map<string, Set<Function>>();
  const calls: string[] = [];
  let state = { enabled: true, phase: "idle" };
  dom.window.nexusDesktop = {
    async invoke(command: string, args?: { enabled?: boolean }) {
      calls.push(command);
      if (command === "harness_desktop_status") return { phase: "idle" };
      if (command === "update_status") return state;
      if (command === "update_settings") {
        state = { ...state, enabled: args?.enabled === true };
        for (const callback of listeners.get("nexus-update") ?? []) callback({ payload: state });
        return state;
      }
      if (command === "startup_status") return { available: true, running: true };
      return command === "update_check" ? { phase: "idle" } : {};
    },
    listen(name: string, callback: Function) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name)!.add(callback);
      return () => listeners.get(name)!.delete(callback);
    },
  };
  let root, loader;
  try {
    const React = await import("react"); const { createRoot } = await import("react-dom/client");
    loader = await createUiTestLoader(); const { App } = await loader.loadModule("/src/App.tsx");
    root = createRoot(document.getElementById("root")!);
    await React.act(async () => root.render(React.createElement(App)));
    const button = () => document.querySelector('.sidebar-footer button[title="Update Nexus"]') as HTMLButtonElement;
    const dialog = () => document.querySelector('[role="dialog"][aria-label="Update Nexus"]');
    const action = (text: string) => [...(dialog()?.querySelectorAll('button') ?? [])].find(b => b.textContent === text) as HTMLButtonElement;
    const publish = async (phase: string, extra = {}) => React.act(async () => {
      for (const callback of listeners.get("nexus-update") ?? []) callback({payload:{enabled:true,phase,version:'0.2.0',...extra}});
    });
    assert.equal(button(),null);
    await publish('available');
    assert.equal(calls.filter(c=>c==='update_download').length,0);
    await React.act(async()=>button().click());
    assert.ok(dialog());assert.equal(action('Update and restart'),undefined);
    await React.act(async()=>action('Not now').click());assert.equal(dialog(),null);
    assert.equal(calls.filter(c=>c==='update_download').length,0);
    await React.act(async()=>button().click());
    await React.act(async()=>action('Confirm and download').click());
    assert.equal(calls.filter(c=>c==='update_download').length,1);
    await publish('downloading',{percent:42.8});
    assert.equal(dialog()?.querySelector('progress')?.getAttribute('value'),'42');
    assert.equal(action('Update and restart'),undefined);
    await publish('ready');
    assert.equal(calls.filter(c=>c==='update_install').length,0);
    await React.act(async()=>action('Restart later').click());assert.equal(dialog(),null);
    await React.act(async()=>button().click());
    await React.act(async()=>action('Update and restart').click());
    assert.equal(calls.filter(c=>c==='update_install').length,1);
    await React.act(async()=>action('Restart later').click());
    const settings = [...document.querySelectorAll('nav button')].find(button => button.textContent?.includes('Settings'));
    assert.ok(settings);
    await React.act(async () => (settings as HTMLButtonElement).click());
    const row = [...document.querySelectorAll('.integration-list > div')].find(element => element.textContent?.includes('Automatic update checks'));
    assert.ok(row);
    const checkbox = row.querySelector('input') as HTMLInputElement;
    assert.equal(checkbox.checked, true);
    await React.act(async () => checkbox.click());
    assert.equal(calls.filter(command => command === 'update_settings').length, 1);
    assert.equal(checkbox.checked, false);
    await React.act(async () => { for (const callback of listeners.get("nexus-update") ?? []) callback({ payload: { enabled: false, phase: "idle" } }); });
    const manual = [...document.querySelectorAll('button')].find(b => b.textContent === 'Check for updates') as HTMLButtonElement;
    assert.ok(manual); assert.equal(manual.disabled, false);
    await React.act(async () => manual.click());
    assert.equal(calls.filter(command => command === 'update_check').length, 1);
    assert.equal(checkbox.checked, false);
  } finally {
    if (root) { const { act } = await import("react"); await act(async () => root.unmount()); }
    await loader?.close(); dom.window.close();
    for (const [key, descriptor] of previous) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key]; }
  }
});
