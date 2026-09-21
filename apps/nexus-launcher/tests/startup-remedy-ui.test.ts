import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

const props = {
  busyAction: null,
  credentialInvalidationPending: false,
  runAction: async () => true,
  refresh: async () => undefined,
  themeMode: "system",
  setThemeMode: () => undefined,
};

const snapshotFor = (help: string, status: string, code: string) => ({
  recovery: { harness: { state: "stopped" }, harness_stop_required: false },
  releases: { releases: [] },
  updates: {},
  profiles: {
    active_profile: "web",
    disabled_plugins: [],
    compatibility: {
      status,
      source_profile: "web",
      effective_profile: "web",
      release_id: "slot-a",
      checked_at_unix: 1,
      last_trigger: "startup",
      error: "boot failed",
      failure_stage: "startup_probe",
      candidates: [],
      disabled: [],
      declarations: [],
      dependency_origins: [],
      diagnosis: { code, help, level: "blocking", certainty: "matched_signature",
        summary: "Invalid or unreadable configuration", remedy: "Repair the named configuration or patch file; preserve a backup before editing.",
        evidence: [] },
    },
  },
});

test("a blocking startup failure offers a repair entry for its diagnosis domain", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CompatibilitySummary } = await loader.loadModule("/src/App.tsx");
    const render = (help: string, status = "failed", code = "configuration") =>
      renderToStaticMarkup(
        createElement(CompatibilitySummary, {
          ...props,
          snapshot: snapshotFor(help, status, code),
          onRepair: () => undefined,
        }),
      );

    const settings = render("settings");
    assert.match(settings, /Open the profile patch file/);
    assert.match(settings, /Open Harness settings/);
    assert.match(settings, /Retry Harness startup/);

    for (const code of ["port_conflict", "permission", "runtime_arguments"]) {
      assert.match(render("settings", "failed", code), /Open Harness settings/);
      assert.doesNotMatch(render("settings", "failed", code), /Open the profile patch file/);
    }
    assert.match(render("profiles"), /Go to profile management/);
    assert.match(render("logs"), /Open the startup log/);
    // Without a candidate list there is nothing to tick, so repair is the entry.
    assert.match(render("plugins"), /Repair profile dependencies/);
    assert.match(render("plugins", "failed", "missing_module"), /Inspect local dependencies/);
    assert.doesNotMatch(render("plugins", "failed", "module_api"), /Inspect local dependencies/);
    assert.match(render("plugins", "needs_choice", "duplicate_entry"), /No individual plugin was identified/);

    // A remedy must never be offered for a domain that did not fail.
    assert.doesNotMatch(settings, /Go to profile management/);
    assert.doesNotMatch(settings, /Open the startup log/);
  } finally {
    await loader.close();
  }
});

test("repair entries that need navigation stay hidden when no handler is wired", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CompatibilitySummary } = await loader.loadModule("/src/App.tsx");
    const markup = renderToStaticMarkup(
      createElement(CompatibilitySummary, { ...props, snapshot: snapshotFor("logs", "failed", "readiness_timeout") }),
    );
    assert.doesNotMatch(markup, /Open the startup log/);
    assert.match(markup, /Retry Harness startup/);
  } finally {
    await loader.close();
  }
});

test("the self-check names the failing plugin and groups the entries waiting on it", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CompatibilitySummary } = await loader.loadModule("/src/App.tsx");
    const snapshot = snapshotFor("plugins", "failed", "required_services");
    snapshot.profiles.compatibility.diagnosis.activation = {
      entries: [
        { id: "store", package: "@deepseek-ai/dsh-store", state: "failed", reason: "failed to import", missing: [] },
        { id: "chat", package: "@deepseek-ai/dsh-client-ui-chat", state: "pending", reason: "pending", missing: ["sessions"] },
        { id: "jobs", package: "@deepseek-ai/dsh-client-ui-jobs", state: "pending", reason: "pending", missing: ["sessions"] },
      ],
      missing_services: ["sessions"],
      truncated: false,
    };
    const markup = renderToStaticMarkup(
      createElement(CompatibilitySummary, { ...props, snapshot, onRepair: () => undefined }),
    );
    assert.match(markup, /Plugins that did not activate/);
    assert.match(markup, /@deepseek-ai\/dsh-store<\/strong>: failed to import/);
    assert.match(markup, /Start from these reported failures/);
    // Consequences stay listed by name, grouped under the service they wait for.
    assert.match(markup, /sessions · Waiting plugins: 2/);
    assert.match(markup, /@deepseek-ai\/dsh-client-ui-chat/);
    assert.match(markup, /@deepseek-ai\/dsh-client-ui-jobs/);

    const orphaned = snapshotFor("plugins", "failed", "required_services");
    orphaned.profiles.compatibility.diagnosis.activation = {
      entries: [{ id: "chat", package: "@deepseek-ai/dsh-client-ui-chat", state: "pending", reason: "pending", missing: ["sessions"] }],
      missing_services: ["sessions"],
      truncated: false,
    };
    const noRootCause = renderToStaticMarkup(
      createElement(CompatibilitySummary, { ...props, snapshot: orphaned, onRepair: () => undefined }),
    );
    assert.match(noRootCause, /No plugin reported a failure of its own/);
    assert.doesNotMatch(noRootCause, /Start from these reported failures/);
  } finally {
    await loader.close();
  }
});

test("fatal activation evidence identifies the failing bundle in the repair chooser", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CompatibilitySummary } = await loader.loadModule("/src/App.tsx");
    const snapshot = snapshotFor("plugins", "needs_choice", "plugin_activation");
    Object.assign(snapshot.profiles, { api_version: "v1", manifests: [{ name: "web", bundles: ["@example/archive", "upload-addon"] }] });
    Object.assign(snapshot.profiles.compatibility, {
      checked_disabled_plugins: [],
      candidates: [{ package: "@example/archive", reason: "codec has no create() factory" }, { package: "upload-addon", reason: "Replaces built-in upload" }],
    });
    snapshot.profiles.compatibility.diagnosis.repair_candidates = [
      { package: "@example/archive", reason: "codec has no create() factory", evidence: "activation_failure" },
      { package: "upload-addon", reason: "Replaces built-in upload", evidence: "replaces_official_entry" },
    ];
    const markup = renderToStaticMarkup(createElement(CompatibilitySummary, { ...props, snapshot }));
    assert.equal(markup.split("Disable recommended plugins and retry").length - 1, 1);
    assert.equal(markup.split("Choose how to handle plugin errors").length - 1, 1);
    assert.match(markup, /<details class="startup-diagnostics"><summary>Diagnostics and manual recovery<\/summary>/);
    assert.ok(markup.indexOf("Disable recommended plugins and retry") < markup.indexOf("Diagnostics and manual recovery"));
    assert.match(markup, /plugin-fault[^]*?<strong>@example\/archive<\/strong>/);
    assert.doesNotMatch(markup, /No individual plugin was identified|No plugin is confirmed faulty/);
  } finally { await loader.close(); }
});
