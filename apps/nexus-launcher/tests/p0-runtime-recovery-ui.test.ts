import assert from "node:assert/strict";
import test from "node:test";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

const startup = { available: true, running: true };

test("damaged workspace records expose untruncated location and explicit repair actions", async () => {
  const loader = await createUiTestLoader();
  try {
    const { DegradedNotice } = await loader.loadModule("/src/App.tsx");
    const raw = 'C:/fixture/profiles.json: expected comma at line 15 column 9';
    const markup = renderToStaticMarkup(createElement(DegradedNotice, {
      errors: {"/v1/profiles": raw}, onNavigate: () => {}, onRetry: () => {},
    }));
    assert.match(markup, /profiles.json/);
    assert.match(markup, /line 15 column 9/);
    assert.match(markup, /Back up the file/);
    assert.match(markup, /Open related module/);
    assert.match(markup, /Retry/);
  } finally { await loader.close(); }
});

test("read-only recovery omits duplicate endpoint rejection text but retains unrelated failures", async () => {
  const loader = await createUiTestLoader();
  try {
    const { DegradedNotice } = await loader.loadModule("/src/App.tsx");
    const errors = {
      "/v1/config": "Backend error: Read-only recovery: damaged profiles",
      "/v1/state": "Read-only recovery: damaged profiles",
    };
    const render = (value: Record<string, string>, readOnlyRecovery: boolean) =>
      renderToStaticMarkup(createElement(DegradedNotice, { errors: value, readOnlyRecovery }));
    assert.equal(render(errors, true), "");
    assert.match(render(errors, false), /damaged profiles/);
    const mixed = render({ ...errors, "/v1/diagnostics": "Independent export failure" }, true);
    assert.match(mixed, /Independent export failure/);
    assert.doesNotMatch(mixed, /damaged profiles/);
  } finally {
    await loader.close();
  }
});
const baseSnapshot = {
  startup,
  endpointErrors: {},
  status: {},
  health: {},
  state: {},
  harnessRuntime: {},
  harnessUi: {},
  profiles: {},
  checkpoints: {},
  releases: {},
  updates: {},
  diagnostics: {},
  config: {},
  recovery: {},
};
const props = {
  busyAction: null,
  credentialInvalidationPending: false,
  runAction: async () => true,
  refresh: async () => undefined,
  themeMode: "system",
  setThemeMode: () => undefined,
};

async function loadViews() {
  const loader = await createUiTestLoader();
  const app = await loader.loadModule("/src/App.tsx");
  return {
    loader,
    GuideView: app.GuideView,
    CompatibilitySummary: app.CompatibilitySummary,
    CompatibilityDialog: app.CompatibilityDialog,
    ProfilesView: app.ProfilesView,
    ProfilePlugins: app.ProfilePlugins,
    UpdatesView: app.UpdatesView,
    CheckpointsView: app.CheckpointsView,
  };
}

test("profile hub collapses children; profile plugins show truthful inventory", async () => {
  const { loader, ProfilesView, ProfilePlugins } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      recovery: {
        manual_entry_available: true,
        harness_stop_required: false,
        harness: { state: "failed" },
      },
      profiles: {
        active_profile: "web",
        manifests: [
          {
            name: "web",
            bundles: ["dsh-base"],
            plugins: [
              { package: "dsh-base", version: "1.0.0", builtin: true, removable: false },
              { package: "extra-plugin", version: "2.0.0", builtin: false, removable: true },
            ],
          },
        ],
      },
    };
    const markup = renderToStaticMarkup(createElement(ProfilesView, { ...props, snapshot }));
    assert.match(markup, /Profile catalog/);
    assert.match(markup, /▸/);
    assert.ok(markup.indexOf("Open settings.yaml") < markup.indexOf("profile-row-toggle"));
    assert.ok(markup.indexOf("profile-row-toggle") < markup.indexOf("New profile name"));
    assert.ok(markup.indexOf("New profile name") < markup.indexOf(">Create profile<"));
    assert.doesNotMatch(markup, /Saved checkpoints/);
    assert.doesNotMatch(markup, /Plugin inventory/);
    const pluginsMarkup = renderToStaticMarkup(
      createElement(ProfilePlugins, { ...props, snapshot, profile: "web" }),
    );
    assert.match(pluginsMarkup, /dsh-base/);
    assert.match(pluginsMarkup, /Built-in/);
    assert.match(pluginsMarkup, /extra-plugin/);
    assert.match(pluginsMarkup, /Removable/);
  } finally {
    await loader.close();
  }
});

