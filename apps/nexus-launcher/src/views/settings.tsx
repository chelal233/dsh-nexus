import { type Snapshot, type JsonObject, type ViewProps, type ThemeMode } from "../app-types";
import {
  stringValue,
  asObject,
  arrayValue,
  nestedValue,
  harnessRuntimeValue,
  numberValue,
  booleanValue,
} from "../json-values";
import { useI18n, type Locale } from "../i18n";
import { useDraftReference, useDraftState } from "../draft-memory";
import { harnessUiMatchesRuntime } from "../harness-session";
import { launchInputValueLabel, errorMessage, harnessOptionLabel } from "../display-format";
import { Panel, PathInput, ActionButton, PageIntro, EmptyState } from "../ui-components";
import {
  launchInputMatches,
  refreshEditableDraft,
  finishDraftSave,
  replacementArgumentRows,
} from "../settings-state";
import {
  Info,
  Package,
  SlidersHorizontal,
  Gear,
  MonitorPlay,
  WarningCircle,
  Cpu,
  TerminalWindow,
  CheckCircle,
  Bell,
  Key,
} from "@phosphor-icons/react";
import { useEffect, useState, useRef, useMemo, useCallback } from "react";
import { proxyRequest } from "../agent-bridge";
import {
  preferencesDraft,
  githubRefKind,
  type HarnessPreferencesDraft,
  preferencesPayload,
} from "../harness-preferences";
import { patchPreviewExpired, runtimeSettingsGate } from "../control-state";
import {
  type HarnessConfigDraft,
  harnessDraftFromConfig,
  isLoopbackReadinessTarget,
  harnessConfigPayloadFromDraft,
  emptyHarnessDraft,
  harnessLaunchMode,
} from "../harness-config";
import {
  type RuntimeStatusViewState,
  createRuntimeStatusController,
  RuntimeStatusPanel,
} from "../runtime-status";
import { displayZoom, ZOOM_CHANGED, setDisplayZoom, ZOOM_LEVELS } from "../display-preferences";
import {
  notificationsEnabledPreference,
  setNotificationsEnabledPreference,
} from "../notifications";
import { invoke } from "../desktop";
import { useDesktopUpdate } from "../desktop-update";

