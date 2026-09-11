import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { readFileSync } from "node:fs";
import { createDraftMemory, DraftMemoryContext, useDraftState, useDraftReference } from "../src/draft-memory.ts";

// Remount real hooks with changed server data, as navigation/error rendering does.
test("editor remount retains a dirty value and its original revision only inside the same App/root/editor", () => {
  const store = createDraftMemory();
  const root = "root-a:settings";
  const value = store.slot(root, "preferences.value", () => "secret draft");
  const revision = store.slot(root, "preferences.revision", () => ({ current: "revision-one" }));
  function Editor() {
    const [draft] = useDraftState("preferences.value", "new server value");
    const base = useDraftReference("preferences.revision", "revision-two");
    return createElement("span", null, draft + ":" + base.current);
  }
  const render = (scope: string, memory = store) => renderToStaticMarkup(createElement(DraftMemoryContext.Provider,
    { value: { store: memory, scope } }, createElement(Editor)));
  assert.equal(render(root), "<span>secret draft:revision-one</span>");
  render("root-a:maintenance"); // Navigate away, then mount the editor again.
  assert.equal(render(root), "<span>secret draft:revision-one</span>");
  assert.equal(render("root-b:settings"), "<span>new server value:revision-two</span>");
  assert.equal(render(root, createDraftMemory()), "<span>new server value:revision-two</span>", "new App forgets memory");
  value.value = "saved value"; revision.value.current = "revision-three";
  assert.equal(render(root), "<span>saved value:revision-three</span>");
});

test("App retains editor memory without retaining stale runtime snapshots or browser credentials", () => {
  const app = readFileSync(new URL("../src/App.tsx", import.meta.url), "utf8");
  const memory = readFileSync(new URL("../src/draft-memory.ts", import.meta.url), "utf8");
  assert.match(app, /setSnapshot\(failClosedSnapshot\(emptySnapshot\)\)/);
  assert.match(app, /<DraftMemoryContext.Provider key=\{draftRoot.current\}/);
  assert.match(app, /contentMode === "error"[\s\S]*?<ErrorState/);
  assert.doesNotMatch(memory, /(?:localStorage|sessionStorage)\./);
  for (const line of app.split("\n").filter(line => /useDraftState|useDraftReference/.test(line))) {
    assert.doesNotMatch(line, /harnessUi|["'](?:token|snapshot|health)["']/);
  }
});