test("cold operations render stages without a confirmation pause", async () => {
  const { loader, UpdatesView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      config: { runtime: { source: "official", mode: "portable" } },
      updates: {
        update: { state: "running" },
        operation: {
          operation_id: "cold-7",
          phase: "awaiting_confirmation",
          tag: "v1.2.3",
          progress_percent: 25,
        },
      },
      releases: { releases: [] },
    };
    const markup = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot }));
    // Supply confirmation is retired: no confirm actions, no supply plan
    // details, and no bundled-runtime download promises.
    assert.doesNotMatch(markup, /Confirm exact plan/);
    assert.doesNotMatch(markup, /supply_plan/);
    assert.match(markup, /Current stage/);
    assert.match(markup, /v1\.2\.3/);
    assert.match(markup, /test an isolated profile first/);
  } finally {
    await loader.close();
  }
});

test("finished cold attempts show dated collapsed history and safe actions", async () => {
  const { loader, UpdatesView } = await loadViews();
  try {
    const operation = {
      operation_id: "cold-old",
      phase: "failed",
      tag: "v0.1.2-rc1",
      updated_at_unix: 1788753322,
      progress_percent: 100,
      error: "old npm failure",
      output_tail: "old build output",
      cleanup_pending: false,
    };
    const render = (overrides = {}) =>
      renderToStaticMarkup(
        createElement(UpdatesView, {
          ...props,
          snapshot: { ...baseSnapshot, updates: { operation: { ...operation, ...overrides } } },
        }),
      );
    const history = render();
    assert.match(history, /Last installation/);
    assert.match(history, /2026/);
    assert.match(history, /saved installation record/);
    assert.match(history, /<details><summary>Installation log and details<\/summary>/);
    assert.match(history, /old npm failure/);
    assert.match(history, /Retry installation/);
    assert.match(history, /Clear finished record/);
    assert.doesNotMatch(history, /Current stage|<progress|role="alert"/);
    const cleanup = render({ cleanup_pending: true, cleanup_error: "still cleaning" });
    assert.match(cleanup, /Retry cleanup/);
    assert.match(cleanup, /still cleaning/);
    assert.doesNotMatch(cleanup, /Retry installation|Clear finished record/);
    const active = render({ phase: "building", error: null });
    assert.match(active, /Current stage/);
    assert.match(active, /<progress/);
    assert.match(active, /old build output/);
    assert.doesNotMatch(
      active,
      /Last installation|Installation log and details|Clear finished record/,
    );
  } finally {
    await loader.close();
  }
});

test("compatibility summary identifies the checked release, projection, and disabled plugin", async () => {
  const { loader, CompatibilitySummary } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      profiles: {
        compatibility: {
          status: "isolated",
          source_profile: "desktop",
          effective_profile: "nexus-projection",
          release_id: "rc1",
          disabled: [{ package: "third-party-plugin", reason: "missing startup API" }],
        },
      },
    };
    const markup = renderToStaticMarkup(
      createElement(CompatibilitySummary, { ...props, snapshot }),
    );
    assert.match(markup, /Plugin check result/);
    assert.match(markup, /desktop/);
    assert.match(markup, /nexus-projection/);
    assert.match(markup, /rc1/);
    assert.match(markup, /third-party-plugin/);
    assert.match(markup, /missing startup API/);
    assert.match(markup, /not every runtime feature/);
  } finally {
    await loader.close();
  }
});