export function HarnessArgumentReference({ snapshot }: { snapshot: Snapshot }) {
  const { t } = useI18n();
  const version =
    stringValue(asObject(snapshot.config?.external_harness), "version") ||
    stringValue(
      arrayValue(snapshot.releases, "releases").find(
        (item) => stringValue(item, "id") === stringValue(snapshot.releases, "current_release"),
      ),
      "version",
    );
  const verified = version === "0.1.2-rc.1" || version === "dsh-v0.1.2-rc.1";
  const options = [
    ["--port", "Web profile", "Listening port; prefer the Web port setting."],
    ["--no-open", "Web profile", "Do not open a browser; prefer the browser setting."],
    [
      "--profile",
      "Managed by Nexus",
      "Selected in Configuration and plugins; added automatically.",
    ],
    [
      "--patch",
      "Managed by Nexus",
      "Use Runtime configuration patches for ordering, caching and failure protection.",
    ],
    [
      "--dump-config",
      "Terminal only",
      "Print the composed configuration and exit; do not use for service startup.",
    ],
    [
      "--dump-default-config",
      "Terminal only",
      "Print the default configuration and exit; do not use for service startup.",
    ],
    ["--help", "Terminal only", "Show command help and exit."],
    ["--version", "Terminal only", "Show the version and exit."],
  ];
  return (
    <>
      <datalist id="harness-argument-options">
        {verified && ["--port", "--no-open"].map((flag) => <option key={flag} value={flag} />)}
      </datalist>
      <details className="advanced-settings">
        <summary>{t("Argument reference")}</summary>
        <p>
          {t(
            verified
              ? "Reference verified for Harness 0.1.2-rc.1. Profile-specific arguments may differ."
              : "Current version is unverified. This reference describes 0.1.2-rc.1; automatic suggestions are disabled.",
          )}
        </p>
        <div className="table-scroll">
          <table className="argument-reference">
            <thead>
              <tr>
                <th>{t("Argument")}</th>
                <th>{t("Applies to")}</th>
                <th>{t("Description")}</th>
              </tr>
            </thead>
            <tbody>
              {options.map(([flag, scope, description]) => (
                <tr key={flag}>
                  <td>
                    <code>{flag}</code>
                  </td>
                  <td>{t(scope)}</td>
                  <td>{t(description)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </>
  );
}

/// Guided setup: the one-stop flow as a wizard. Steps check themselves from
/// live state and advance automatically; starting Harness stays an explicit
/// button press (no implicit start, per contract).
// A dirty editor retains the revision it was based on across background polling.
export function useDraftRevision(config: unknown, dirty: boolean, key: string): string {
  const revision = stringValue(asObject(config), "revision") || "";
  const base = useDraftReference(key, revision);
  if (!dirty) base.current = revision;
  return base.current;
}

export function LaunchInputsPanel({ snapshot }: { snapshot: Snapshot }) {
  const { t } = useI18n();
  const explanation = nestedValue(snapshot.config, "launch_inputs");
  const recorded = asObject(explanation.running_launch);
  const current = launchInputMatches(recorded, asObject(snapshot.harnessRuntime)) ? recorded : {};
  let observedPort: string | null = null;
  if (
    Object.keys(current).length &&
    harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi)
  ) {
    try {
      const url = new URL(stringValue(snapshot.harnessUi, "url") || "");
      observedPort = url.port || "80";
    } catch {
      /* No verified URL yet. */
    }
  }
  const show = (value: JsonObject) =>
    Object.keys(value).length === 0 ? (
      <p>{t("Launch input record unavailable")}</p>
    ) : (
      <dl className="detail-list">
        {arrayValue(value, "fields").map((item, index) => {
          const row = asObject(item);
          return (
            <div key={index}>
              <dt>{t(stringValue(row, "name") || "Unknown")}</dt>
              <dd>
                {launchInputValueLabel(row, t)}
                <small> · {t(stringValue(row, "source") || "Unknown")}</small>
              </dd>
            </div>
          );
        })}
      </dl>
    );
  return (
    <Panel title={t("Launch configuration explained")} icon={<Info size={18} />}>
      <p className="field-help">
        {t(
          "These are launch inputs, not the final configuration after Harness applies patches. Inherited values have not been inspected.",
        )}
      </p>
      <h3>{t("Next launch inputs")}</h3>
      <p>{t("Changes apply on the next explicit launch.")}</p>
      {show(asObject(explanation.next_launch))}
      <h3>{t("Current instance launch inputs")}</h3>
      {show(current)}
      {observedPort && (
        <p>
          {t("Observed current port")}: {observedPort}
        </p>
      )}
    </Panel>
  );
}

export function HarnessSourcePanel({
  snapshot,
  busyAction,
  runAction,
}: Pick<ViewProps, "snapshot" | "busyAction" | "runAction">) {
  const { t } = useI18n();
  const source = asObject(snapshot.config?.external_harness);
  const savedPath = stringValue(source, "root") || "";
  const [path, setPath] = useDraftState("external-source-path", savedPath);
  const [dirty, setDirty] = useDraftState("external-source-dirty", false);
  const revision = useDraftRevision(snapshot.config, dirty, "external-harness");
  useEffect(() => {
    if (!dirty) setPath(savedPath);
  }, [savedPath, dirty]);
  const state = stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state") || "";
  const disabled = busyAction !== null || !["stopped", "detached", "failed"].includes(state);
  const choose = async (external: boolean) => {
    if (disabled) return;
    if (
      await runAction(t("Select Harness source"), "/v1/config", {
        action: external ? "set_external_harness" : "clear_external_harness",
        expected_revision: revision,
        ...(external ? { external_harness_path: path.trim() } : {}),
      })
    )
      setDirty(false);
  };
  return (
    <Panel title={t("Harness program source")} icon={<Package size={18} />}>
      <p>{savedPath ? t("External directory") : t("Installed version slots")}</p>
      {savedPath && (
        <dl className="detail-list">
          <dt>{t("External directory")}</dt>
          <dd>{savedPath}</dd>
          <dt>{t("Version")}</dt>
          <dd>
            {stringValue(source, "version") === "unknown"
              ? t("Unknown")
              : stringValue(source, "version")}
          </dd>
        </dl>
      )}
      <p className="field-help">
        {t(
          "Nexus reads an already built Harness directory. It does not install, build, update, copy or remove that program. Harness and plugins retain their normal system permissions.",
        )}
      </p>
      <label className="form-field">
        <span>{t("External Harness directory")}</span>
        <PathInput
          value={path}
          directory
          disabled={disabled}
          onChange={(value) => {
            setPath(value);
            setDirty(true);
          }}
        />
      </label>
      <p className="field-help">
        {t(
          "Directory identity, file names, sizes and modification times are checked, with content hashes for key manifests and the CLI entry. Ordinary changes require confirmation again; this is not supply-chain authentication.",
        )}
      </p>
      <div className="button-row">
        <ActionButton disabled={disabled || !path.trim()} onClick={() => void choose(true)}>
          {t("Confirm external directory")}
        </ActionButton>
        <ActionButton disabled={disabled || !savedPath} onClick={() => void choose(false)}>
          {t("Switch back to Nexus-managed Harness")}
        </ActionButton>
        <ActionButton
          disabled={!dirty || busyAction !== null}
          onClick={() => {
            setPath(savedPath);
            setDirty(false);
          }}
        >
          {t("Discard changes")}
        </ActionButton>
      </div>
    </Panel>
  );
}

function HarnessPreferencesPanel({
  snapshot,
  busyAction,
  runAction,
  children,
}: Pick<ViewProps, "snapshot" | "busyAction" | "runAction"> & { children?: React.ReactNode }) {
  const { t } = useI18n();
  const saved = nestedValue(asObject(snapshot.config), "harness_preferences");
  const [draft, setDraft] = useDraftState("preferences.value", () => preferencesDraft(saved));
  const [dirty, setDirty] = useDraftState("preferences.dirty", false);
  const preferencesRevision = useDraftRevision(snapshot.config, dirty, "preferences.revision");
  const [error, setError] = useState<string | null>(null);
  const [patchBusy, setPatchBusy] = useState(false);
  const [expandedPatch, setExpandedPatch] = useState<number | null>(null);
  const [patchPreview, setPatchPreview] = useState<JsonObject | null>(null);
  const [previewNow, setPreviewNow] = useState(Date.now);
  useEffect(() => {
    if (!patchPreview) return;
    setPreviewNow(Date.now());
    const timer = setInterval(() => setPreviewNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [patchPreview]);
  const previewExpired = patchPreviewExpired(patchPreview?.expires_at_unix, previewNow);
  const [previewDraft, setPreviewDraft] = useState("");
  const [refLists, setRefLists] = useState<Record<string, JsonObject>>({});
  useEffect(() => {
    if (!dirty) setDraft(preferencesDraft(saved));
  }, [dirty, snapshot.config]);
  const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
  const updates = asObject(snapshot.updates);
  const operation = asObject(updates.operation);
  const gate = runtimeSettingsGate(
    runtime.state,
    numberValue(runtime, "pid"),
    asObject(updates.update).state,
    operation.phase,
    booleanValue(operation, "cleanup_pending"),
    busyAction !== null,
  );
  const disabled = gate.disabled || patchBusy || snapshot.startup?.available !== true;
  const refKey = (index: number) =>
    JSON.stringify([
      index,
      draft.patch_entries[index]?.source,
      draft.patch_entries[index] && githubRefKind(draft.patch_entries[index]),
    ]);
  const loadRefs = async (index: number, page = 1) => {
    if (disabled) return;
    setPatchBusy(true);
    setError(null);
    const key = refKey(index);
    try {
      const result = await proxyRequest<JsonObject>("/v1/config", "POST", {
        action: "list_harness_patch_refs",
        expected_revision: preferencesRevision,
        patch_query: {
          entry: {
            ...draft.patch_entries[index],
            github_ref_kind: githubRefKind(draft.patch_entries[index]),
          },
          page,
        },
      });
      setRefLists((current) => ({ ...current, [key]: result }));
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setPatchBusy(false);
    }
  };
  const applyPreview = async () => {
    if (
      disabled ||
      !patchPreview ||
      patchPreviewExpired(patchPreview.expires_at_unix, Date.now()) ||
      previewDraft !== JSON.stringify(draft)
    )
      return;
    if (
      await runAction(t("Apply previewed patches"), "/v1/config", {
        action: "apply_harness_patch_preview",
        expected_revision: preferencesRevision,
        patch_query: { preview_id: patchPreview.preview_id },
      })
    ) {
      setPatchPreview(null);
      setDirty(false);
    }
  };
  const discardPreview = async () => {
    if (disabled || !patchPreview) return;
    setPatchBusy(true);
    setError(null);
    try {
      await proxyRequest("/v1/config", "POST", {
        action: "discard_harness_patch_preview",
        expected_revision: preferencesRevision,
        patch_query: { preview_id: patchPreview.preview_id },
      });
      setPatchPreview(null);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setPatchBusy(false);
    }
  };
  const change = (key: Exclude<keyof HarnessPreferencesDraft, "patch_entries">, value: string) => {
    setDraft((current) => ({ ...current, [key]: value }));
    setDirty(true);
    setError(null);
  };
  const field = (
    key: Exclude<keyof HarnessPreferencesDraft, "patch_entries">,
    label: string,
    help?: string,
  ) => (
    <label className="form-field" key={key}>
      <span className="field-label">{t(label)}</span>
      {["home", "agents_home", "bundled_skill_dir"].includes(key) ? (
        <PathInput
          value={draft[key]}
          directory
          disabled={disabled}
          placeholder={t("Inherit upstream default")}
          onChange={(value) => change(key, value)}
        />
      ) : (
        <input
          className="form-input"
          value={draft[key]}
          disabled={disabled}
          placeholder={t("Inherit upstream default")}
          onChange={(event) => change(key, event.target.value)}
        />
      )}
      {help && <span className="field-help">{t(help)}</span>}
    </label>
  );
  const choice = (
    key: Exclude<keyof HarnessPreferencesDraft, "patch_entries">,
    label: string,
    options: string[],
  ) => (
    <label className="form-field" key={key}>
      <span className="field-label">{t(label)}</span>
      <select
        className="form-input"
        value={draft[key]}
        disabled={disabled}
        onChange={(event) => change(key, event.target.value)}
      >
        <option value="">{t("Inherit upstream default")}</option>
        {options.map((value) => (
          <option key={value} value={value}>
            {harnessOptionLabel(value, t)}
          </option>
        ))}
      </select>
    </label>
  );
  const editPatches = (entries: HarnessPreferencesDraft["patch_entries"]) => {
    setDraft((current) => ({ ...current, patches: "", patch_entries: entries }));
    setDirty(true);
    setError(null);
  };
  const movePatch = (index: number, direction: number) => {
    const entries = [...draft.patch_entries];
    [entries[index], entries[index + direction]] = [entries[index + direction], entries[index]];
    editPatches(entries);
    setExpandedPatch((current) =>
      current === index ? index + direction : current === index + direction ? index : current,
    );
  };
  const save = async (download = false) => {
    if (disabled) return;
    const result = preferencesPayload(draft);
    if (result.error) {
      setError(result.error);
      return;
    }
    if (download) {
      setPatchBusy(true);
      setError(null);
      setPatchPreview(null);
      const captured = JSON.stringify(draft);
      try {
        const preview = await proxyRequest<JsonObject>("/v1/config", "POST", {
          action: "preview_harness_patches",
          expected_revision: preferencesRevision,
          harness_preferences: result.value,
        });
        setPatchPreview(preview);
        setPreviewDraft(captured);
      } catch (cause) {
        setError(errorMessage(cause));
      } finally {
        setPatchBusy(false);
      }
      return;
    }
    if (
      await runAction(t("Save Harness preferences"), "/v1/config", {
        action: download ? "fetch_harness_patches" : "set_harness_preferences",
        expected_revision: preferencesRevision,
        harness_preferences: result.value,
      })
    )
      setDirty(false);
  };
  return (
    <Panel title={t("Harness configuration")} icon={<SlidersHorizontal size={18} />}>
      <p className="field-help">
        {t("Blank fields inherit upstream behavior. Changes apply on the next launch.")}
      </p>
      <div className="form-grid">
        {field(
          "home",
          "Harness data directory",
          "Changing this path only changes where Harness looks for data. Existing files are not moved or deleted.",
        )}
        {field("port", "Web port", "Web profiles only. Default 3080; 0 selects an available port.")}
        {choice("open_browser", "Open browser after launch", ["true", "false"])}
        {choice("telemetry_disabled", "Disable session telemetry", ["true", "false"])}
      </div>
      <p className="field-help">
        {t(
          "Disabling telemetry stops session sharing. Inherited upstream behavior shares session records when feedback is submitted.",
        )}
      </p>
      <details>
        <summary>{t("Advanced Harness preferences")}</summary>
        <div className="form-grid">
          {field("deepseek_base_url", "DeepSeek model API address")}
          {field("search_base_url", "DeepSeek search API address")}
          {field(
            "search_provider",
            "Search provider ID",
            "The named provider must already be installed and available.",
          )}
          {field(
            "fetch_provider",
            "Web fetch provider ID",
            "The named provider must already be installed and available.",
          )}
          {field("agents_home", "Shared agent skills directory")}
          {field("bundled_skill_dir", "Bundled skills directory")}
          {choice("permission_mode", "Permission mode", [
            "read-only",
            "workspace-write",
            "danger-full-access",
          ])}
          {choice("tools_mode", "Tool mode (temporary upstream option)", ["native", "ptc", "both"])}
        </div>
        <p className="field-help">
          {t(
            "Danger full access removes the default sandbox restrictions and automatic approval prompts. Tool mode applies to web and headless profiles.",
          )}
        </p>
        <h3>{t("Runtime configuration patches")}</h3>
        <p className="field-help">
          {t(
            "Applied from top to bottom after the profile configuration. Later patches act on the result of earlier patches. Save and restart Harness to apply changes.",
          )}
        </p>
        <div className="patch-list">
          {draft.patch_entries.map((entry, index) => (
            <section className="patch-entry" key={index}>
              <div className="profile-entry-header patch-entry-header">
                <button
                  type="button"
                  className="profile-row-toggle patch-row-toggle"
                  aria-expanded={expandedPatch === index}
                  aria-controls={`patch-editor-${index}`}
                  onClick={() => setExpandedPatch(expandedPatch === index ? null : index)}
                >
                  <span className="profile-chevron" aria-hidden="true">
                    {expandedPatch === index ? "▾" : "▸"}
                  </span>
                  <span className="patch-number">{index + 1}</span>
                  <strong className="patch-source" title={entry.source}>
                    {entry.source || t("Add patch")}
                  </strong>
                </button>
                <div className="button-row patch-row-actions">
                  <label>
                    <input
                      type="checkbox"
                      disabled={disabled}
                      checked={entry.enabled}
                      onChange={(event) =>
                        editPatches(
                          draft.patch_entries.map((item, i) =>
                            i === index ? { ...item, enabled: event.target.checked } : item,
                          ),
                        )
                      }
                    />
                    {t("Enabled")}
                  </label>
                  <ActionButton
                    disabled={disabled || index === 0}
                    onClick={() => movePatch(index, -1)}
                  >
                    {t("Move up")}
                  </ActionButton>
                  <ActionButton
                    disabled={disabled || index + 1 === draft.patch_entries.length}
                    onClick={() => movePatch(index, 1)}
                  >
                    {t("Move down")}
                  </ActionButton>
                  <ActionButton
                    tone="danger"
                    disabled={disabled}
                    onClick={() => {
                      editPatches(draft.patch_entries.filter((_, i) => i !== index));
                      setExpandedPatch((current) =>
                        current === index
                          ? null
                          : current !== null && current > index
                            ? current - 1
                            : current,
                      );
                    }}
                  >
                    {t("Remove patch entry")}
                  </ActionButton>
                </div>
              </div>
              <div
                id={`patch-editor-${index}`}
                className="patch-editor form-field"
                hidden={expandedPatch !== index}
              >
                <label>
                  <span className="field-label">
                    {t("Local absolute path or HTTPS / GitHub file URL")}
                  </span>
                  <input
                    className="form-input"
                    disabled={disabled}
                    value={entry.source}
                    onChange={(event) =>
                      editPatches(
                        draft.patch_entries.map((item, i) =>
                          i === index
                            ? { source: event.target.value, enabled: item.enabled }
                            : item,
                        ),
                      )
                    }
                  />
                </label>
                {entry.source.startsWith("https://github.com/") && (
                  <div className="form-grid">
                    <label>
                      <span className="field-label">{t("GitHub reference type")}</span>
                      <select
                        className="form-input"
                        disabled={disabled}
                        value={githubRefKind(entry)}
                        onChange={(event) =>
                          editPatches(
                            draft.patch_entries.map((item, i) =>
                              i === index
                                ? {
                                    ...item,
                                    github_ref_kind: event.target.value,
                                    github_ref_name:
                                      item.github_ref_name ??
                                      item.source.split("/blob/")[1]?.split("/")[0] ??
                                      "",
                                    sha256: undefined,
                                    cache_identity: undefined,
                                    resolved_commit: undefined,
                                  }
                                : item,
                            ),
                          )
                        }
                      >
                        <option value="branch">{t("Branch")}</option>
                        <option value="tag">{t("Tag")}</option>
                        <option value="commit">{t("Commit")}</option>
                      </select>
                    </label>
                    <label>
                      <span className="field-label">{t("GitHub reference name")}</span>
                      <input
                        className="form-input"
                        disabled={disabled}
                        value={
                          entry.github_ref_name ??
                          entry.source.split("/blob/")[1]?.split("/")[0] ??
                          ""
                        }
                        onChange={(event) =>
                          editPatches(
                            draft.patch_entries.map((item, i) =>
                              i === index
                                ? {
                                    ...item,
                                    github_ref_kind: githubRefKind(item),
                                    github_ref_name: event.target.value,
                                    sha256: undefined,
                                    cache_identity: undefined,
                                    resolved_commit: undefined,
                                  }
                                : item,
                            ),
                          )
                        }
                      />
                    </label>
                    {entry.resolved_commit && (
                      <span className="field-help">
                        {t("Cached commit")}: {entry.resolved_commit}
                      </span>
                    )}
                    {githubRefKind(entry) !== "commit" && (
                      <div className="button-row">
                        <ActionButton disabled={disabled} onClick={() => void loadRefs(index)}>
                          {t("Load branches or tags")}
                        </ActionButton>
                        {refLists[refKey(index)] && (
                          <select
                            className="form-input"
                            disabled={disabled}
                            value=""
                            aria-label={t("Choose GitHub reference")}
                            onChange={(event) => {
                              if (event.target.value)
                                editPatches(
                                  draft.patch_entries.map((item, i) =>
                                    i === index
                                      ? {
                                          ...item,
                                          github_ref_kind: githubRefKind(item),
                                          github_ref_name: event.target.value,
                                          sha256: undefined,
                                          cache_identity: undefined,
                                          resolved_commit: undefined,
                                        }
                                      : item,
                                  ),
                                );
                            }}
                          >
                            <option value="">{t("Choose GitHub reference")}</option>
                            {arrayValue(refLists[refKey(index)], "entries").map((value) => {
                              const ref = asObject(value);
                              return (
                                <option
                                  key={stringValue(ref, "name")}
                                  value={stringValue(ref, "name")}
                                >
                                  {stringValue(ref, "name")} ·{" "}
                                  {stringValue(ref, "commit")?.slice(0, 12)}
                                </option>
                              );
                            })}
                          </select>
                        )}
                        {numberValue(refLists[refKey(index)] ?? {}, "next_page") && (
                          <ActionButton
                            disabled={disabled}
                            onClick={() =>
                              void loadRefs(
                                index,
                                numberValue(refLists[refKey(index)], "next_page")!,
                              )
                            }
                          >
                            {t("More references")}
                          </ActionButton>
                        )}
                      </div>
                    )}
                    <label>
                      <span className="field-label">{t("GitHub file path")}</span>
                      <input
                        className="form-input"
                        disabled={disabled}
                        value={
                          entry.github_file_path ??
                          entry.source.split("/blob/")[1]?.split("/").slice(1).join("/") ??
                          ""
                        }
                        onChange={(event) =>
                          editPatches(
                            draft.patch_entries.map((item, i) =>
                              i === index
                                ? {
                                    ...item,
                                    github_file_path: event.target.value,
                                    sha256: undefined,
                                    cache_identity: undefined,
                                    resolved_commit: undefined,
                                  }
                                : item,
                            ),
                          )
                        }
                      />
                    </label>
                  </div>
                )}
                <span className="field-help">
                  {entry.sha256
                    ? `${t("Cached SHA256")}: ${entry.sha256}`
                    : entry.source.startsWith("https://")
                      ? t("Not downloaded")
                      : t("Local file")}
                </span>
              </div>
            </section>
          ))}
        </div>
        <div className="button-row">
          <ActionButton
            disabled={disabled || draft.patch_entries.length >= 32}
            onClick={() => {
              setExpandedPatch(draft.patch_entries.length);
              editPatches([...draft.patch_entries, { source: "", enabled: true }]);
            }}
          >
            {t("Add patch")}
          </ActionButton>
          <ActionButton
            disabled={
              disabled ||
              !draft.patch_entries.some(
                (entry) => entry.enabled && entry.source.startsWith("https://"),
              )
            }
            onClick={() => void save(true)}
          >
            {t("Preview remote patch update")}
          </ActionButton>
        </div>
        {patchPreview && (
          <div className="status-block">
            <strong>{t("Patch update preview")}</strong>
            <p role="status">
              {previewExpired
                ? t("This patch preview has expired. Preview again before applying.")
                : t("Preview valid for {seconds} more seconds", {
                    seconds: Math.max(
                      0,
                      Math.ceil(Number(patchPreview.expires_at_unix) - previewNow / 1000),
                    ),
                  })}
            </p>
            <p>
              {t(
                "Preview downloads candidates but does not save settings. Apply uses these exact cached files without downloading again. Changes are a bounded, redacted line comparison; unchanged or sensitive text may be omitted.",
              )}
            </p>
            {arrayValue(patchPreview, "entries").map((value, index) => {
              const row = asObject(value);
              const changes = asObject(row.changes);
              return (
                <details key={index}>
                  <summary>{stringValue(row, "source")}</summary>
                  <p>
                    {t("Previous SHA256")}: {stringValue(row, "old_sha256") || t("Not available")}
                  </p>
                  {row.old_bytes == null && (
                    <p>
                      {t(
                        "No previous content is available for comparison. The preview shows candidate content, not a verified set of additions.",
                      )}
                    </p>
                  )}
                  <p>
                    {t("Candidate SHA256")}: {stringValue(row, "new_sha256")}
                  </p>
                  <p>
                    {t("Cached commit")}: {stringValue(row, "old_commit") || t("Not available")} →{" "}
                    {stringValue(row, "new_commit") || t("Not available")}
                  </p>
                  <pre>
                    {arrayValue(changes, "lines")
                      .map((value) => {
                        const line = asObject(value);
                        return `${line.line}: - ${line.before ?? ""}\n${line.line}: + ${line.after ?? ""}`;
                      })
                      .join("\n")}
                  </pre>
                  {booleanValue(changes, "truncated") && <p>{t("Preview truncated")}</p>}
                </details>
              );
            })}
            {previewDraft !== JSON.stringify(draft) && (
              <p>{t("Draft changed; create a new preview before applying.")}</p>
            )}
            <div className="button-row">
              <ActionButton
                disabled={disabled || previewExpired || previewDraft !== JSON.stringify(draft)}
                onClick={() => void applyPreview()}
              >
                {t("Apply previewed patches")}
              </ActionButton>
              <ActionButton disabled={disabled} onClick={() => void discardPreview()}>
                {t("Cancel preview")}
              </ActionButton>
            </div>
          </div>
        )}
        <p className="field-help">
          {t(
            "Remote patches are cached locally and never downloaded at startup. Preview downloads candidates; only Apply saves this draft. Failed downloads retain the previous configuration and block affected enabled patches. Only self-contained UTF-8 files up to 1 MiB are supported; relative remote file dependencies are not downloaded.",
          )}
        </p>
        <p className="field-help">
          {t(
            "Patch files customize plugins and are applied in the listed order. Select only files you trust.",
          )}
        </p>
        <p className="field-help">
          {t(
            "Branches and tags are resolved only when you download explicitly. Startup uses the cached commit without contacting GitHub. After a patch failure, disable it and save before retrying. With patches enabled, automatic browser opening is suppressed; open Harness after its health check passes.",
          )}
        </p>
        <div className="form-grid">
          {field("context_window", "Context window (sdk-minimal only)")}
          {choice("max_tokens_as_success", "Treat token limit as success (sdk only)", [
            "true",
            "false",
          ])}
        </div>
        <label className="form-field">
          <span className="field-label">{t("System prompt (sdk-minimal only)")}</span>
          <textarea
            className="form-input"
            rows={3}
            value={draft.system_prompt}
            disabled={disabled}
            placeholder={t("Inherit upstream default")}
            onChange={(event) => change("system_prompt", event.target.value)}
          />
        </label>
      </details>
      {error && (
        <p className="form-error" role="alert">
          {t(error)}
        </p>
      )}
      {disabled && (
        <p className="field-help" role="status">
          {t(
            "Stop Harness and wait for updates and cleanup to finish before changing preferences.",
          )}
        </p>
      )}
      <div className="button-row">
        <ActionButton disabled={disabled || !dirty} onClick={() => void save()}>
          {t("Save Harness preferences")}
        </ActionButton>
        <ActionButton
          disabled={!dirty || busyAction !== null}
          onClick={() => {
            setDraft(preferencesDraft(saved));
            setDirty(false);
            setError(null);
          }}
        >
          {t("Discard changes")}
        </ActionButton>
      </div>
      {children}
    </Panel>
  );
}

export function SettingsView({
  snapshot,
  themeMode,
  setThemeMode,
  busyAction,
  runAction,
  repairSection,
}: ViewProps) {
  const scrollSection = (id: string) =>
    document.getElementById(`settings-${id}`)?.scrollIntoView({ block: "start" });
  useEffect(() => {
    if (repairSection) scrollSection(repairSection.section);
  }, [repairSection]);
  const [zoom, setZoom] = useState(displayZoom);
  useEffect(() => {
    const sync = (event: Event) => setZoom((event as CustomEvent<number>).detail);
    window.addEventListener(ZOOM_CHANGED, sync);
    return () => window.removeEventListener(ZOOM_CHANGED, sync);
  }, []);
  const { locale, setLocale, t } = useI18n();
  const [notificationsEnabled, setNotificationsEnabled] = useState(
    notificationsEnabledPreference(),
  );
  const [autostartEnabled, setAutostartEnabled] = useState<boolean | null>(null);
  const desktopUpdate = useDesktopUpdate();
  const [desktopUpdateError, setDesktopUpdateError] = useState("");
  const [buildIdentity, setBuildIdentity] = useState<Record<string, unknown>>({});
  useEffect(() => {
    void invoke<Record<string, unknown>>("build_identity")
      .then(setBuildIdentity)
      .catch(() => undefined);
  }, []);
  const [armedReset, setArmedReset] = useState<string | null>(null);
  const resetRevision = useRef("");
  const [logLevel, setLogLevel] = useState<string>(
    () => window.localStorage.getItem("nexus.launcher.agent-log-level") || "info",
  );
  useEffect(() => {
    // Re-apply the persisted level whenever settings open; best-effort in
    // browser-only previews.
    void invoke("agent_log_set", { level: logLevel }).catch(() => undefined);
  }, [logLevel]);
  useEffect(() => {
    // Best-effort: the launcher desktop bundle answers; browser-only
    // development previews stay with an unavailable checkbox.
    invoke<boolean>("autostart_status")
      .then((value) => setAutostartEnabled(value === true))
      .catch(() => setAutostartEnabled(null));
  }, []);
  const toggleAutostart = async (enabled: boolean) => {
    try {
      await invoke("autostart_set", { enabled });
      setAutostartEnabled(enabled);
    } catch {
      setAutostartEnabled(null);
    }
  };
  const config = asObject(snapshot.config);
  const harness = nestedValue(config, "harness");
  const harnessEnvOverride = booleanValue(config, "harness_env_override");
  const hasHarnessConfig = Object.keys(harness).length > 0;
  const harnessRuntime = harnessRuntimeValue(snapshot.harnessRuntime);
  const harnessState = stringValue(harnessRuntime, "state");
  const harnessInTransition = harnessState === "starting" || harnessState === "stopping";
  const configControlsDisabled =
    busyAction !== null ||
    snapshot.startup?.available !== true ||
    harnessInTransition ||
    harnessState === "running";
  const [editingHarness, setEditingHarness] = useDraftState("harness.editing", !hasHarnessConfig);
  const [draft, setDraft] = useDraftState<HarnessConfigDraft>("harness.value", () =>
    harnessDraftFromConfig(config),
  );
  const [draftDirty, setDraftDirty] = useDraftState("harness.dirty", false);
  const harnessRevision = useDraftRevision(snapshot.config, draftDirty, "harness.revision");
  const [formError, setFormError] = useState<string | null>(null);
  const [argRows, setArgRows] = useDraftState<Array<{ key: string; value: string }>>(
    "harness.arguments",
    [],
  );

  const draftDirtyRef = useRef(false);

  useEffect(() => {
    setFormError(null);
  }, [locale]);

  useEffect(() => {
    draftDirtyRef.current = draftDirty;
  }, [draftDirty]);

  useEffect(() => {
    if (!draftDirty) {
      setDraft(harnessDraftFromConfig(asObject(snapshot.config)));
      setEditingHarness(
        Object.keys(nestedValue(asObject(snapshot.config), "harness")).length === 0,
      );
    }
  }, [draftDirty, snapshot.config]);

  const updateDraft = (field: keyof HarnessConfigDraft, value: string | boolean) => {
    setDraft((current) => ({
      ...current,
      [field]: value,
      ...(field === "readinessUrl" && typeof value === "string" && !value.trim()
        ? { readinessTokenRequired: false }
        : {}),
      ...(field === "readinessUrl" ? { readinessUrlRedacted: false } : {}),
    }));
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setFormError(null);
  };

  const openEditor = () => {
    const nextDraft = harnessDraftFromConfig(config);
    setDraft(nextDraft);
    setArgRows(argsToRows(nextDraft.args));
    // Keep the editor open while the background poll refreshes runtime data.
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setFormError(null);
    setEditingHarness(true);
  };

  const saveHarness = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setFormError(null);
    const runtimePinPath = stringValue(nestedValue(config.runtime, "node"), "path") || "";
    const program = draft.program.trim() || runtimePinPath || "node";
    const entry = draft.entry.trim() || "{release_root}/apps/cli/lib/bin.js";
    if (
      draft.mode === "node" &&
      (numberValue(snapshot.health, "harness_config_wire_version") ?? 0) < 2
    ) {
      setFormError(
        t(
          "This Agent does not advertise the explicit Node Harness configuration contract. Update Agent before saving Node mode.",
        ),
      );
      return;
    }
    const readinessUrl = draft.readinessUrl.trim();
    if (readinessUrl && !isLoopbackReadinessTarget(readinessUrl)) {
      setFormError(t("Readiness target must be an HTTP or TCP loopback URL."));
      return;
    }
    const timeoutText = draft.timeout.trim();
    let timeout: number | undefined;
    if (timeoutText) {
      const parsed = Number(timeoutText);
      if (!Number.isInteger(parsed) || parsed <= 0 || parsed > 86400) {
        setFormError(t("Timeout must be a positive integer."));
        return;
      }
      timeout = parsed;
    }
    if (draft.argsRedacted && !draft.replaceRedactedArgs) {
      setFormError(t("Replace hidden arguments before saving."));
      return;
    }
    const incompleteRow = argRows.find(
      (row) => !row.key.trim() || (row.key.trim() === "--" && !row.value.trim()),
    );
    if (incompleteRow !== undefined) {
      setFormError(t("Finish or remove the empty argument row before saving."));
      return;
    }
    if (argRows.some((row) => row.key.trim() === "--profile" && row.value.trim() !== "{profile}")) {
      setFormError(
        t(
          "Profile arguments are managed automatically. Select the profile in Configuration and plugins.",
        ),
      );
      return;
    }
    let argsText = rowsToArgsText(argRows);
    if (!argRows.some((row) => row.key.trim() === "--profile")) {
      argsText = argsText ? `--profile\n{profile}\n${argsText}` : "--profile\n{profile}";
    }
    if (argsText.includes("[REDACTED]")) {
      setFormError(t("Replace hidden arguments before saving."));
      return;
    }
    const harnessPayload = harnessConfigPayloadFromDraft({
      ...draft,
      mode: "node",
      program,
      entry,
      args: argsText,
    });
    harnessPayload.readiness_url = readinessUrl || null;
    harnessPayload.readiness_timeout_secs = timeout ?? null;
    const saved = await runAction(t("Save Harness configuration"), "/v1/config", {
      action: "set_harness",
      expected_revision: harnessRevision,
      harness: harnessPayload,
      preserve_harness_readiness_url: draft.readinessUrlRedacted,
    });
    if (saved === true) {
      draftDirtyRef.current = false;
      setDraftDirty(false);
      setEditingHarness(false);
      setFormError(null);
    }
  };

  const clearHarness = async () => {
    if (
      !window.confirm(t("Remove the Harness launch configuration? Harness must be stopped first."))
    )
      return;
    const cleared = await runAction(t("Clear Harness configuration"), "/v1/config", {
      action: "clear_harness",
      expected_revision: harnessRevision,
    });
    if (cleared === true) {
      setDraft(emptyHarnessDraft);
      setArgRows([]);
      draftDirtyRef.current = false;
      setDraftDirty(false);
      setEditingHarness(true);
      setFormError(null);
    }
  };

  const configuredMode = harnessLaunchMode(harness);
  const configuredArgs = arrayValue(harness, "args").filter(
    (item): item is string => typeof item === "string",
  );
  const configuredEntry =
    configuredMode === "node"
      ? stringValue(harness, "entry") || configuredArgs[0] || t("Not configured")
      : undefined;

  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatusViewState>({
    phase: "idle",
    status: null,
    error: null,
  });
  const runtimeController = useMemo(
    () =>
      createRuntimeStatusController(
        (path, method) => proxyRequest<unknown>(path, method),
        setRuntimeStatus,
      ),
    [],
  );
  const checkRuntime = useCallback(async () => {
    await runtimeController.check(snapshot.startup?.available === true);
  }, [runtimeController, snapshot.startup?.available]);
  const updatesSnapshot = asObject(snapshot.updates);
  const runtimeUpdateState = stringValue(asObject(updatesSnapshot.update), "state");
  const runtimeOperationPhase = stringValue(asObject(updatesSnapshot.operation), "phase");
  const runtimeCleanupPending = booleanValue(
    asObject(updatesSnapshot.operation),
    "cleanup_pending",
  );
  const runtimeGate = runtimeSettingsGate(
    harnessState,
    numberValue(harnessRuntime, "pid"),
    runtimeUpdateState,
    runtimeOperationPhase,
    runtimeCleanupPending,
    busyAction !== null,
  );
  const runtimeGateReason =
    runtimeGate.reason === "harness_not_stopped"
      ? t("Harness must be positively stopped before saving runtime settings.")
      : runtimeGate.reason === "update_active"
        ? t("Wait for the update to become idle before saving runtime settings.")
        : runtimeGate.reason === "cold_active"
          ? t("Wait for the cold switch to finish before saving runtime settings.")
          : runtimeGate.reason === "cleanup_pending"
            ? t("Retry cold cleanup before saving runtime settings.")
            : null;
  const runtime = nestedValue(config, "runtime");
  const savedRuntimeDraft = {
    node: stringValue(nestedValue(runtime, "node"), "path") || "",
    pnpm: stringValue(nestedValue(runtime, "pnpm"), "path") || "",
    git: stringValue(nestedValue(runtime, "git"), "path") || "",
    source: stringValue(runtime, "source") || "official",
  };
  const [runtimeDraft, setRuntimeDraft] = useDraftState("runtime.value", () => ({
    value: savedRuntimeDraft,
    dirty: false,
  }));
  const runtimeRevision = useDraftRevision(snapshot.config, runtimeDraft.dirty, "runtime.revision");
  const [runtimeSaving, setRuntimeSaving] = useState(false);
  const pins = runtimeDraft.value;
  const runtimeSource = runtimeDraft.value.source;
  useEffect(() => {
    setRuntimeDraft((current) => refreshEditableDraft(current, savedRuntimeDraft));
  }, [
    savedRuntimeDraft.node,
    savedRuntimeDraft.pnpm,
    savedRuntimeDraft.git,
    savedRuntimeDraft.source,
    runtimeDraft.dirty,
  ]);
  const changeRuntime = (name: keyof typeof savedRuntimeDraft, value: string) => {
    setRuntimeDraft((current) => ({ value: { ...current.value, [name]: value }, dirty: true }));
  };
  const saveRuntime = async () => {
    if (runtimeGate.disabled || runtimeSaving) return;
    setRuntimeSaving(true);
    try {
      const saved = await runAction(t("Save runtime settings"), "/v1/config", {
        action: "set_runtime",
        expected_revision: runtimeRevision,
        runtime: {
          node: pins.node.trim() ? { path: pins.node.trim(), ownership: "system" } : null,
          pnpm: pins.pnpm.trim() ? { path: pins.pnpm.trim(), ownership: "system" } : null,
          git: pins.git.trim() ? { path: pins.git.trim(), ownership: "system" } : null,
          source: runtimeSource,
          mode: stringValue(runtime, "mode") || "portable",
        },
      });
      setRuntimeDraft((current) => finishDraftSave(current, saved === true));
    } finally {
      setRuntimeSaving(false);
    }
  };
  return (
    <>
      <PageIntro
        kicker={t("System / Settings")}
        title={t("Settings")}
        detail={t(
          "Configuration remains Agent-owned. This view intentionally exposes metadata, not credentials or raw environment values.",
        )}
      />
      <nav className="section-nav" aria-label={t("Settings sections")}>
        {(
          [
            ["display", "Appearance and display"],
            ["harness", "Harness configuration"],
            ["runtime", "Runtime and launch"],
            ["repair", "Repair & reset"],
            ["application", "Application and about"],
          ] as const
        ).map(([id, label]) => (
          <a
            key={id}
            className="button"
            href={`#settings-${id}`}
            onClick={(event) => {
              event.preventDefault();
              scrollSection(id);
            }}
          >
            {t(label)}
          </a>
        ))}
      </nav>
      <section className="settings-section settings-group" id="settings-display">
        <Panel title={t("Appearance")} icon={<Gear size={18} />}>
          <label className="field-label" htmlFor="theme-mode">
            {t("Theme")}
          </label>
          <select
            id="theme-mode"
            className="theme-select"
            value={themeMode}
            onChange={(event) => setThemeMode(event.target.value as ThemeMode)}
          >
            <option value="system">{t("System")}</option>
            <option value="light">{t("Light")}</option>
            <option value="dark">{t("Dark")}</option>
          </select>
          <p className="field-help">
            {t("System follows the operating system preference. Your choice is saved locally.")}
          </p>
          <label className="field-label" htmlFor="locale-mode">
            {t("Language")}
          </label>
          <select
            id="locale-mode"
            className="theme-select"
            value={locale}
            onChange={(event) => setLocale(event.target.value as Locale)}
          >
            <option value="en">{t("English")}</option>
            <option value="zh">{t("Chinese")}</option>
          </select>
          <p className="field-help">{t("Choose the language used by the Launcher interface.")}</p>
        </Panel>

        <Panel title={t("Display and window behavior")} icon={<MonitorPlay size={18} />}>
          <label className="form-field">
            {t("Page zoom")}
            <select value={zoom} onChange={(event) => setDisplayZoom(Number(event.target.value))}>
              {ZOOM_LEVELS.map((value) => (
                <option key={value} value={value}>
                  {value}%
                </option>
              ))}
            </select>
          </label>
          <p>
            {t(
              "Use Ctrl + / Ctrl - to zoom and Ctrl 0 to reset. Returning to this window refreshes service status.",
            )}
          </p>
          <p>
            {t(
              "Closing the window keeps Nexus in the tray. The tray menu lets you exit the launcher while keeping services running, or stop services and exit.",
            )}
          </p>
        </Panel>
      </section>
      <section className="settings-section settings-group" id="settings-harness">
        <HarnessSourcePanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
        <HarnessPreferencesPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction}>
          <details className="advanced-settings">
            <summary>{t("Advanced startup parameters")}</summary>{" "}
            <p className="panel-description">
              {t("Configure the external Harness here. Editing config.json is only a fallback.")}
            </p>
            {harnessEnvOverride && (
              <p className="field-help" role="status">
                {t(
                  "Environment variables override part of this Harness configuration. Saved file values remain in place, but the override wins at launch time.",
                )}
              </p>
            )}
            {!editingHarness && hasHarnessConfig ? (
              <>
                <dl className="detail-list">
                  <div>
                    <dt>{t("Node executable")}</dt>
                    <dd>{stringValue(harness, "program") || t("Not configured")}</dd>
                  </div>
                  <div>
                    <dt>{t("Harness entry")}</dt>
                    <dd>{configuredEntry}</dd>
                  </div>
                  <div>
                    <dt>{t("Readiness URL")}</dt>
                    <dd>
                      {isLoopbackReadinessTarget(stringValue(harness, "readiness_url"))
                        ? stringValue(harness, "readiness_url")
                        : t("Not shown")}
                    </dd>
                  </div>
                </dl>
                <div className="form-actions">
                  <button
                    type="button"
                    className="button"
                    disabled={configControlsDisabled}
                    onClick={openEditor}
                  >
                    {t("Edit configuration")}
                  </button>
                  <button
                    type="button"
                    className="button danger"
                    disabled={configControlsDisabled}
                    onClick={() => void clearHarness()}
                  >
                    {t("Clear configuration")}
                  </button>
                </div>
              </>
            ) : (
              <form className="config-form" onSubmit={(event) => void saveHarness(event)}>
                <p className="field-help">
                  {t(
                    "Harness always runs in Node mode from the active release slot. The executable, entry, and profile wiring are managed by Nexus.",
                  )}
                </p>
                <div className="form-grid">
                  <label className="form-field">
                    <span className="field-label">
                      {t("Readiness timeout (seconds)")} <em>{t("Optional")}</em>
                    </span>
                    <input
                      className="form-input"
                      inputMode="numeric"
                      value={draft.timeout}
                      onChange={(event) => updateDraft("timeout", event.target.value)}
                      placeholder={t("Agent default")}
                      disabled={configControlsDisabled}
                    />
                  </label>
                  <label className="form-field full">
                    <span className="field-label">
                      {t("Readiness URL")} <em>{t("Optional")}</em>
                    </span>
                    <input
                      className="form-input"
                      type="text"
                      value={draft.readinessUrl}
                      onChange={(event) => updateDraft("readinessUrl", event.target.value)}
                      placeholder={t("Readiness URL example")}
                      disabled={configControlsDisabled}
                    />
                    <span className="field-help">
                      {t(
                        "Use an HTTP loopback URL for a 2xx check, or tcp://127.0.0.1:PORT when the Harness protects its page with authentication.",
                      )}
                    </span>
                  </label>
                  <label className="form-check full">
                    <input
                      type="checkbox"
                      checked={draft.readinessTokenRequired}
                      onChange={(event) =>
                        updateDraft("readinessTokenRequired", event.target.checked)
                      }
                      disabled={configControlsDisabled || !draft.readinessUrl.trim()}
                    />
                    <span>{t("Require a fresh Harness token before accepting readiness")}</span>
                  </label>
                </div>
                <div className="form-field full">
                  <span className="field-label">{t("Additional arguments")}</span>
                  <p className="field-help">
                    {t(
                      "The profile argument always follows the active profile and is added automatically.",
                    )}
                  </p>
                  <HarnessArgumentReference snapshot={snapshot} />
                  {(!draft.argsRedacted || draft.replaceRedactedArgs) && (
                    <>
                      {argRows.map((row, index) => (
                        <div className="kv-row" key={index}>
                          <input
                            className="form-input"
                            list="harness-argument-options"
                            autoComplete="off"
                            value={row.key}
                            placeholder="--flag"
                            disabled={configControlsDisabled}
                            onChange={(event) =>
                              setArgRows((current) =>
                                current.map((item, i) =>
                                  i === index ? { ...item, key: event.target.value } : item,
                                ),
                              )
                            }
                          />
                          <input
                            className="form-input"
                            value={row.value}
                            placeholder={t("Value (optional)")}
                            disabled={configControlsDisabled}
                            onChange={(event) =>
                              setArgRows((current) =>
                                current.map((item, i) =>
                                  i === index ? { ...item, value: event.target.value } : item,
                                ),
                              )
                            }
                          />
                          <ActionButton
                            tone="danger"
                            disabled={configControlsDisabled}
                            onClick={() =>
                              setArgRows((current) => current.filter((_, i) => i !== index))
                            }
                          >
                            {t("Remove")}
                          </ActionButton>
                        </div>
                      ))}
                    </>
                  )}
                  {draft.argsRedacted && !draft.replaceRedactedArgs && (
                    <p className="form-error" role="alert">
                      <WarningCircle size={15} />
                      {t(
                        "Existing sensitive arguments are hidden. Enable replacement before saving.",
                      )}
                    </p>
                  )}
                  {draft.argsRedacted && draft.replaceRedactedArgs && (
                    <p className="field-help">
                      {t(
                        "Re-enter the complete argument list. Previous arguments that are not entered again will be removed.",
                      )}
                    </p>
                  )}
                  {draft.argsRedacted && (
                    <label className="form-check">
                      <input
                        type="checkbox"
                        checked={draft.replaceRedactedArgs}
                        onChange={(event) => {
                          updateDraft("replaceRedactedArgs", event.target.checked);
                          setArgRows((current) =>
                            replacementArgumentRows(current, event.target.checked),
                          );
                        }}
                        disabled={configControlsDisabled}
                      />
                      <span>{t("Replace hidden arguments")}</span>
                    </label>
                  )}
                  {(!draft.argsRedacted || draft.replaceRedactedArgs) && (
                    <div className="button-row">
                      <ActionButton
                        disabled={configControlsDisabled}
                        onClick={() =>
                          setArgRows((current) => [...current, { key: "--", value: "" }])
                        }
                      >
                        {t("Add argument")}
                      </ActionButton>
                    </div>
                  )}
                </div>
                {formError && (
                  <div className="form-error" role="alert">
                    <WarningCircle size={16} />
                    {formError}
                  </div>
                )}
                {harnessState === "running" && (
                  <p className="field-help" role="status">
                    {t("Stop Harness before changing its launch configuration.")}
                  </p>
                )}
                <div className="form-actions">
                  <button
                    type="submit"
                    className="button primary"
                    disabled={configControlsDisabled}
                  >
                    {t("Save startup parameters")}
                  </button>
                  {hasHarnessConfig && (
                    <button
                      type="button"
                      className="button"
                      disabled={configControlsDisabled}
                      onClick={() => {
                        draftDirtyRef.current = false;
                        setDraftDirty(false);
                        setFormError(null);
                        setEditingHarness(false);
                      }}
                    >
                      {t("Cancel")}
                    </button>
                  )}
                  {hasHarnessConfig && (
                    <button
                      type="button"
                      className="button danger"
                      disabled={configControlsDisabled}
                      onClick={() => void clearHarness()}
                    >
                      {t("Clear configuration")}
                    </button>
                  )}
                </div>
              </form>
            )}
            {!hasHarnessConfig && !editingHarness && (
              <EmptyState
                title={t("Harness is not configured")}
                detail={t(
                  "The Agent remains usable as a control plane until an external Harness is configured.",
                )}
              />
            )}
          </details>
        </HarnessPreferencesPanel>
      </section>
      <section className="settings-section settings-group" id="settings-runtime">
        <LaunchInputsPanel snapshot={snapshot} />
        <Panel title={t("Runtime settings")} icon={<Cpu size={18} />}>
          <p className="field-help">
            {t(
              "Runtime settings apply to the next Harness launch and dependency operation. Restore previous configuration can undo the last saved configuration.",
            )}
          </p>
          <div className="status-block">
            <div className="form-grid">
              {(["node", "pnpm", "git"] as const).map((name) => (
                <label key={name} className="form-field">
                  <span className="field-label">
                    {name} {t("pin")}
                  </span>
                  <PathInput
                    value={pins[name]}
                    disabled={runtimeSaving || runtimeGate.disabled}
                    placeholder={t("Use bundled runtime")}
                    onChange={(value) => changeRuntime(name, value)}
                  />
                </label>
              ))}
            </div>
            <p className="field-help">
              {t(
                "Explicit paths take priority. Leave blank to use the complete bundled Node/npm/pnpm combination; system discovery is used only when no bundle is present.",
              )}
            </p>
            <ActionButton
              disabled={runtimeGate.disabled || runtimeSaving}
              onClick={() => void saveRuntime()}
            >
              {t("Save runtime settings")}
            </ActionButton>
            {runtimeDraft.dirty && (
              <ActionButton
                disabled={runtimeSaving}
                onClick={() => setRuntimeDraft({ value: savedRuntimeDraft, dirty: false })}
              >
                {t("Cancel")}
              </ActionButton>
            )}
            {runtimeGateReason && (
              <p className="field-help" role="status">
                {runtimeGateReason}
              </p>
            )}
          </div>
          <hr className="panel-divider" />
          <RuntimeStatusPanel
            agentAvailable={snapshot.startup?.available === true}
            state={runtimeStatus}
            onCheck={() => void checkRuntime()}
          />
        </Panel>
      </section>
      <section className="settings-section settings-group" id="settings-repair">
        <Panel title={t("Repair & reset")} icon={<Gear size={18} />}>
          <p className="field-help">
            {t(
              "Reset repairs broken Nexus state. Harness data under .dsh is never touched; installed version slots stay on disk.",
            )}
          </p>
          <div className="button-row">
            <ActionButton
              disabled={
                runtimeGate.disabled ||
                snapshot.startup?.available !== true ||
                draftDirty ||
                runtimeDraft.dirty
              }
              onClick={() => {
                if (
                  window.confirm(
                    t("Restore the previous valid Nexus configuration? Harness will stay stopped."),
                  )
                )
                  void runAction(t("Restore previous configuration"), "/v1/maintenance", {
                    action: "restore_previous",
                    scope: "config",
                    expected_revision: stringValue(config, "revision") || "",
                  });
              }}
            >
              {t("Restore previous configuration")}
            </ActionButton>
            <ActionButton
              tone={armedReset === "config" ? "danger" : undefined}
              disabled={busyAction !== null}
              onClick={() => {
                const scope = "config";
                if (armedReset === scope) {
                  setArmedReset(null);
                  void runAction(t("Reset Nexus configuration"), "/v1/maintenance", {
                    action: "reset",
                    scope,
                    expected_revision: resetRevision.current,
                  });
                } else {
                  resetRevision.current = stringValue(config, "revision") || "";
                  setArmedReset(scope);
                }
              }}
            >
              {armedReset === "config"
                ? t("Click again to confirm")
                : t("Reset Nexus configuration")}
            </ActionButton>
            <ActionButton
              tone={armedReset === "slots" ? "danger" : undefined}
              disabled={busyAction !== null}
              onClick={() => {
                const scope = "slots";
                if (armedReset === scope) {
                  setArmedReset(null);
                  void runAction(t("Reset configuration and slot registry"), "/v1/maintenance", {
                    action: "reset",
                    scope,
                    expected_revision: resetRevision.current,
                  });
                } else {
                  resetRevision.current = stringValue(config, "revision") || "";
                  setArmedReset(scope);
                }
              }}
            >
              {armedReset === "slots"
                ? t("Click again to confirm")
                : t("Reset configuration and slot registry")}
            </ActionButton>
          </div>
          <p className="field-help">
            {t(
              "Restores the previous valid Nexus settings without starting Harness or moving data. It cannot repair an unreadable configuration whose data paths cannot be verified.",
            )}
          </p>
          <p className="field-help">
            {t(
              "Reset backups contain original private configuration. Keep them local; use diagnostic export for a redacted bundle to share.",
            )}
          </p>
          {armedReset && (
            <p className="form-error" role="alert">
              {t("Click the same button again to run the reset. Harness must be stopped.")}
            </p>
          )}
        </Panel>
      </section>
      <section className="settings-section settings-group" id="settings-application">
        <Panel title={t("Release identity")} icon={<Info size={18} />}>
          <dl className="detail-list">
            <div>
              <dt>{t("Version")}</dt>
              <dd>{stringValue(buildIdentity, "version") || t("Not available")}</dd>
            </div>
            <div>
              <dt>{t("Build")}</dt>
              <dd>
                <code>{stringValue(buildIdentity, "buildId") || t("Not available")}</code>
              </dd>
            </div>
            <div>
              <dt>{t("Bundled runtime")}</dt>
              <dd>
                Node {stringValue(buildIdentity, "node") || "—"} / npm{" "}
                {stringValue(buildIdentity, "npm") || "—"} / pnpm{" "}
                {stringValue(buildIdentity, "pnpm") || "—"}
              </dd>
            </div>
          </dl>
        </Panel>
        <Panel title={t("Help")} icon={<TerminalWindow size={18} />}>
          <div className="integration-list">
            <div>
              <CheckCircle size={18} />
              <span>{t("Upstream documentation")}</span>
              <a
                href="https://github.com/deepseek-ai/deepseek-harness"
                target="_blank"
                rel="noreferrer"
              >
                github.com/deepseek-ai/deepseek-harness
              </a>
            </div>
            <div>
              <CheckCircle size={18} />
              <span>{t("Diagnostics and logs")}</span>
              <span>
                {t("Runtime logs and diagnostic bundles are collected on the Diagnostics page.")}
              </span>
            </div>
            <div>
              <Gear size={18} />
              <span>{t("Agent log level")}</span>
              <select
                className="form-input"
                value={logLevel}
                onChange={(event) => setLogLevel(event.target.value)}
              >
                <option value="error">{t("Error")}</option>
                <option value="warn">{t("Warning")}</option>
                <option value="info">{t("Information")}</option>
                <option value="debug">{t("Debug")}</option>
                <option value="trace">{t("Trace")}</option>
              </select>
            </div>
          </div>
          <p className="field-help">{t("The log level applies the next time the Agent starts.")}</p>
          <details>
            <summary>{t("Harness fails to start")}</summary>
            <p className="field-help">
              {t(
                "Open the startup log from the Overview or Diagnostics page. Plugin mismatches are expected across versions; use Recovery to remove the affected plugin or restore a healthy snapshot.",
              )}
            </p>
          </details>
          <details>
            <summary>{t("Node, pnpm, or Git is missing")}</summary>
            <p className="field-help">
              {t(
                "Nexus defaults to its complete bundled runtime. Explicit paths in Runtime settings take priority; system discovery is only used without a bundle.",
              )}
            </p>
          </details>
        </Panel>
        <Panel title={t("Native integration")} icon={<Bell size={18} />}>
          <div className="integration-list">
            {desktopUpdate && (
              <>
                <div>
                  <CheckCircle size={18} />
                  <span>{t("Automatic Launcher updates")}</span>
                  <label className="form-check">
                    <input
                      type="checkbox"
                      checked={desktopUpdate.enabled}
                      disabled={desktopUpdate.phase === "installing"}
                      onChange={(event) => {
                        setDesktopUpdateError("");
                        void invoke("update_settings", { enabled: event.target.checked }).catch(
                          (error) => setDesktopUpdateError(error.message || String(error)),
                        );
                      }}
                    />
                    <span>{desktopUpdate.enabled ? t("Enabled") : t("Disabled")}</span>
                  </label>
                </div>
                <div>
                  <CheckCircle size={18} />
                  <span>{t("Launcher updates")}</span>
                  <button
                    className="button secondary small"
                    disabled={["checking", "downloading", "ready", "installing"].includes(
                      desktopUpdate.phase,
                    )}
                    onClick={() => {
                      setDesktopUpdateError("");
                      void invoke("update_check")
                        .then(() => setDesktopUpdateError(t("Update check completed")))
                        .catch((error) => setDesktopUpdateError(error.message || String(error)));
                    }}
                  >
                    {desktopUpdate.phase === "checking"
                      ? t("Checking…")
                      : desktopUpdate.phase === "ready"
                        ? t("Update and restart")
                        : desktopUpdate.phase === "downloading"
                          ? t("Downloading {percent}%", {
                              percent: Math.min(
                                100,
                                Math.max(0, Math.floor(desktopUpdate.percent ?? 0)),
                              ),
                            })
                          : t("Check for updates")}
                  </button>
                </div>
                <p className="field-help">
                  {t(
                    "Background downloads do not interrupt Harness. Stop Harness before applying an update; running tasks will be interrupted. Restart Harness manually after updating.",
                  )}
                </p>
              </>
            )}
            <div>
              <CheckCircle size={18} />
              <span>{t("Single instance guard")}</span>
              <strong>{t("Enabled")}</strong>
            </div>
            <div>
              <Bell size={18} />
              <span>{t("Desktop notifications")}</span>
              <label className="form-check">
                <input
                  type="checkbox"
                  checked={notificationsEnabled}
                  onChange={(event) => {
                    setNotificationsEnabledPreference(event.target.checked);
                    setNotificationsEnabled(event.target.checked);
                  }}
                />
                <span>{t("Enabled")}</span>
              </label>
            </div>
            <div>
              <Key size={18} />
              <span>{t("API transport")}</span>
              <strong>{t("Rust loopback proxy")}</strong>
            </div>
            <div>
              <CheckCircle size={18} />
              <span>{t("Launch on system startup")}</span>
              <label className="form-check">
                <input
                  type="checkbox"
                  checked={autostartEnabled === true}
                  disabled={autostartEnabled === null}
                  onChange={(event) => void toggleAutostart(event.target.checked)}
                />
                <span>
                  {autostartEnabled === null
                    ? t("Unavailable")
                    : autostartEnabled
                      ? t("Enabled")
                      : t("Disabled")}
                </span>
              </label>
            </div>
          </div>
          {desktopUpdateError && <p role="alert">{desktopUpdateError}</p>}
        </Panel>
      </section>
    </>
  );
}

function argsToRows(argsText: string): Array<{ key: string; value: string }> {
  const tokens = argsText
    .split(/\r?\n/)
    .map((value) => value.trim())
    .filter(Boolean);
  const rows: Array<{ key: string; value: string }> = [];
  for (let index = 0; index < tokens.length; index += 1) {
    if (
      tokens[index].startsWith("--") &&
      index + 1 < tokens.length &&
      !tokens[index + 1].startsWith("--")
    ) {
      rows.push({ key: tokens[index], value: tokens[index + 1] });
      index += 1;
    } else {
      rows.push({ key: tokens[index], value: "" });
    }
  }
  return rows;
}

function rowsToArgsText(rows: Array<{ key: string; value: string }>): string {
  const out: string[] = [];
  for (const row of rows) {
    const key = row.key.trim();
    if (!key) continue;
    out.push(key);
    const value = row.value.trim();
    if (value) out.push(value);
  }
  return out.join("\n");
}
