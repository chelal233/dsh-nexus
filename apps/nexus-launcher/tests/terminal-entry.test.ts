import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("Workbench reports configured custom command ahead of unrelated selected slot",async()=>{
 const loader=await createUiTestLoader();try{const {OverviewView}=await loader.loadModule("/src/App.tsx");
 const html=renderToStaticMarkup(createElement(OverviewView,{snapshot:{startup:{available:true},profiles:{active_profile:"default"},releases:{current_release:"old-slot",releases:[]},harnessRuntime:{harness:{state:"stopped"}},config:{config:{harness:{mode:"direct",program:"custom.exe"}}}},busyAction:null,runAction:async()=>true}));
 assert.match(html,/Configured command: custom.exe/);
 }finally{await loader.close();}
});

test("Global header exposes the DSH terminal while Harness is stopped, with unavailable and busy gates", async () => {
  const loader = await createUiTestLoader();
  try {
    const { HarnessTerminalButton } = await loader.loadModule("/src/App.tsx");
    for (const [available, busy, current] of [[true, false, "slot"], [false, false, "slot"], [true, true, "slot"], [true, false, null]]) {
      const html = renderToStaticMarkup(createElement(HarnessTerminalButton, {
        snapshot: { startup: { available }, profiles: { active_profile: "custom" },
          releases: { current_release: current, releases: [{ id: "slot" }] },
          harnessRuntime: { harness: { state: "stopped" } }, config: {} },
        busyAction: busy ? "operation" : null, runAction: async () => true,
      }));
      const button = [...html.matchAll(/<button\b[^>]*>[\s\S]*?<\/button>/g)].find(match => match[0].includes("Open DSH terminal"));
      assert.ok(button, "global entry remains available with Harness stopped");
      assert.equal(/\bdisabled=/.test(button[0]), !available || busy || !current);
    }
  } finally { await loader.close(); }
});