test("startup dialog exposes an explicit basic check without claiming success before execution", async () => {
  const { loader, CompatibilityDialog } = await loadViews();
  try {
    const markup = renderToStaticMarkup(
      createElement(CompatibilityDialog, {
        ...props,
        snapshot: baseSnapshot,
        pending: false,
        onClose: () => undefined,
      }),
    );
    assert.match(markup, /Run basic checks/);
    assert.match(markup, /Does not compile or start Harness/);
    assert.doesNotMatch(markup, /No blocking issues found/);
    const blocked = renderToStaticMarkup(
      createElement(CompatibilityDialog, {
        ...props,
        snapshot: baseSnapshot,
        pending: false,
        onClose: () => undefined,
        basicResult: {
          api_version: "v1",
          ready: false,
          checked_at_unix: 1,
          checks: [
            { id: "entry", status: "blocked", reason: "ENTRY_MISSING", next: "REPAIR_VERSION" },
            { id: "home", status: "blocked", reason: "HOME_DENIED", next: "CHOOSE_HOME" },
          ],
        },
      }),
    );
    for (const message of ["ENTRY_MISSING", "REPAIR_VERSION", "HOME_DENIED", "CHOOSE_HOME"])
      assert.match(blocked, new RegExp(message));
    assert.match(blocked, /Resolve the blocking issues before startup/);
  } finally {
    await loader.close();
  }
});

test("failed compatibility offers explicit plugin choices and retry without claiming success", async () => {
  const { loader, CompatibilitySummary } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      recovery: { harness_stop_required: false, harness: { state: "stopped" } },
      releases: { current_release: "old", releases: [{ id: "target" }] },
      profiles: {
        compatibility: {
          status: "needs_choice",
          source_profile: "desktop",
          release_id: "target",
          error: "Unclassified plugin startup error",
          disabled: [],
          candidates: [
            {
              package: "third-party",
              reason: "Not identified as faulty; optional isolation for troubleshooting",
            },
            { package: "broken-plugin", reason: "DSH reported a loader error for this plugin" },
          ],
        },
      },
    };
    const markup = renderToStaticMarkup(
      createElement(CompatibilitySummary, { ...props, snapshot }),
    );
    assert.match(markup, /Choose how to handle plugin errors/);
    assert.match(markup, /class="notice action-error plugin-fault"/);
    assert.match(markup, /class="status-pill bad"/);
    assert.ok(markup.indexOf("broken-plugin") < markup.indexOf("third-party"));
    assert.match(markup, /<details><summary>Error details/);
    assert.match(markup, /type="checkbox"/);
    assert.doesNotMatch(markup, /checked=""/);
    assert.match(markup, /Select failing plugins/);
    assert.match(markup, /<details><summary>Other plugins for troubleshooting/);
    assert.match(markup, /Save disabled plugins/);
    assert.match(markup, /Retry version switch/);
    assert.match(markup, /not confirmed faults/);
    assert.doesNotMatch(markup, /Startup check passed|Verified profile/);
  } finally {
    await loader.close();
  }
});

test("saved isolation is visible and reversible before another check", async () => {
  const { loader, CompatibilitySummary } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      recovery: { harness_stop_required: false, harness: { state: "stopped" } },
      profiles: { active_profile: "desktop", disabled_plugins: ["third-party"] },
    };
    const markup = renderToStaticMarkup(
      createElement(CompatibilitySummary, { ...props, snapshot }),
    );
    assert.match(markup, /Disabled plugins/);
    assert.match(markup, /Restore plugin on next check/);
    assert.match(markup, /third-party/);
    assert.doesNotMatch(markup, /Startup check passed/);
  } finally {
    await loader.close();
  }
});

