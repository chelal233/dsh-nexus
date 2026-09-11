import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("creating a checkpoint is available only inside the current profile", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CheckpointsView } = await loader.loadModule("/src/App.tsx");
    const snapshot = {startup:{available:true}, profiles:{active_profile:"web"}, checkpoints:{}, recovery:{harness:{state:"stopped"}}};
    const render = (profileFilter: string) => renderToStaticMarkup(createElement(CheckpointsView, {snapshot, profileFilter, embedded:true, busyAction:null,runAction:async()=>true}));
    const createButton = (html: string) => [...html.matchAll(/<button([^>]*)>([\s\S]*?)<\/button>/g)].find(match => match[2].includes("Create checkpoint"))!;
    assert.doesNotMatch(createButton(render("web"))[1], /disabled/);
    assert.match(createButton(render("other"))[1], /disabled/);
    assert.match(render("other"), /Select this profile before creating a checkpoint/);
  } finally {await loader.close();}
});

test("profile delete excludes current profile and cleanup tree keeps protected items disabled",async()=>{
  const previousWindow=globalThis.window;
  globalThis.window={localStorage:{getItem:()=>null}} as any;
  const loader=await createUiTestLoader();
  try {
    const {ProfilesView,SpaceMaintenancePanel,SettingsView,CanaryProgress}=await loader.loadModule("/src/App.tsx");
    const snapshot:any={startup:{available:true},endpointErrors:{},config:{},status:{},health:{},state:{},harnessRuntime:{},harnessUi:{},releases:{},updates:{},checkpoints:{},diagnostics:{},recovery:{harness:{state:"stopped"}},
      profiles:{active_profile:"web",manifests:[{name:"web",bundles:[]},{name:"old",bundles:[]}]},
      maintenance:{preview:{preview_id:"tree-1",areas:[{kind:"Version slots",path:"C:/Nexus/releases",bytes:20}],items:[{id:"old",name:"old-slot",path:"C:/Nexus/releases/old",eligible:true,bytes:10},{id:"current",name:"current-slot",path:"C:/Nexus/releases/current",eligible:false,reason:"Current release",bytes:10}]}}};
    const props={snapshot,busyAction:null,runAction:async()=>true,refresh:async()=>{},themeMode:"system",setThemeMode:()=>{}};
    const render=(view:any)=>renderToStaticMarkup(createElement(view,props));
    const profiles=render(ProfilesView);
    assert.equal((profiles.match(/>Delete<\/button>/g)||[]).length,1);
    assert.match(profiles,/Deleted profiles/);
    const storage=render(SpaceMaintenancePanel);
    assert.match(storage,/storage-children/);
    assert.match(storage,/Select all removable items/);
    assert.match(storage,/<input type="checkbox" disabled=""\/><span class="storage-item-name">current-slot/);
    assert.match(storage,/Protected/);
    const settings=render(SettingsView);
    assert.ok(settings.indexOf('id="settings-display"')>=0);
    assert.ok(settings.indexOf('id="settings-display"')<settings.indexOf('id="settings-harness"'));
    const progress=renderToStaticMarkup(createElement(CanaryProgress,{running:false,progress:{completed_rounds:[{outcome:"passed",duration_ms:4000,enabled_bundles:[]}]}}));
    assert.match(progress,/<details><summary>Completed probe rounds/);
    assert.doesNotMatch(progress,/<details open/);
  } finally {globalThis.window=previousWindow;await loader.close();}
});
