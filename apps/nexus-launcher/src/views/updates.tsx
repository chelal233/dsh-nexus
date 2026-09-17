import { ActionButton, Panel, Modal, PathInput, PageIntro, EmptyState } from "../ui-components";
import { useI18n } from "../i18n";
import { useState, useEffect, useId, useRef, useCallback } from "react";
import { type HarnessPanelProps, type JsonObject, type ViewProps } from "../app-types";
import { proxyRequest } from "../agent-bridge";
import {
  nestedValue,
  stringValue,
  arrayValue,
  booleanValue,
  numberValue,
  asObject,
} from "../json-values";
import { errorMessage, localizedRuntimeState, formatTimestamp } from "../display-format";
import { useDraftState } from "../draft-memory";
import {
  offlineImportDefaults,
  refreshEditableDraft,
  offlineArchivePathValid,
  offlinePackageCommand,
  releasePromotionCommand,
  updateSourcePayload,
  finishDraftSave,
} from "../settings-state";
import {
  createLatestRequest,
  coldOperationIsTerminal,
  externalHarnessRoot,
} from "../control-state";
import { Package, WarningCircle, CloudArrowUp } from "@phosphor-icons/react";
import { operationRetryCommand, releaseCatalogIsCurrent } from "../operation-status";
import { useDraftRevision } from "./settings";

export function ArchivePath({ path, exported }: { path: string; exported: boolean }) {
  const { t } = useI18n();
  const [copyState, setCopyState] = useState("");
  useEffect(() => setCopyState(""), [path]);
  return (
    <div className="status-block">
      <label className="form-field">
        <span className="field-label">
          {t(exported ? "Exported package path" : "Archive path")}
        </span>
        <input
          className="form-input"
          readOnly
          value={path}
          onFocus={(event) => event.target.select()}
        />
      </label>
      <ActionButton
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(path);
            setCopyState("Path copied");
          } catch {
            setCopyState("Select the path and copy it manually.");
          }
        }}
      >
        {t("Copy path")}
      </ActionButton>
      {copyState && <small role="status">{t(copyState)}</small>}
    </div>
  );
}