test("checkpoint fixtures show legacy truth and pending retry or abort", async () => {
  const { loader, CheckpointsView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      recovery: { harness_stop_required: false, harness: { state: "stopped" } },
      checkpoints: {
        checkpoints: [
          { id: "legacy-1", profile: "web", created_at_unix: 1, state: { profile: "web" } },
        ],
        pending_restore: {
          checkpoint_id: "cp-2",
          snapshot_id: "snap-2",
          ticket_id: "ticket-2",
          state: "materialization_pending",
          retryable: true,
          abortable: true,
          error: "install failed",
        },
        healthy_capture_error: "snapshot store busy",
      },
    };
    const markup = renderToStaticMarkup(createElement(CheckpointsView, { ...props, snapshot }));
    assert.match(markup, /Legacy metadata only/);
    assert.match(markup, /materialization pending/i);
    assert.match(markup, /install failed/);
    assert.match(markup, /Retry/);
    assert.match(markup, /Abort/);
    assert.match(markup, /snapshot store busy/);
  } finally {
    await loader.close();
  }
});

test("compatibility provenance distinguishes switch checks, startup cache reuse, and old records", async () => {
  const { loader, CompatibilitySummary } = await loadViews();
  try {
    const report = {
      status: "isolated",
      source_profile: "desktop",
      release_id: "rc1",
      checked_at_unix: 1788670000,
      trigger: "version_switch",
      last_trigger: "startup",
      last_used_at_unix: 1788670200,
      cache_reused: true,
      disabled: [],
    };
    const render = (compatibility: object) =>
      renderToStaticMarkup(
        createElement(CompatibilitySummary, {
          ...props,
          snapshot: { ...baseSnapshot, profiles: { compatibility } },
        }),
      );
    const cached = render(report);
    assert.match(cached, /During version switch/);
    assert.match(cached, /Before startup or restart/);
    assert.match(cached, /Reused previous check result/);
    assert.match(cached, /Checked at/);
    assert.match(cached, /Last used/);
    assert.doesNotMatch(cached, /New check result/);
    assert.match(
      render({ ...report, cache_reused: false, last_trigger: "version_switch" }),
      /New check result/,
    );
    const legacy = render({ status: "isolated", disabled: [] });
    assert.match(legacy, /Legacy record: trigger not recorded/);
    assert.doesNotMatch(legacy, /New check result/);
  } finally {
    await loader.close();
  }
});

test("busy cold switch keeps cancellation enabled while other mutations are disabled", async () => {
  const { loader, UpdatesView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      lifecycleBusy: true,
      updates: {
        update: { state: "running" },
        operation: { operation_id: "cold-1", phase: "verifying", progress_percent: 80 },
      },
    };
    const markup = renderToStaticMarkup(
      createElement(UpdatesView, { ...props, busyAction: "Operation in progress", snapshot }),
    );
    assert.match(markup, /<button(?![^>]*disabled)[^>]*>Cancel<\/button>/);
    assert.match(markup, /<button[^>]*disabled[^>]*>Save update source<\/button>/);
  } finally {
    await loader.close();
  }
});

test("check details live in a dialog and actual startup failure overrides preflight success", async () => {
  const { loader, CompatibilityDialog, ProfilesView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      harnessRuntime: { harness: { state: "failed" } },
      recovery: {
        log_tail: [
          { stream: "stderr", content: "task-board ledger is already owned", truncated: false },
        ],
      },
      profiles: {
        compatibility: {
          status: "passed",
          trigger: "profile_switch",
          last_trigger: "profile_switch",
          source_profile: "desktop",
          release_id: "rc1",
        },
      },
    };
    const page = renderToStaticMarkup(createElement(ProfilesView, { ...props, snapshot }));
    assert.doesNotMatch(page, /Startup compatibility check|task-board ledger/);
    const dialog = renderToStaticMarkup(
      createElement(CompatibilityDialog, { ...props, snapshot, pending: false, onClose() {} }),
    );
    assert.match(dialog, /role="dialog"/);
    assert.match(dialog, /Harness startup failed/);
    assert.match(dialog, /task-board ledger/);
    assert.match(dialog, /During profile switch/);
    const busy = renderToStaticMarkup(
      createElement(CompatibilityDialog, { ...props, snapshot, pending: true, onClose() {} }),
    );
    assert.match(busy, /check continues in the background/);
    assert.doesNotMatch(busy, /Startup check passed/);
    const earlyFailure = renderToStaticMarkup(
      createElement(CompatibilityDialog, {
        ...props,
        snapshot: {
          ...baseSnapshot,
          startup: { ...startup, harness_startup_error: "Node entry missing" },
        },
        pending: false,
        onClose() {},
      }),
    );
    assert.match(earlyFailure, /Node entry missing/);
  } finally {
    await loader.close();
  }
});

