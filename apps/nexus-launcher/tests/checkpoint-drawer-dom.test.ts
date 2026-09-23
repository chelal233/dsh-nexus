import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { createUiTestLoader } from "./ui-test-loader.ts";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";

test("history uses local timestamps and an accessible drawer with readable/source modes", async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: "http://localhost/" });
  const bindings = { window: dom.window, document: dom.window.document, navigator: dom.window.navigator, HTMLElement: dom.window.HTMLElement, Node: dom.window.Node, Event: dom.window.Event, CustomEvent: dom.window.CustomEvent, IS_REACT_ACT_ENVIRONMENT: true };
  const old = new Map(Object.keys(bindings).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  const oldTZ = process.env.TZ; process.env.TZ = "Asia/Shanghai";
  for (const [key, value] of Object.entries(bindings)) Object.defineProperty(globalThis, key, { value, configurable: true, writable: true });
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const root = createRoot(document.getElementById("root")!);
  mockIPC(() => ({}));
  const loader = await createUiTestLoader();
  try {
    const { CheckpointsView, readableSnapshotFields } = await loader.loadModule("/src/App.tsx");
    const { I18nProvider } = await loader.loadModule("/src/i18n.ts");
    const milliseconds = Date.parse("2026-09-22T20:03:04Z");
    const timestamp = new Date(milliseconds).toLocaleString("en-US");
    const summary = { snapshot_id: "snapshot-hidden-id", created_unix_ms: milliseconds, profile_name: "web", kind: "manual", dsh_version: "0.1.7", plugin_count: 2, file_count: 1 };
    const content = "port: 0\nplugins:\n  - archive\n  - files\nenabled: true\n";
    let fail = false, requests = 0, pending: ((value: unknown) => void) | null = null, delayed = false;
    const value = { summary, files: [{ path: "profiles/web/settings.yaml", state: "present", stored_size: content.length, content }, { path: "truncated.json", state: "present", content: "RAW-TRUNCATED", content_truncated: true }] };
    mockIPC(() => { requests++; if (delayed) return new Promise(resolve => { pending = resolve; }); if (fail) throw Error("fixture failure"); return value; });
    await React.act(async () => root.render(React.createElement(I18nProvider, { initialLocale: "en" }, React.createElement(CheckpointsView, {
      snapshot: { profiles: { active_profile: "web" }, checkpoints: { snapshots: [{ snapshot_id: summary.snapshot_id, summary, valid: true }], checkpoints: [{ id: "checkpoint-hidden-id", created_at_unix: milliseconds / 1000, profile: "web", release: "0.1.7" }] }, recovery: { harness: { state: "stopped" } }, startup: { available: true } },
      busyAction: null, runAction: async () => true, embedded: true,
    }))));
    assert.equal([...document.querySelectorAll("strong")].filter(node => node.textContent === timestamp).length, 2);
    assert.ok(!document.getElementById("root")!.textContent!.includes("snapshot-hidden-id"));
    const click = async (node: HTMLElement) => { await React.act(async () => node.click()); };
    const button = (text: string) => [...document.querySelectorAll("button")].find(node => node.textContent?.trim() === text)!;
    const detailButton = button("Detail"); detailButton.focus(); await click(detailButton);
    const drawer = () => document.querySelector<HTMLElement>('[role="dialog"]')!;
    assert.ok(drawer().classList.contains("drawer-overlay"));
    assert.ok(document.getElementById("root")!.inert);
    assert.ok(drawer().textContent!.includes("Asia/Shanghai"));
    assert.ok(drawer().querySelector(".snapshot-values")!.textContent!.includes("archive"));
    assert.equal(drawer().querySelectorAll("pre").length, 0);
    assert.ok(!drawer().textContent!.includes("RAW-TRUNCATED"));
    await click(button("Source code"));
    assert.equal(drawer().querySelector(".snapshot-readable-file pre")!.textContent, content);
    assert.equal(drawer().querySelectorAll(".snapshot-readable-file pre")[1].textContent, "RAW-TRUNCATED");
    await click(button("Visual"));
    await React.act(async () => drawer().querySelector(".modal-card")!.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    assert.equal(document.querySelector('[role="dialog"]'), null);
    assert.equal(document.activeElement, detailButton);
    // A closed request cannot reopen the drawer after its reply arrives.
    delayed = true; await click(detailButton);
    await click(drawer().querySelector<HTMLElement>('button[aria-label="Close"]')!);
    await React.act(async () => pending!(value));
    assert.equal(document.querySelector('[role="dialog"]'), null);
    delayed = false; fail = true; await click(detailButton);
    assert.ok(drawer().textContent!.includes("fixture failure"));
    const before = requests; fail = false; await click(button("Retry"));
    assert.equal(requests, before + 1);
    assert.ok(drawer().querySelector(".snapshot-values"));
    await click(drawer().querySelector<HTMLElement>('button[aria-label="Close"]')!);
    // Legacy checkpoints are readable without an unsupported server request.
    const details = [...document.querySelectorAll("button")].filter(node => node.textContent?.trim() === "Detail");
    const beforeLegacy = requests; await click(details[1]);
    assert.equal(requests, beforeLegacy);
    assert.ok(drawer().textContent!.includes("Legacy metadata only"));
    assert.equal(readableSnapshotFields("invalid: ["), null);
    assert.ok(readableSnapshotFields("a: &cycle\n  self: *cycle").some((row: any) => row.note === "nested"));
    assert.equal(readableSnapshotFields('{"port":0,"enabled":false}')[0].value, 0);
  } finally {
    await React.act(async () => root.unmount());
    await loader.close(); clearMocks(); dom.window.close();
    if (oldTZ === undefined) delete process.env.TZ; else process.env.TZ = oldTZ;
    for (const [key, descriptor] of old) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete (globalThis as any)[key]; }
  }
});