export function OfflinePackagePanel({
  snapshot,
  busyAction,
  runAction,
  actionPending,
}: HarnessPanelProps) {
  const { t } = useI18n();
  const tabId = useId();
  const [transferMode, setTransferMode] = useDraftState<"import" | "export">(
    "offline.direction",
    "import",
  );
  const [importPath, setImportPath] = useDraftState("offline.import", "");
  const [exportPath, setExportPath] = useDraftState("offline.export", "");
  const [contents, setContents] = useDraftState("offline.contents.v3", () => ({
    runtime: true,
    profiles: [] as string[],
    configuration: true,
    environment: false,
    sessions: false,
    plugins: false,
    credentials: false,
  }));
  const [preview, setPreview] = useState<{ path: string; value: JsonObject } | null>(null);
  const [importContents, setImportContents] = useState(() => offlineImportDefaults(null));
  const [previewBusy, setPreviewBusy] = useState(false),
    [previewError, setPreviewError] = useState("");
  const previewRequest = useRef(createLatestRequest());
  useEffect(() => {
    previewRequest.current.cancel();
    setPreview(null);
    setPreviewBusy(false);
    setPreviewError("");
  }, [importPath]);
  useEffect(() => () => previewRequest.current.cancel(), []);
  const inspect = async () => {
    const token = previewRequest.current.begin();
    setPreviewBusy(true);
    setPreviewError("");
    try {
      const value = await proxyRequest<JsonObject>("/v1/updates", "POST", {
        action: "offline_inspect",
        archive_path: importPath.trim(),
      });
      if (previewRequest.current.isCurrent(token)) {
        setPreview({ path: importPath, value });
        const data = nestedValue(value, "contents");
        setImportContents(offlineImportDefaults(data));
      }
    } catch (cause) {
      if (previewRequest.current.isCurrent(token)) {
        setPreview(null);
        setPreviewError(errorMessage(cause));
      }
    } finally {
      if (previewRequest.current.isCurrent(token)) setPreviewBusy(false);
    }
  };
  const previewHasRuntime =
    preview !== null && nestedValue(preview.value, "contents")?.runtime !== false;
  const current = stringValue(snapshot.releases, "current_release") || "";
  const [releaseDraft, setReleaseDraft] = useDraftState("offline.release", () => ({
    value: current,
    dirty: false,
  }));
  useEffect(
    () => setReleaseDraft((draft) => refreshEditableDraft(draft, current)),
    [current, releaseDraft.dirty],
  );
  const releases = arrayValue(snapshot.releases, "releases");
  const operation = nestedValue(snapshot.updates, "operation"),
    install = nestedValue(snapshot.updates, "install_operation");
  const phase = stringValue(operation, "phase");
  const locked =
    busyAction !== null ||
    snapshot.startup?.available !== true ||
    !!snapshot.lifecycleBusy ||
    (!!phase && !coldOperationIsTerminal(phase)) ||
    booleanValue(operation, "cleanup_pending") ||
    stringValue(install, "phase") === "installing" ||
    booleanValue(install, "cleanup_pending");
  const exportSelected = releases.some((item) => stringValue(item, "id") === releaseDraft.value);
  const profiles = arrayValue(snapshot.profiles, "manifests");
  const offline = stringValue(operation, "kind")?.startsWith("offline_") === true;
  const active = offline && !!phase && !coldOperationIsTerminal(phase);
  const [progressOpen, setProgressOpen] = useState(active);
  useEffect(() => {
    if (active) setProgressOpen(true);
  }, [active, stringValue(operation, "operation_id")]);
  const launch = async (command: JsonObject, title: string) => {
    setProgressOpen(true);
    if (!(await runAction(title, "/v1/updates", command))) setProgressOpen(false);
  };
  return (
    <section id="offline-packages">
      <Panel title={t("Offline packages")} icon={<Package size={18} />}>
        {progressOpen && (
          <Modal
            title={t("Package transfer")}
            locked={active || busyAction !== null}
            onClose={() => setProgressOpen(false)}
          >
            {busyAction !== null && !active ? (
              <p role="status">{t("Preparing package")}</p>
            ) : (
              <OfflineOperationStatus
                snapshot={snapshot}
                busyAction={busyAction}
                actionPending={actionPending}
                runAction={runAction}
              />
            )}
            {!active && busyAction === null && (
              <ActionButton tone="primary" onClick={() => setProgressOpen(false)}>
                {t("Done")}
              </ActionButton>
            )}
          </Modal>
        )}
        <div className="transfer-tabs" role="tablist" aria-label={t("Package transfer")}>
          {(["import", "export"] as const).map((mode) => (
            <button
              key={mode}
              type="button"
              role="tab"
              id={`${tabId}-${mode}-tab`}
              aria-controls={`${tabId}-${mode}-panel`}
              aria-selected={transferMode === mode}
              tabIndex={transferMode === mode ? 0 : -1}
              disabled={active || busyAction !== null}
              onClick={() => setTransferMode(mode)}
              onKeyDown={(event) => {
                if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
                event.preventDefault();
                const next =
                  event.key === "Home"
                    ? "import"
                    : event.key === "End"
                      ? "export"
                      : mode === "import"
                        ? "export"
                        : "import";
                setTransferMode(next);
                document.getElementById(`${tabId}-${next}-tab`)?.focus();
              }}
            >
              {t(mode === "import" ? "Import" : "Export")}
            </button>
          ))}
        </div>
        <div
          className="transfer-pane"
          role="tabpanel"
          id={`${tabId}-import-panel`}
          aria-labelledby={`${tabId}-import-tab`}
          tabIndex={0}
          hidden={transferMode !== "import"}
        >
          <p className="field-help">
            {t(
              "Choose a package, read its contents, then select what to import. No dependency downloads or builds are needed.",
            )}
          </p>
          <div>
            <label className="form-field">
              <span className="field-label">{t("Package to import (full .tar.gz path)")}</span>
              <PathInput
                value={importPath}
                placeholder="D:\Offline\harness.tar.gz"
                disabled={locked}
                archive
                onChange={setImportPath}
              />
            </label>
            <p className="field-help">
              {t(
                "Integrity checks detect damaged packages; they do not authenticate the publisher. Only import packages from sources you trust.",
              )}
            </p>
            <div className="button-row">
              <ActionButton
                disabled={locked || previewBusy || !offlineArchivePathValid(importPath)}
                onClick={() => void inspect()}
              >
                {t(previewBusy ? "Reading package contents" : "Read package contents")}
              </ActionButton>
            </div>
            {previewError && (
              <p className="form-error" role="alert">
                {previewError}
              </p>
            )}
          </div>
          {preview?.path === importPath && (
            <fieldset disabled={locked || previewBusy} className="offline-contents">
              <legend>{t("Choose contents to import")}</legend>
              {previewHasRuntime && (
                <div className="transfer-content-group">
                  <label>
                    <input
                      type="checkbox"
                      checked={importContents.runtime}
                      onChange={(event) =>
                        setImportContents((value) => ({ ...value, runtime: event.target.checked }))
                      }
                    />
                    {t("Program and runtime")} · {stringValue(preview.value, "version")}
                  </label>
                </div>
              )}
              <div className="transfer-content-group">
                <span className="field-label">{t("Profiles")}</span>
                <div className="profile-options">
                  {arrayValue(nestedValue(preview.value, "contents"), "profiles")
                    .map(String)
                    .map((name) => (
                      <label key={name}>
                        <input
                          type="checkbox"
                          checked={importContents.profiles.includes(name)}
                          onChange={(event) =>
                            setImportContents((value) => ({
                              ...value,
                              profiles: event.target.checked
                                ? [...value.profiles, name]
                                : value.profiles.filter((item) => item !== name),
                            }))
                          }
                        />
                        {name}
                      </label>
                    ))}
                </div>
                <label>
                  <input
                    type="checkbox"
                    disabled={
                      !booleanValue(nestedValue(preview.value, "contents"), "plugins") ||
                      !importContents.profiles.length
                    }
                    checked={importContents.plugins && !!importContents.profiles.length}
                    onChange={(event) =>
                      setImportContents((value) => ({ ...value, plugins: event.target.checked }))
                    }
                  />
                  {t("Installed plugins and complete dependencies")}
                </label>
                <p className="field-help">{t("Plugins belong to the selected profiles.")}</p>
              </div>
              <div className="transfer-content-group transfer-options">
                <label>
                  <input
                    type="checkbox"
                    disabled={
                      !(
                        nestedValue(preview.value, "contents")?.environment ??
                        booleanValue(nestedValue(preview.value, "contents"), "configuration")
                      )
                    }
                    checked={importContents.environment}
                    onChange={(event) =>
                      setImportContents((value) => ({
                        ...value,
                        environment: event.target.checked,
                      }))
                    }
                  />
                  {t("Shared environment settings")}
                </label>
                <label>
                  <input
                    type="checkbox"
                    disabled={!booleanValue(nestedValue(preview.value, "contents"), "sessions")}
                    checked={importContents.sessions}
                    onChange={(event) =>
                      setImportContents((value) => ({ ...value, sessions: event.target.checked }))
                    }
                  />
                  {t("Session history and attachments")}
                </label>
                <label>
                  <input
                    type="checkbox"
                    disabled={!booleanValue(nestedValue(preview.value, "contents"), "credentials")}
                    checked={importContents.credentials}
                    onChange={(event) =>
                      setImportContents((value) => ({
                        ...value,
                        credentials: event.target.checked,
                        credential_policy: "preserve",
                      }))
                    }
                  />
                  {t("Account credentials and .env")}
                </label>
                {importContents.credentials && (
                  <div className="form-field">
                    <p className="form-error" role="alert">
                      {t(
                        "This archive is not encrypted. Replacing credentials changes the accounts used by this environment. Original files remain in the previous data directory; a recovery record identifies them.",
                      )}
                    </p>
                    <label>
                      {t("Credential conflicts")}
                      <select
                        className="form-select"
                        value={importContents.credential_policy}
                        onChange={(event) =>
                          setImportContents((value) => ({
                            ...value,
                            credential_policy: event.target.value as "preserve" | "replace",
                          }))
                        }
                      >
                        <option value="preserve">{t("Keep existing local credentials")}</option>
                        <option value="replace">{t("Replace with package credentials")}</option>
                      </select>
                    </label>
                  </div>
                )}
              </div>
              {importContents.sessions && (
                <p className="field-help">
                  {t(
                    "Session messages and associated storage are copied unchanged and may contain private content. Project files are not included.",
                  )}
                </p>
              )}
              <p className="field-help">
                {t(
                  "This is the package manifest. Every file is verified during import before activation.",
                )}
              </p>
            </fieldset>
          )}
          <div className="transfer-actions">
            <p className="field-help">
              {t(
                preview?.path === importPath && !previewHasRuntime
                  ? "Only selected data is imported. Program and runtime are unchanged. Harness stays stopped."
                  : "Only selected contents are imported. The current version changes only when program and runtime are selected. Harness stays stopped.",
              )}
            </p>
            <ActionButton
              tone="primary"
              disabled={
                locked ||
                previewBusy ||
                preview?.path !== importPath ||
                (!importContents.runtime &&
                  !importContents.profiles.length &&
                  !importContents.environment &&
                  !importContents.sessions &&
                  !importContents.credentials) ||
                !offlineArchivePathValid(importPath)
              }
              onClick={() =>
                void launch(
                  {
                    ...offlinePackageCommand("offline_import", importPath),
                    offline_contents: {
                      ...importContents,
                      configuration:
                        importContents.configuration && !!importContents.profiles.length,
                      plugins: importContents.plugins && !!importContents.profiles.length,
                    },
                  },
                  t("Offline package import"),
                )
              }
            >
              {t("Import package")}
            </ActionButton>
          </div>
        </div>
        <div
          className="transfer-pane"
          role="tabpanel"
          id={`${tabId}-export-panel`}
          aria-labelledby={`${tabId}-export-tab`}
          tabIndex={0}
          hidden={transferMode !== "export"}
        >
          <p className="field-help">
            {t(
              "Select what to export, then choose where to save the package. Program and runtime are optional.",
            )}
          </p>
          <fieldset disabled={locked} className="offline-contents">
            <legend>{t("Export contents")}</legend>
            <div className="transfer-content-group">
              <label>
                <input
                  type="checkbox"
                  checked={contents.runtime}
                  onChange={(event) =>
                    setContents((value) => ({ ...value, runtime: event.target.checked }))
                  }
                />
                {t("Program and runtime")}
              </label>
              {contents.runtime && (
                <label className="form-field">
                  <span className="field-label">{t("Version to export")}</span>
                  <select
                    value={releaseDraft.value}
                    disabled={locked}
                    onChange={(event) =>
                      setReleaseDraft({ value: event.target.value, dirty: true })
                    }
                  >
                    <option value="">{t("Select an installed version")}</option>
                    {releases.map((item) => (
                      <option key={stringValue(item, "id")} value={stringValue(item, "id")}>
                        {stringValue(item, "version") || stringValue(item, "id")}
                      </option>
                    ))}
                  </select>
                </label>
              )}
            </div>
            <div className="transfer-content-group">
              <span className="field-label">{t("Profiles")}</span>
              <div className="profile-options">
                {profiles.map((profile) => {
                  const name = stringValue(profile, "name") || "";
                  return (
                    <label key={name}>
                      <input
                        type="checkbox"
                        checked={contents.profiles.includes(name)}
                        onChange={(event) =>
                          setContents((value) => ({
                            ...value,
                            profiles: event.target.checked
                              ? [...value.profiles, name]
                              : value.profiles.filter((item) => item !== name),
                          }))
                        }
                      />
                      {name}
                    </label>
                  );
                })}
              </div>
              <label>
                <input
                  type="checkbox"
                  disabled={!contents.profiles.length}
                  checked={contents.plugins && !!contents.profiles.length}
                  onChange={(event) =>
                    setContents((value) => ({ ...value, plugins: event.target.checked }))
                  }
                />
                {t("Installed plugins and complete dependencies")}
              </label>
              <p className="field-help">{t("Plugins belong to the selected profiles.")}</p>
            </div>
            <div className="transfer-content-group transfer-options">
              <label>
                <input
                  type="checkbox"
                  checked={contents.environment}
                  onChange={(event) =>
                    setContents((value) => ({ ...value, environment: event.target.checked }))
                  }
                />
                {t("Shared environment settings")}
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={contents.sessions}
                  onChange={(event) =>
                    setContents((value) => ({ ...value, sessions: event.target.checked }))
                  }
                />
                {t("Session history and attachments")}
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={contents.credentials}
                  onChange={(event) =>
                    setContents((value) => ({ ...value, credentials: event.target.checked }))
                  }
                />
                {t("Account credentials and .env")}
              </label>
            </div>
            {contents.credentials && (
              <p className="form-error" role="alert">
                {t(
                  "This archive is not encrypted and includes account credentials. Anyone who can read it can use those accounts, including recipients of a shared-folder copy.",
                )}
              </p>
            )}
            {contents.sessions && (
              <p className="field-help">
                {t(
                  "Session messages and associated storage are copied unchanged and may contain private content. Project files are not included.",
                )}
              </p>
            )}
            {!contents.runtime && (
              <p className="field-help">
                {t(
                  "Data-only transfer keeps the target program and runtime. Unselected local data is retained; the previous data directory is preserved.",
                )}
              </p>
            )}
          </fieldset>
          <div>
            <label className="form-field">
              <span className="field-label">{t("Export destination (full .tar.gz path)")}</span>
              <PathInput
                value={exportPath}
                placeholder="D:\Offline\harness-export.tar.gz"
                disabled={locked}
                archive
                save
                onChange={setExportPath}
              />
            </label>
            <p className="field-help">
              {t(
                "Choose a new file outside Nexus-managed data. Existing files are never overwritten. Export does not change the selected version.",
              )}
            </p>
          </div>
          <div className="transfer-actions">
            <ActionButton
              tone="primary"
              disabled={
                locked ||
                (contents.runtime && !exportSelected) ||
                (!contents.runtime &&
                  !contents.profiles.length &&
                  !contents.environment &&
                  !contents.sessions &&
                  !contents.credentials) ||
                !offlineArchivePathValid(exportPath) ||
                contents.profiles.some(
                  (name) => !profiles.some((profile) => stringValue(profile, "name") === name),
                )
              }
              onClick={() =>
                void launch(
                  {
                    ...offlinePackageCommand("offline_export", exportPath, releaseDraft.value),
                    offline_contents: {
                      ...contents,
                      plugins: contents.plugins && !!contents.profiles.length,
                    },
                  },
                  t("Offline package export"),
                )
              }
            >
              {t("Export package")}
            </ActionButton>
          </div>
        </div>
        {offline && (
          <div className="package-last-result">
            <span>
              {t(
                stringValue(operation, "kind") === "offline_export"
                  ? "Offline package export"
                  : "Offline package import",
              )}{" "}
              · {localizedRuntimeState(phase, t)}
            </span>
            <ActionButton onClick={() => setProgressOpen(true)}>{t("View progress")}</ActionButton>
          </div>
        )}
      </Panel>
    </section>
  );
}