test("plugin rows expose movable installed bundles, locked roots and read-only projections", async () => {
  const { loader, ProfilePlugins } = await loadViews();
  try {
    const bundles = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app", "third-party"];
    const manifest = {
      name: "desktop",
      bundles,
      plugins: bundles.map((packageName, i) => ({
        package: packageName,
        builtin: i < 2,
        removable: i === 2,
      })),
    };
    const snapshot = {
      ...baseSnapshot,
      recovery: { harness: { state: "stopped" } },
      profiles: { active_profile: "desktop", manifests: [manifest] },
    };
    const markup = renderToStaticMarkup(
      createElement(ProfilePlugins, { ...props, snapshot, profile: "desktop" }),
    );
    assert.equal((markup.match(/draggable="false"/g) || []).length, 2);
    assert.equal((markup.match(/draggable="true"/g) || []).length, 1);
    assert.match(markup, /Fixed load position/);
    assert.match(markup, /Removable/);
    const generated = renderToStaticMarkup(
      createElement(ProfilePlugins, {
        ...props,
        profile: "desktop",
        snapshot: {
          ...snapshot,
          profiles: {
            ...snapshot.profiles,
            manifests: [{ ...manifest, source_profile: "original" }],
          },
        },
      }),
    );
    assert.match(generated, /source profile original/);
    assert.doesNotMatch(generated, /draggable="true"/);
    const running = renderToStaticMarkup(
      createElement(ProfilePlugins, {
        ...props,
        profile: "desktop",
        snapshot: { ...snapshot, recovery: { harness: { state: "running" } } },
      }),
    );
    assert.doesNotMatch(running, /draggable="true"/);
  } finally {
    await loader.close();
  }
});

test("update progress keeps stage and terminal errors visible without ownership internals", async () => {
  const { loader, UpdatesView } = await loadViews();
  try {
    const render = (operation: object) =>
      renderToStaticMarkup(
        createElement(UpdatesView, {
          ...props,
          snapshot: {
            ...baseSnapshot,
            updates: { update: { state: "idle", error: "older failure" }, operation },
          },
        }),
      );
    const running = render({
      operation_id: "cold-private-id",
      tag: "v1",
      phase: "cloning",
      progress_percent: 10,
      owner_quiescent: false,
    });
    assert.match(running, /Current stage/);
    assert.match(running, /v1/);
    assert.doesNotMatch(
      running,
      /Owner quiescent|No update error reported|cold-private-id|older failure/,
    );
    const failed = render({
      operation_id: "cold-private-id",
      phase: "failed",
      error: "Clone connection failed",
      cleanup_pending: true,
    });
    assert.match(failed, /role="alert"/);
    assert.match(failed, /Clone connection failed/);
    assert.match(failed, /Retry cleanup/);
    const completed = render({
      operation_id: "cold-private-id",
      phase: "succeeded",
      progress_percent: 100,
    });
    assert.match(completed, /Last installation/);
    assert.doesNotMatch(completed, /older failure/);
  } finally {
    await loader.close();
  }
});

test("guide limits onboarding to three steps and gates completion on an installed source", async () => {
  const { loader, GuideView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      harnessRuntime: { state: "detached" },
      releases: { releases: [] },
    };
    const markup = renderToStaticMarkup(createElement(GuideView, { ...props, snapshot }));
    assert.match(markup, /Prepare/);
    assert.match(markup, /Install Harness/);
    assert.match(markup, /Check and start/);
    assert.doesNotMatch(
      markup,
      /Start and use|Release slots|Offline runtime bundle|Run basic checks/,
    );
    assert.match(markup, /Not started/);
    assert.doesNotMatch(markup, /role="dialog"/);
  } finally {
    await loader.close();
  }
});

test("a stale legacy success does not hide a running or failed cold operation", async () => {
  const { loader, UpdatesView } = await loadViews();
  try {
    const snapshot = {
      ...baseSnapshot,
      releases: { releases: [] },
      updates: {
        update: { state: "succeeded" },
        operation: { operation_id: "new", phase: "cloning", tag: "v-next" },
      },
    };
    const active = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot }));
    assert.match(active, /Cloning/);
    assert.doesNotMatch(active, /Current stage: Succeeded/);
    snapshot.updates.operation = {
      ...snapshot.updates.operation,
      phase: "failed",
      error: "download failed",
    } as any;
    assert.match(
      renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot })),
      /download failed/,
    );
    snapshot.updates.operation = {
      ...snapshot.updates.operation,
      phase: "succeeded",
      release_id: "missing",
    } as any;
    const missing = renderToStaticMarkup(createElement(UpdatesView, { ...props, snapshot }));
    assert.match(missing, /Verifying installed version/);
    assert.match(missing, /version slot is unavailable/);
  } finally {
    await loader.close();
  }
});


test("Nexus record recovery offers time selection without manual repair fields", async () => {
  const loader = await createUiTestLoader();
  try {
    const { RecoveryRecordWizard } = await loader.loadModule("/src/App.tsx");
    const markup = renderToStaticMarkup(createElement(RecoveryRecordWizard, { disabled: false }));
    assert.match(markup, /Restore Nexus records/);
    assert.match(markup, /Choose a recovery time/);
    assert.match(markup, /Restore with one click/);
    assert.match(markup, /<select[^>]*disabled/);
    assert.doesNotMatch(markup, /<input|<textarea|SHA256|Active profile name|Manual final step/);
    assert.match(markup, /Harness files, plugins and conversations are not changed/);
  } finally {
    await loader.close();
  }
});

test("blocking conflict is prominent and unrelated dependency inventory is hidden", async () => {
  const {loader, CompatibilitySummary} = await loadViews();
  try {
    const html = renderToStaticMarkup(createElement(CompatibilitySummary, {...props, snapshot: {...baseSnapshot, profiles: {active_profile: 'web', compatibility: {
      status:'needs_choice', source_profile:'web', release_id:'one', failure_stage:'plugin_loading',
      error:'TypeError: duplicate loader entry id: file-upload',
      candidates:[{package:'dsh-file-upload',reason:'Declares the duplicate loader entry ID'}],
      dependency_origins:[{package:'@noble/hashes',chains:[],incomplete:true}],
    }}}}));
    assert.match(html, /Blocking startup error/);
    assert.match(html, /Duplicate plugin entry ID: file-upload/);
    assert.match(html, /dsh-file-upload/);
    assert.doesNotMatch(html, /@noble\/hashes|Local dependency evidence is incomplete|Verified profile/);
    assert.ok(html.indexOf('Blocking startup error') < html.indexOf('Plugin version declarations'));
  } finally { await loader.close(); }
});

test("optional plugin limitation does not render a blocking alert or plugin-disable choices", async () => {
  const {loader, CompatibilitySummary} = await loadViews();
  try {
    const html=renderToStaticMarkup(createElement(CompatibilitySummary,{...props,snapshot:{...baseSnapshot,profiles:{active_profile:'web',compatibility:{
      status:'passed',source_profile:'web',release_id:'one',diagnosis:{level:'limited',summary:'Harness is ready; some optional plugins did not activate',remedy:'You can continue using Harness. Inspect only the listed optional plugins if you need their features.',evidence:['optional (plugin): missing service'],code:'optional_plugins'}
    }}}}));
    assert.match(html,/Limited functionality/);
    assert.match(html,/You can continue using Harness/);
    assert.doesNotMatch(html,/Blocking startup error|Choose how to handle plugin errors/);
  } finally {await loader.close();}
});