export function OfflineOperationStatus({
  snapshot,
  busyAction,
  runAction,
  actionPending,
}: HarnessPanelProps) {
  const { t } = useI18n();
  const operation = nestedValue(snapshot.updates, "operation"),
    progress = nestedValue(snapshot.updates, "offline_progress");
  const id = stringValue(operation, "operation_id"),
    phase = stringValue(operation, "phase");
  if (!id) return <p role="status">{t("Preparing package")}</p>;
  const finished = coldOperationIsTerminal(phase),
    cleanup = booleanValue(operation, "cleanup_pending");
  const exporting = stringValue(operation, "kind") === "offline_export";
  const stage = stringValue(progress, "stage");
  const completed = numberValue(progress, "completed") || 0,
    total = numberValue(progress, "total");
  const elapsed = Math.max(
    0,
    Math.floor((Date.now() - (numberValue(progress, "stage_started_at") || Date.now())) / 1000),
  );
  const stages: Record<string, string> = {
    prepare: "Preparing package",
    measure_slot: "Scanning Harness files",
    measure_runtime: "Scanning runtime files",
    copy_slot: "Copying Harness files",
    copy_runtime: "Copying runtime files",
    restore_links: "Restoring dependency links",
    copy_environment: "Copying profile dependencies",
    normalize: "Preparing portable paths",
    remove_links: "Recording dependency links",
    scan_files: "Listing package files",
    hash_files: "Verifying file hashes",
    compress: "Compressing archive",
    flush_archive: "Saving archive to disk",
    scan_archive: "Reading archive contents",
    extract: "Extracting package",
    publish: "Registering environment",
  };
  const runtime = nestedValue(snapshot.config, "runtime");
  const retry = operationRetryCommand(
    operation,
    stringValue(runtime, "source") || "official",
    stringValue(runtime, "mode") || "portable",
  );
  return (
    <div className="status-block" role="status">
      <strong>
        {t(exporting ? "Offline package export" : "Offline package import")}:{" "}
        {localizedRuntimeState(phase, t)}
      </strong>
      {!finished && (
        <>
          <span>{t(stages[stage || ""] || "Preparing package")}</span>
          <progress
            aria-label={t("Stage progress")}
            max={total || undefined}
            value={total ? Math.min(completed, total) : undefined}
          />
          <span>
            {stringValue(progress, "unit") === "bytes"
              ? t("Written {size} MiB", { size: (completed / 1024 / 1024).toFixed(1) })
              : total
                ? t("{done} / {total} files", { done: completed, total })
                : t("Processed {count} entries", { count: completed })}{" "}
            · {t("Stage elapsed {seconds}s", { seconds: elapsed })}
          </span>
        </>
      )}
      {phase === "succeeded" && (
        <p>
          {t(
            exporting
              ? "The package was exported. The selected version is unchanged."
              : nestedValue(operation, "offline_contents")?.runtime === false
                ? "Data import completed. Your program and runtime are unchanged."
                : "Import selects the verified version as current. Harness stays stopped; run startup checks before starting it.",
          )}
        </p>
      )}
      {phase === "succeeded" &&
        !exporting &&
        stringValue(operation, "warning") ===
          "Some incoming configuration values were not applied to preserve your local credentials. To use the package values, review the credential conflict option before importing again." && (
          <p className="field-help">
            {t(
              "Some incoming configuration values were not applied to preserve your local credentials. To use the package values, review the credential conflict option before importing again.",
            )}
          </p>
        )}
      {stringValue(operation, "error") && (
        <p className="form-error" role="alert">
          {stringValue(operation, "error")}
        </p>
      )}
      {stringValue(operation, "credential_recovery_path") && (
        <details>
          <summary>{t("Credential recovery record")}</summary>
          <p>
            {t(
              "Original credential files remain in the previous data directory. The record lists their locations; stop Harness before restoring them.",
            )}
          </p>
          <code>{stringValue(operation, "credential_recovery_path")}</code>
        </details>
      )}
      {cleanup && (
        <p className="form-error" role="alert">
          {t("Cleanup is incomplete. Retry cleanup before starting another update.")}{" "}
          {stringValue(operation, "cleanup_error")}
        </p>
      )}
      {stringValue(operation, "output_tail") && (
        <details>
          <summary>{t("Operation log and details")}</summary>
          <pre>{stringValue(operation, "output_tail")}</pre>
        </details>
      )}
      {finished && phase === "succeeded" && (
        <progress aria-label={t("Stage progress")} max={100} value={100} />
      )}
      <div className="button-row">
        {(!finished || cleanup) && (
          <ActionButton
            tone="danger"
            disabled={
              (actionPending ?? (busyAction !== null && !snapshot.lifecycleBusy)) ||
              phase === "cancelling"
            }
            onClick={() =>
              void runAction(t("Cancel offline operation"), "/v1/updates", {
                action: "cancel",
                operation_id: id,
              })
            }
          >
            {t(cleanup ? "Retry cleanup" : "Cancel")}
          </ActionButton>
        )}
        {finished && !cleanup && phase !== "succeeded" && retry && (
          <ActionButton
            disabled={busyAction !== null}
            onClick={() => void runAction(t("Retry offline operation"), "/v1/updates", retry)}
          >
            {t("Retry offline operation")}
          </ActionButton>
        )}
      </div>
      <hr className="panel-divider" />
    </div>
  );
}

export function UpdatesView({
  snapshot,
  busyAction,
  runAction,
  actionPending,
  refresh,
  embedded,
  openSettings,
  autoLoadTags = false,
}: ViewProps) {
  const { t, locale } = useI18n();
  const update = nestedValue(snapshot.updates, "update");
  const operation = nestedValue(snapshot.updates, "operation");
  const releases = arrayValue(snapshot.releases, "releases");
  const updateState = stringValue(update, "state");
  const runtime = nestedValue(snapshot.config, "runtime");
  const persistedSource = stringValue(runtime, "source") || "official";
  const currentUpdateSource = stringValue(nestedValue(snapshot.config, "update"), "source") || "";
  const persistedMode = stringValue(runtime, "mode") || "portable";
  const promoteRelease = async (id: string) => {
    try {
      const preview = await proxyRequest<JsonObject>("/v1/releases", "POST", {
        action: "promote",
        id,
        inspect_only: true,
      });
      const confirmation = stringValue(preview, "rollback_confirmation");
      const command = releasePromotionCommand(
        id,
        confirmation || null,
        !confirmation ||
          window.confirm(
            t(
              "There is no verified rollback version. Switch manually to {version} anyway? If it fails, automatic rollback will be unavailable. Harness will stay stopped.",
              { version: id },
            ),
          ),
      );
      if (command)
        await runAction(
          t(
            externalHarnessRoot(snapshot.config)
              ? "Prepare this version slot"
              : "Switch to this version",
          ),
          "/v1/releases",
          command,
        );
    } catch (error) {
      setTagsError(errorMessage(error));
    }
  };
  const [tagList, setTagList] = useState<JsonObject | null>(null);
  const [selectedTag, setSelectedTag] = useState<string>("");
  const [tagsLoading, setTagsLoading] = useState(false);
  const [tagsError, setTagsError] = useState<string | null>(null);
  const [sourceDraft, setSourceDraft] = useDraftState("updates.source", () => ({
    value: currentUpdateSource,
    dirty: false,
  }));
  const sourceRevision = useDraftRevision(
    snapshot.config,
    sourceDraft.dirty,
    "updates.source.revision",
  );
  const [sourceSaving, setSourceSaving] = useState(false);
  const latestTags = useRef(createLatestRequest());
  useEffect(() => () => latestTags.current.cancel(), []);
  useEffect(() => {
    setSourceDraft((current) => refreshEditableDraft(current, currentUpdateSource));
  }, [currentUpdateSource, sourceDraft.dirty]);
  const saveSource = async () => {
    if (sourceSaving || busyAction !== null) return;
    setSourceSaving(true);
    try {
      const saved = await runAction(t("Save update source"), "/v1/config", {
        action: "set_update_source",
        expected_revision: sourceRevision,
        update: updateSourcePayload(sourceDraft.value),
      });
      setSourceDraft((current) => finishDraftSave(current, saved === true));
    } finally {
      setSourceSaving(false);
    }
  };
  useEffect(() => {
    latestTags.current.cancel();
    setTagsLoading(false);
    setTagList(null);
    setSelectedTag("");
    setTagsError(null);
  }, [currentUpdateSource]);
  const tags: string[] = tagList ? arrayValue(tagList, "tags").map((tag) => String(tag)) : [];
  const loadTags = useCallback(async () => {
    const token = latestTags.current.begin();
    setTagsLoading(true);
    setTagsError(null);
    try {
      const value = await proxyRequest<JsonObject>("/v1/releases/tags");
      if (latestTags.current.isCurrent(token)) {
        setTagList(value);
        setSelectedTag("");
      }
    } catch (cause) {
      if (latestTags.current.isCurrent(token)) {
        setTagList(null);
        setSelectedTag("");
        setTagsError(errorMessage(cause));
      }
    } finally {
      if (latestTags.current.isCurrent(token)) setTagsLoading(false);
    }
  }, []);
  const autoLoadedSource = useRef<string | null>(null);
  useEffect(() => {
    if (
      !autoLoadTags ||
      !snapshot.startup?.available ||
      sourceDraft.dirty ||
      autoLoadedSource.current === currentUpdateSource
    )
      return;
    autoLoadedSource.current = currentUpdateSource;
    void loadTags();
  }, [autoLoadTags, snapshot.startup?.available, currentUpdateSource, sourceDraft.dirty, loadTags]);
  const configurationRecovery = asObject(asObject(snapshot.updates).configuration_recovery);
  const configurationRecoveryId = stringValue(configurationRecovery, "operation_id");
  const publicationRecovery = asObject(asObject(snapshot.updates).publication_recovery);
  const publicationId = stringValue(publicationRecovery, "operation_id");
  const installOperation = asObject(asObject(snapshot.updates).install_operation);
  const installId = stringValue(installOperation, "operation_id");
  const installPhase = stringValue(installOperation, "phase");
  const installCleanup = booleanValue(installOperation, "cleanup_pending");
  const operationPhase = stringValue(operation, "phase");
  const operationKind = stringValue(operation, "kind") || "cold_switch";
  const offlineExport = operationKind === "offline_export",
    offlineImport = operationKind === "offline_import";
  const archivePath = stringValue(operation, "archive_path") || "";
  const retryCommand = operationRetryCommand(operation, persistedSource, persistedMode);
  const operationId = stringValue(operation, "operation_id");
  const cleanupPending = booleanValue(operation, "cleanup_pending");
  const publishedId = stringValue(operation, "release_id");
  const slotVisible =
    !!publishedId && releases.some((item) => stringValue(item, "id") === publishedId);
  const catalogCurrent = releaseCatalogIsCurrent(snapshot as unknown as JsonObject);
  const verificationPending = operationPhase === "succeeded" && !offlineExport && !catalogCurrent;
  const unpublishedSuccess =
    operationPhase === "succeeded" && !offlineExport && catalogCurrent && !slotVisible;
  const finished = !!operationId && coldOperationIsTerminal(operationPhase);
  const canDismiss = finished && !cleanupPending;
  const attemptDetails = (
    <>
      {stringValue(operation, "output_tail") && (
        <div className="snapshot-file">
          <strong>
            {t(offlineExport || offlineImport ? "Operation output" : "Install output")}
          </strong>
          <pre>{stringValue(operation, "output_tail")}</pre>
        </div>
      )}
      {stringValue(operation, "warning")?.includes("bundled_pnpm_major_skew") && (
        <p className="field-help">
          <WarningCircle size={15} />
          {t(
            "Using the bundled pnpm: it differs from the release's exact pnpm pin, but the major version matches.",
          )}
        </p>
      )}
      {stringValue(operation, "warning")?.includes("rollback_health_required") && (
        <p className="notice degraded" role="status">
          {t(
            "Version prepared only. Your current selection is unchanged. Select the prepared version in Release slots to review the rollback warning and confirm a manual switch.",
          )}
        </p>
      )}
      {(stringValue(operation, "error") || (!operationId && stringValue(update, "error"))) && (
        <p className="form-error" role={canDismiss ? undefined : "alert"}>
          <WarningCircle size={15} />
          {stringValue(operation, "error") || stringValue(update, "error")}
        </p>
      )}
    </>
  );
  return (
    <>
      <div id="installation-status" />
      {!embedded && (
        <PageIntro
          kicker={t("Releases / Updates")}
          title={t("Updates")}
          detail={t("Cold switches are asynchronous and never start Harness automatically.")}
        />
      )}

      {installId && (
        <Panel title={t("Installation")} icon={<CloudArrowUp size={18} />}>
          <p>
            {t("Current stage")}: {localizedRuntimeState(installPhase, t)}
          </p>
          <p>{stringValue(installOperation, "error")}</p>
          <p>{stringValue(installOperation, "cleanup_error")}</p>
          <div className="button-row">
            <ActionButton onClick={() => void refresh()}>{t("Refresh")}</ActionButton>
            {(installPhase === "installing" || installCleanup) && (
              <ActionButton
                tone="danger"
                onClick={() =>
                  void runAction(t("Cancel"), "/v1/updates", {
                    action: "cancel",
                    operation_id: installId,
                  })
                }
              >
                {installCleanup ? t("Retry cleanup") : t("Cancel")}
              </ActionButton>
            )}
          </div>
        </Panel>
      )}
      {!publicationId && configurationRecoveryId && (
        <Panel title={t("Configuration recovery")} icon={<WarningCircle size={18} />}>
          <p>
            {t(
              "Retry the interrupted settings save, or keep the current valid configuration and previous backup exactly as they are. This does not repair invalid configuration files or start Harness.",
            )}
          </p>
          <div className="button-row">
            <ActionButton
              disabled={busyAction !== null}
              onClick={() =>
                void runAction(t("Retry recovery"), "/v1/updates", {
                  action: "configuration_retry",
                  operation_id: configurationRecoveryId,
                })
              }
            >
              {t("Retry recovery")}
            </ActionButton>
            <ActionButton
              disabled={busyAction !== null || !booleanValue(configurationRecovery, "can_preserve")}
              onClick={() =>
                void runAction(t("Keep current and end recovery"), "/v1/updates", {
                  action: "configuration_abandon",
                  operation_id: configurationRecoveryId,
                })
              }
            >
              {t("Keep current and end recovery")}
            </ActionButton>
          </div>
        </Panel>
      )}
      {publicationId && (
        <Panel title={t("Publication recovery")} icon={<WarningCircle size={18} />}>
          <p>
            {t(
              "Retry interrupted publication, or keep current configuration and every existing version and candidate file. Keeping current ends this recovery without compiling or starting Harness. Retained candidate files are not automatically cleaned.",
            )}
          </p>
          <p className="field-help">{stringValue(publicationRecovery, "reason")}</p>
          <div className="button-row">
            <ActionButton
              disabled={busyAction !== null}
              onClick={() =>
                void runAction(t("Retry recovery"), "/v1/updates", {
                  action: "publication_retry",
                  operation_id: publicationId,
                })
              }
            >
              {t("Retry recovery")}
            </ActionButton>
            <ActionButton
              disabled={busyAction !== null}
              onClick={() =>
                void runAction(t("Keep current and end recovery"), "/v1/updates", {
                  action: "publication_abandon",
                  operation_id: publicationId,
                })
              }
            >
              {t("Keep current and end recovery")}
            </ActionButton>
          </div>
        </Panel>
      )}
      <Panel
        title={t(embedded ? "Choose version and install" : "Upstream tags & cold switch")}
        icon={<CloudArrowUp size={18} />}
      >
        <label className="form-field">
          <span>{t("Advanced source settings")}</span>
          <div className="kv-row">
            <input
              className="form-input"
              value={sourceDraft.value}
              placeholder="https://github.com/deepseek-ai/deepseek-harness"
              disabled={busyAction !== null || sourceSaving}
              onChange={(event) => setSourceDraft({ value: event.target.value, dirty: true })}
            />
            <ActionButton
              disabled={
                busyAction !== null ||
                sourceSaving ||
                !sourceDraft.dirty ||
                !sourceDraft.value.trim()
              }
              onClick={() => void saveSource()}
            >
              {t("Save update source")}
            </ActionButton>
            {sourceDraft.dirty && (
              <ActionButton
                disabled={sourceSaving}
                onClick={() => setSourceDraft({ value: currentUpdateSource, dirty: false })}
              >
                {t("Cancel")}
              </ActionButton>
            )}
          </div>
        </label>
        <div className="status-block">
          <div className="upstream-tag-row">
            <label className="form-field">
              <span>{t("Upstream tags")}</span>
              <select
                className="form-input"
                value={selectedTag}
                disabled={sourceDraft.dirty || tagsLoading || tags.length === 0}
                onChange={(event) => setSelectedTag(event.target.value)}
              >
                <option value="">{t("Select a tag")}</option>
                {tags.map((tag) => (
                  <option key={tag} value={tag}>
                    {tag}
                  </option>
                ))}
              </select>
            </label>
            <ActionButton
              disabled={tagsLoading || sourceDraft.dirty || !snapshot.startup?.available}
              onClick={() => void loadTags()}
            >
              {t(tagsLoading ? "Listing tags" : "List upstream tags")}
            </ActionButton>
          </div>
          {sourceDraft.dirty && <p>{t("Save the upstream address before loading tags.")}</p>}
          {tagsError ? (
            <p className="form-error" role="alert">
              {tagsError}
            </p>
          ) : (
            <p className="field-help" role="status">
              {t(
                tagsLoading
                  ? "Listing tags"
                  : tagList
                    ? tags.length
                      ? "Loaded {count} tags"
                      : "No upstream tags found"
                    : "No tags loaded",
                { count: tags.length },
              )}
            </p>
          )}
          {selectedTag && (
            <ActionButton
              tone="primary"
              disabled={
                busyAction !== null ||
                tagsLoading ||
                sourceDraft.dirty ||
                cleanupPending ||
                (!!operationId && !coldOperationIsTerminal(operationPhase))
              }
              onClick={() =>
                void runAction(t("Switch to tag"), "/v1/updates", {
                  action: "switch",
                  tag: selectedTag,
                  source: persistedSource,
                  mode: persistedMode,
                })
              }
            >
              {t(
                embedded
                  ? "Install Harness"
                  : releases.some((item) => stringValue(item, "version") === selectedTag)
                    ? "Switch to tag"
                    : "Fetch this tag",
              )}
            </ActionButton>
          )}
        </div>
        {!offlineExport &&
          !offlineImport &&
          (!!operationId || updateState === "running" || updateState === "failed") && (
            <>
              <hr className="panel-divider" />
              <details
                className="installation-record"
                key={`${operationId}:${canDismiss}`}
                open={
                  !canDismiss ||
                  operationPhase !== "succeeded" ||
                  verificationPending ||
                  unpublishedSuccess ||
                  undefined
                }
              >
                <summary>
                  {t(finished ? "Last installation" : "Current stage")}:{" "}
                  {localizedRuntimeState(operationPhase || updateState, t)}
                </summary>
                <div className="status-block">
                  <strong>
                    {offlineExport
                      ? t("Offline package export")
                      : offlineImport
                        ? t("Offline package import")
                        : finished
                          ? t("Last installation")
                          : t("Current stage")}
                    :{" "}
                    {verificationPending
                      ? t("Verification pending")
                      : unpublishedSuccess
                        ? t("Verifying installed version")
                        : localizedRuntimeState(operationPhase || updateState, t)}
                  </strong>
                  {finished && (
                    <span>
                      {formatTimestamp(
                        numberValue(operation, "updated_at_unix") ??
                          numberValue(operation, "started_at_unix"),
                        t("Not available"),
                        locale,
                      )}
                    </span>
                  )}
                  {finished && (
                    <p className="field-help">
                      {t(
                        offlineExport || offlineImport
                          ? "This is the saved result of the last offline package operation."
                          : "This is a saved installation record, not a new error from reinstalling Nexus.",
                      )}
                    </p>
                  )}
                  {archivePath && (
                    <ArchivePath
                      path={archivePath}
                      exported={offlineExport && operationPhase === "succeeded"}
                    />
                  )}
                  {offlineExport && operationPhase === "succeeded" && (
                    <p className="field-help">
                      {t(
                        cleanupPending
                          ? "The package was exported. Temporary-file cleanup still needs attention."
                          : "The package was exported. The selected version is unchanged.",
                      )}
                    </p>
                  )}
                  {offlineImport && operationPhase === "succeeded" && (
                    <p className="field-help">
                      {t(
                        "Import selects the verified version as current. Harness stays stopped; run startup checks before starting it.",
                      )}
                    </p>
                  )}
                  {(stringValue(operation, "tag") || stringValue(update, "release_id")) && (
                    <span>
                      {stringValue(operation, "tag") || stringValue(update, "release_id")}
                    </span>
                  )}
                  {unpublishedSuccess && (
                    <p className="form-error" role="alert">
                      {t(
                        "The task reports completion, but its version slot is unavailable. Refresh to verify installation before starting Harness.",
                      )}
                    </p>
                  )}
                  {operationId && !finished && (
                    <progress
                      aria-label={t("Update progress")}
                      max="100"
                      value={numberValue(operation, "progress_percent") || 0}
                    >
                      {numberValue(operation, "progress_percent") || 0}%
                    </progress>
                  )}
                  {canDismiss ? (
                    <details key={operationId}>
                      <summary>
                        {t(
                          offlineExport || offlineImport
                            ? "Operation log and details"
                            : "Installation log and details",
                        )}
                      </summary>
                      {attemptDetails}
                    </details>
                  ) : (
                    attemptDetails
                  )}
                  {stringValue(operation, "cleanup_error") && (
                    <p className="form-error" role="alert">
                      <WarningCircle size={15} />
                      {t("Cleanup error")}: {stringValue(operation, "cleanup_error")}
                    </p>
                  )}
                  {cleanupPending && (
                    <p className="notice degraded">
                      {t("Cleanup is incomplete. Retry cleanup before starting another update.")}
                    </p>
                  )}
                  <div className="button-row">
                    <ActionButton disabled={busyAction !== null} onClick={() => void refresh()}>
                      {t("Refresh")}
                    </ActionButton>
                    {operationId &&
                      (!coldOperationIsTerminal(operationPhase) || cleanupPending) && (
                        <ActionButton
                          tone="danger"
                          disabled={
                            (actionPending ?? (busyAction !== null && !snapshot.lifecycleBusy)) ||
                            operationPhase === "cancelling"
                          }
                          onClick={() =>
                            void runAction(
                              t(
                                offlineExport || offlineImport
                                  ? "Cancel offline operation"
                                  : "Cancel cold switch",
                              ),
                              "/v1/updates",
                              { action: "cancel", operation_id: operationId },
                            )
                          }
                        >
                          {cleanupPending ? t("Retry cleanup") : t("Cancel")}
                        </ActionButton>
                      )}
                    {canDismiss && operationPhase !== "succeeded" && retryCommand && (
                      <ActionButton
                        tone="primary"
                        disabled={busyAction !== null || snapshot.startup?.available !== true}
                        onClick={() =>
                          void runAction(
                            t(
                              offlineExport || offlineImport
                                ? "Retry offline operation"
                                : "Retry installation",
                            ),
                            "/v1/updates",
                            retryCommand,
                          )
                        }
                      >
                        {t(
                          offlineExport || offlineImport
                            ? "Retry offline operation"
                            : "Retry installation",
                        )}
                      </ActionButton>
                    )}
                    {canDismiss && (
                      <ActionButton
                        disabled={busyAction !== null}
                        onClick={() =>
                          void runAction(t("Clear finished record"), "/v1/updates", {
                            action: "clear_finished",
                            operation_id: operationId,
                          })
                        }
                      >
                        {t("Clear finished record")}
                      </ActionButton>
                    )}
                  </div>
                  {canDismiss && (
                    <span className="field-help">
                      {t("Clearing this record keeps installed versions and Harness data.")}
                    </span>
                  )}
                </div>
              </details>
            </>
          )}
      </Panel>
      {!embedded && (
        <Panel title={t("Release slots")} icon={<Package size={18} />}>
          {externalHarnessRoot(snapshot.config) && (
            <div className="notice">
              <p>
                {t("Preparing a version slot does not change the active external program source.")}
              </p>
              <ActionButton onClick={() => openSettings?.()}>
                {t("Choose program source in Settings")}
              </ActionButton>
            </div>
          )}
          {releases.length === 0 ? (
            <EmptyState
              title={t("No release slots")}
              detail={t(
                "A successful cold switch registers and promotes its immutable slot without starting Harness.",
              )}
            />
          ) : (
            <div className="table-scroll">
              <table className="release-slots-table">
                <thead>
                  <tr>
                    <th scope="col">{t("Slot")}</th>
                    <th scope="col">{t("Version")}</th>
                    <th scope="col">{t("Status")}</th>
                  </tr>
                </thead>
                <tbody>
                  {releases.map((item) => {
                    const slotId = stringValue(item, "id") || "";
                    const current = stringValue(snapshot.releases, "current_release");
                    const lkg = stringValue(snapshot.releases, "last_known_good");
                    const protectedSlot = slotId === current || slotId === lkg;
                    return (
                      <tr key={slotId}>
                        <th scope="row">
                          <code>{slotId || t("Release")}</code>
                        </th>
                        <td>
                          {stringValue(item, "version") || t("Unknown version")}
                          {slotId === lkg && <small>{t("Last known good")}</small>}
                        </td>
                        <td>
                          <div className="release-slot-actions">
                            {slotId === current ? (
                              <button type="button" className="button slot-current" disabled>
                                {t(
                                  externalHarnessRoot(snapshot.config)
                                    ? "Prepared slot"
                                    : "Current",
                                )}
                              </button>
                            ) : (
                              <ActionButton
                                disabled={busyAction !== null}
                                onClick={() => void promoteRelease(slotId)}
                              >
                                {t(
                                  externalHarnessRoot(snapshot.config)
                                    ? "Prepare this version slot"
                                    : "Switch to this version",
                                )}
                              </ActionButton>
                            )}
                            {!protectedSlot && (
                              <ActionButton
                                disabled={busyAction !== null}
                                onClick={() =>
                                  void runAction(t("Release slot"), "/v1/releases", {
                                    action: "remove",
                                    id: slotId,
                                  })
                                }
                              >
                                {t("Release slot")}
                              </ActionButton>
                            )}
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </Panel>
      )}
      {!embedded && (
        <OfflinePackagePanel
          snapshot={snapshot}
          busyAction={busyAction}
          actionPending={actionPending}
          runAction={runAction}
        />
      )}
    </>
  );
}
