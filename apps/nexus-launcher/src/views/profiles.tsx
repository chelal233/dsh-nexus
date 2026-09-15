import { type ViewProps, type JsonObject } from "../app-types";
import { stringValue, arrayValue, asObject, booleanValue, numberValue } from "../json-values";
import { proxyRequest } from "../agent-bridge";
import {
  errorMessage,
  formatTimestamp,
  localizedRuntimeState,
  snapshotContentNote,
} from "../display-format";
import {
  PageIntro,
  Panel,
  ActionButton,
  DataList,
  StatusPill,
  LoadingState,
  ErrorState,
  EmptyState,
} from "../ui-components";
import { RestoreStatusPanel } from "../operation-notices";
import { useI18n } from "../i18n";
import { useState, useRef, useCallback, useEffect } from "react";
import {
  createLatestRequest,
  recoveryMutationGate,
  pluginMoveTarget,
  FIXED_PROFILE_PLUGINS,
  pluginIsolationChoice,
} from "../control-state";
import {
  SlidersHorizontal,
  WarningCircle,
  Package,
  ListChecks,
  CheckCircle,
  ClipboardText,
} from "@phosphor-icons/react";

export function ProfilesView(props: ViewProps) {
  const { t, locale } = useI18n();
  const { snapshot, busyAction, runAction } = props;
  const active = stringValue(snapshot.profiles, "active_profile");
  const [expandedProfiles, setExpandedProfiles] = useState<string[]>([]);
  const [newProfileName, setNewProfileName] = useState("");
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [deletedProfiles, setDeletedProfiles] = useState<JsonObject[]>([]);
  const [archiveError, setArchiveError] = useState("");
  const archiveRequest = useRef(createLatestRequest());
  const reloadDeleted = useCallback(async () => {
    const token = archiveRequest.current.begin();
    try {
      const result = await proxyRequest<JsonObject>("/v1/profiles", "POST", {
        action: "deleted_list",
      });
      if (archiveRequest.current.isCurrent(token)) {
        setDeletedProfiles(arrayValue(result, "deleted").map(asObject));
        setArchiveError(arrayValue(result, "warnings").map(String).join("\n"));
      }
    } catch (error) {
      if (archiveRequest.current.isCurrent(token)) setArchiveError(errorMessage(error));
    }
  }, []);
  const archiveScope =
    String(snapshot.startup?.data_root_id || "") +
    ":" +
    String(asObject(snapshot.config?.harness_preferences).home || "");
  useEffect(() => {
    setDeletedProfiles([]);
    setArchiveError("");
    void reloadDeleted();
    return () => archiveRequest.current.cancel();
  }, [archiveScope, reloadDeleted]);
  const archiveProfile = async (name: string) => {
    if (gate.disabled || name === active || pendingDelete !== name) return;
    setPendingDelete(null);
    if (await runAction(t("Delete profile"), "/v1/profiles", { action: "delete", profile: name })) {
      setExpandedProfiles((current) => current.filter((value) => value !== name));
      await reloadDeleted();
    }
  };
  const toggleProfile = (name: string) =>
    setExpandedProfiles((current) =>
      current.includes(name) ? current.filter((item) => item !== name) : [...current, name],
    );
  const manifests = arrayValue(snapshot.profiles, "manifests");
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(
    booleanValue(recovery, "harness_stop_required"),
    harness.state,
    busyAction !== null,
  );
  return (
    <>
      <PageIntro
        kicker={t("Control / Profiles")}
        title={t("Profiles")}
        detail={t(
          "Select a profile to manage its checkpoints and plugins. Deleted profiles are kept for restoration; the current profile cannot be deleted.",
        )}
      />
      <Panel title={t("Profile catalog")} icon={<SlidersHorizontal size={18} />}>
        <div className="button-row">
          {[
            ["settings", t("Open settings.yaml")],
            ["profile_patch", t("Edit profile patch")],
            ["plugin_manifest", t("Edit plugin manifest")],
            ["profile_dir", t("Open profile directory")],
          ].map(([target, label]) => (
            <ActionButton
              key={target}
              disabled={busyAction !== null}
              onClick={() => void runAction(label, "/v1/profiles", { action: "open_path", target })}
            >
              {label}
            </ActionButton>
          ))}
        </div>
        {gate.reason === "stop_required" || gate.reason === "not_stopped" ? (
          <p className="form-error">
            <WarningCircle size={15} />
            {t("Stop Harness before switching profiles or removing plugins.")}
          </p>
        ) : null}
        <DataList
          items={manifests}
          emptyTitle={t("No valid native profiles")}
          emptyDetail={t("Only valid profile manifests are selectable.")}
          render={(item) => {
            const name = stringValue(item, "name") || t("Unnamed profile");
            const expanded = expandedProfiles.includes(name);
            return (
              <section className="profile-entry">
                <div className="profile-entry-header">
                  <button
                    type="button"
                    className="profile-row-toggle"
                    aria-expanded={expanded}
                    onClick={() => toggleProfile(name)}
                  >
                    <span className="profile-chevron" aria-hidden="true">
                      {expanded ? "▾" : "▸"}
                    </span>
                    <strong>{name}</strong>
                    {name === active && <StatusPill label={t("Active")} tone="good" />}
                    <span>
                      {t("{count} plugin bundles", { count: arrayValue(item, "bundles").length })}
                    </span>
                  </button>
                  <span className="row-meta">
                    {name !== active && (
                      <ActionButton
                        disabled={gate.disabled}
                        onClick={() =>
                          void runAction(t("Profile selection"), "/v1/profiles", {
                            action: "select",
                            profile: name,
                          })
                        }
                      >
                        {t("Select")}
                      </ActionButton>
                    )}
                    {name !== active && (
                      <ActionButton
                        tone="danger"
                        disabled={gate.disabled}
                        onClick={() => setPendingDelete(name)}
                      >
                        {t("Delete")}
                      </ActionButton>
                    )}
                  </span>
                </div>
                {pendingDelete === name && (
                  <div
                    className="profile-delete-confirmation"
                    role="group"
                    aria-label={t("Delete profile")}
                  >
                    <p>
                      {t(
                        "Move profile {name} to Deleted profiles? Its files are kept for restoration. Checkpoints and other profiles are unchanged.",
                        { name },
                      )}
                    </p>
                    <div className="button-row">
                      <ActionButton
                        disabled={busyAction !== null}
                        onClick={() => setPendingDelete(null)}
                      >
                        {t("Cancel")}
                      </ActionButton>
                      <ActionButton
                        tone="danger"
                        disabled={gate.disabled}
                        onClick={() => void archiveProfile(name)}
                      >
                        {t("Delete profile")}
                      </ActionButton>
                    </div>
                  </div>
                )}
                {expanded && (
                  <div className="profile-children">
                    <CheckpointsView {...props} embedded profileFilter={name} />
                    <ProfilePlugins {...props} profile={name} />
                  </div>
                )}
              </section>
            );
          }}
        />
        <div className="button-row">
          <input
            className="form-input"
            value={newProfileName}
            placeholder={t("New profile name")}
            disabled={busyAction !== null}
            onChange={(event) => setNewProfileName(event.target.value)}
          />
          <ActionButton
            tone="primary"
            disabled={busyAction !== null || !newProfileName.trim()}
            onClick={() =>
              void runAction(t("Create profile"), "/v1/profiles", {
                action: "create",
                profile: newProfileName.trim(),
              }).then((ok) => {
                if (ok) setNewProfileName("");
              })
            }
          >
            {t("Create profile")}
          </ActionButton>
        </div>
      </Panel>
      <Panel title={t("Deleted profiles")} icon={<Package size={18} />}>
        <p className="field-help">
          {t(
            "Deleting moves the complete profile into a local recovery folder. Close DSH terminals first. Restoring never overwrites an existing profile.",
          )}
        </p>
        <ActionButton disabled={busyAction !== null} onClick={() => void reloadDeleted()}>
          {t("Refresh")}
        </ActionButton>
        {archiveError && (
          <p role="alert" className="form-error">
            {archiveError}
          </p>
        )}
        {deletedProfiles.length ? (
          <div className="table-scroll">
            <table className="request-table">
              <thead>
                <tr>
                  <th>{t("Profile")}</th>
                  <th>{t("Time")}</th>
                  <th>{t("Action")}</th>
                </tr>
              </thead>
              <tbody>
                {deletedProfiles.map((item) => (
                  <tr key={String(item.id)}>
                    <td>{String(item.profile)}</td>
                    <td>
                      {formatTimestamp(Number(item.created_at_unix), t("Not available"), locale)}
                    </td>
                    <td>
                      <ActionButton
                        disabled={gate.disabled || item.can_restore !== true}
                        onClick={() =>
                          void runAction(t("Restore deleted profile"), "/v1/profiles", {
                            action: "restore_deleted",
                            profile: item.id,
                          }).then((ok) => {
                            if (ok) void reloadDeleted();
                          })
                        }
                      >
                        {t("Restore")}
                      </ActionButton>
                      {item.can_restore !== true && (
                        <small>{t("A profile with this name already exists")}</small>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="field-help">{t("No deleted profiles")}</p>
        )}
      </Panel>
      <RestoreStatusPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
    </>
  );
}

export function CheckpointsView({
  snapshot,
  busyAction,
  runAction,
  embedded,
  profileFilter,
}: ViewProps & { embedded?: boolean; profileFilter?: string }) {
  const { locale, t } = useI18n();
  const otherProfile =
    !!profileFilter && profileFilter !== stringValue(snapshot.profiles, "active_profile");
  const items = arrayValue(snapshot.checkpoints, "checkpoints").filter(
    (item) => !profileFilter || stringValue(item, "profile") === profileFilter,
  );
  const snapshots = arrayValue(snapshot.checkpoints, "snapshots").filter(
    (item) =>
      !profileFilter ||
      stringValue(asObject(asObject(item).summary), "profile_name") === profileFilter,
  );
  const [detail, setDetail] = useState<JsonObject | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const latest = useRef(createLatestRequest());
  useEffect(() => () => latest.current.cancel(), []);
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(
    booleanValue(recovery, "harness_stop_required"),
    harness.state,
    busyAction !== null,
  );
  const loadDetail = async (id: string, action: "detail" | "inspect") => {
    const token = latest.current.begin();
    setDetailLoading(true);
    setDetailError(null);
    try {
      const value = await proxyRequest<JsonObject>("/v1/checkpoints", "POST", { action, id });
      if (latest.current.isCurrent(token)) setDetail(value);
    } catch (cause) {
      if (latest.current.isCurrent(token)) {
        setDetail(null);
        setDetailError(errorMessage(cause));
      }
    } finally {
      if (latest.current.isCurrent(token)) setDetailLoading(false);
    }
  };
  return (
    <>
      {!embedded && (
        <PageIntro
          kicker={t("State / Checkpoints")}
          title={t("Checkpoints")}
          detail={`${t("Checkpoint manifests contain only Harness profile/release selection. Agent lifecycle and Harness runtime are never saved or restored.")} ${t("Manual checkpoints contain a bounded redacted snapshot. Legacy entries restore selection metadata only.")}`}
        />
      )}
      {!embedded && (
        <RestoreStatusPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
      )}
      <Panel title={t("Saved checkpoints")} icon={<ListChecks size={18} />}>
        <div className="panel-toolbar">
          <span className="toolbar-count">{t("{count} saved", { count: items.length })}</span>
          <ActionButton
            tone="primary"
            disabled={otherProfile || gate.disabled || snapshot.startup?.available !== true}
            onClick={() =>
              void runAction(t("Checkpoint creation"), "/v1/checkpoints", {
                action: "create",
                note: t("Native launcher checkpoint"),
              })
            }
          >
            <CheckCircle size={16} />
            {t("Create checkpoint")}
          </ActionButton>
        </div>
        {otherProfile && (
          <p className="field-help">{t("Select this profile before creating a checkpoint.")}</p>
        )}
        <DataList
          items={items}
          emptyTitle={t("No checkpoints yet")}
          emptyDetail={t(
            "Create a checkpoint after the Agent has a stable profile and release state.",
          )}
          render={(item) => {
            const id = stringValue(item, "id") || "";
            const reference = asObject(asObject(item).snapshot);
            const summary = asObject(reference.summary);
            const legacy = !Object.keys(reference).length;
            return (
              <>
                <div>
                  <strong>{id || t("Checkpoint")}</strong>
                  <StatusPill
                    label={
                      legacy
                        ? t("Legacy metadata only")
                        : localizedRuntimeState(stringValue(summary, "kind"), t)
                    }
                    tone={legacy ? "warn" : "good"}
                  />
                  <span>
                    {stringValue(item, "profile") || t("No profile")} ·{" "}
                    {stringValue(summary, "dsh_version") ||
                      stringValue(item, "release") ||
                      t("Unknown version")}
                  </span>
                </div>
                <span className="row-meta">
                  {formatTimestamp(
                    numberValue(item, "created_at_unix"),
                    t("Not available"),
                    locale,
                  )}{" "}
                  <ActionButton
                    disabled={detailLoading || legacy}
                    onClick={() => void loadDetail(id, "detail")}
                  >
                    {t("Detail")}
                  </ActionButton>
                  <ActionButton
                    disabled={detailLoading || legacy}
                    onClick={() => void loadDetail(id, "inspect")}
                  >
                    {t("Inspect")}
                  </ActionButton>
                  <ActionButton
                    disabled={gate.disabled}
                    onClick={() =>
                      void runAction(t("Restore checkpoint"), "/v1/checkpoints", {
                        action: "restore",
                        id,
                      })
                    }
                  >
                    {t("Restore")}
                  </ActionButton>
                </span>
              </>
            );
          }}
        />
      </Panel>
      <Panel title={t("Snapshot inventory")} icon={<ClipboardText size={18} />}>
        <p className="field-help">
          {t(
            "Snapshots restore bounded profile and Harness settings files plus the pointer to an installed program version. Project files, full session data, runtimes and complete program copies are excluded. Install a missing version first. Use Retry or Abort for an interrupted restore.",
          )}
        </p>
        {booleanValue(snapshot.checkpoints, "inventory_refresh_pending") ? (
          <p className="field-help" role="status">
            {t(
              "Snapshot inventory refreshes after capture finishes. Existing snapshots have not been removed.",
            )}
          </p>
        ) : (
          <DataList
            items={snapshots}
            emptyTitle={t("No snapshots reported")}
            emptyDetail={t("Healthy and manual snapshots appear here after capture.")}
            render={(item) => {
              const summary = asObject(asObject(item).summary);
              const id =
                stringValue(item, "snapshot_id") || stringValue(summary, "snapshot_id") || "";
              return (
                <>
                  <div>
                    <strong>{id}</strong>
                    <StatusPill
                      label={localizedRuntimeState(stringValue(summary, "kind"), t)}
                      tone={booleanValue(item, "valid") ? "good" : "bad"}
                    />
                    <span>
                      {stringValue(summary, "profile_name")} · {stringValue(summary, "dsh_version")}{" "}
                      · {numberValue(summary, "file_count") ?? 0} {t("files")}
                    </span>
                  </div>
                  <span className="row-meta">
                    <ActionButton
                      disabled={detailLoading}
                      onClick={() => void loadDetail(id, "detail")}
                    >
                      {t("Detail")}
                    </ActionButton>
                    <ActionButton
                      disabled={detailLoading}
                      onClick={() => void loadDetail(id, "inspect")}
                    >
                      {t("Inspect")}
                    </ActionButton>
                    <ActionButton
                      disabled={gate.disabled}
                      onClick={() => {
                        if (window.confirm(t("Restore this snapshot? Harness must be stopped.")))
                          void runAction(t("Restore snapshot"), "/v1/checkpoints", {
                            action: "restore",
                            id,
                          });
                      }}
                    >
                      {t("Restore snapshot")}
                    </ActionButton>
                  </span>
                </>
              );
            }}
          />
        )}
      </Panel>
      {(detailLoading || detailError || detail) && (
        <Panel title={t("Snapshot detail")} icon={<ClipboardText size={18} />}>
          {detailLoading ? (
            <LoadingState />
          ) : detailError ? (
            <ErrorState
              title={t("Snapshot detail failed")}
              message={detailError}
              onRetry={() => {
                setDetail(null);
                setDetailError(null);
              }}
            />
          ) : (
            <SnapshotDetail value={detail} />
          )}
        </Panel>
      )}
    </>
  );
}

export function SnapshotDetail({ value }: { value: JsonObject | null }) {
  const { t } = useI18n();
  const summary = asObject(asObject(value).summary);
  const files = arrayValue(value, "files");
  const errors = arrayValue(value, "errors").map(String);
  return (
    <div className="status-block">
      <dl className="detail-list compact-details">
        <div>
          <dt>{t("Snapshot")}</dt>
          <dd>{stringValue(value, "snapshot_id") || stringValue(summary, "snapshot_id")}</dd>
        </div>
        <div>
          <dt>{t("Kind")}</dt>
          <dd>{localizedRuntimeState(stringValue(summary, "kind"), t)}</dd>
        </div>
        <div>
          <dt>{t("Version")}</dt>
          <dd>{stringValue(summary, "dsh_version")}</dd>
        </div>
        <div>
          <dt>{t("Files")}</dt>
          <dd>{files.length}</dd>
        </div>
      </dl>
      {errors.map((item) => (
        <p className="form-error" key={item}>
          {item}
        </p>
      ))}
      <DataList
        items={files}
        emptyTitle={t("No snapshot files")}
        emptyDetail={t("No bounded file content was returned.")}
        render={(item) => (
          <div className="snapshot-file">
            <strong>{stringValue(item, "path")}</strong>
            <span>
              {localizedRuntimeState(stringValue(item, "state"), t)} ·{" "}
              {numberValue(item, "stored_size") ?? 0} B
            </span>
            {arrayValue(item, "redacted_paths").length > 0 && (
              <small>
                {t("Redacted fields")}: {arrayValue(item, "redacted_paths").map(String).join(", ")}
              </small>
            )}
            {stringValue(item, "omitted_reason") && (
              <small>{t(stringValue(item, "omitted_reason") || "")}</small>
            )}
            {stringValue(item, "content") && <pre>{stringValue(item, "content")}</pre>}
            {booleanValue(item, "content_truncated") && (
              <small className="truncation-note">
                {snapshotContentNote(asObject(item).content_note, t)}
              </small>
            )}
          </div>
        )}
      />
    </div>
  );
}

export function ProfilePlugins({
  snapshot,
  busyAction,
  runAction,
  refresh,
  profile,
}: ViewProps & { profile?: string }) {
  const { t } = useI18n();
  const [pluginBusy, setPluginBusy] = useState(false);
  const [pluginResult, setPluginResult] = useState<JsonObject | null>(null);
  const draggedPlugin = useRef<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const [orderNotice, setOrderNotice] = useState<string | null>(null);
  const [pluginError, setPluginError] = useState<string | null>(null);
  const latestPlugin = useRef(createLatestRequest());
  useEffect(() => () => latestPlugin.current.cancel(), []);
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const active = profile || stringValue(snapshot.profiles, "active_profile") || "";
  const manifests = arrayValue(snapshot.profiles, "manifests");
  const activeManifest =
    manifests.map(asObject).find((item) => stringValue(item, "name") === active) || {};
  const plugins = arrayValue(activeManifest, "plugins");
  const order = arrayValue(activeManifest, "bundles").map(String);
  const sourceProfile = stringValue(activeManifest, "source_profile");
  const orderUndoId = stringValue(activeManifest, "order_undo_id");
  const gate = recoveryMutationGate(
    booleanValue(recovery, "harness_stop_required"),
    harness.state,
    busyAction !== null || pluginBusy,
  );
  const movePlugin = async (packageName: string, destination: string) => {
    if (gate.disabled || sourceProfile) return;
    const move = pluginMoveTarget(order, packageName, destination);
    if (!move) return;
    setPluginBusy(true);
    setPluginError(null);
    setOrderNotice(null);
    setPluginResult(null);
    try {
      const result = await runAction(t("Plugin load order"), "/v1/profiles", {
        action: "plugin_move",
        profile: active,
        package: packageName,
        target: move.target,
      });
      if (result !== false)
        setOrderNotice(t("Load order saved. It takes effect on the next Harness startup."));
    } catch (cause) {
      setPluginError(errorMessage(cause));
    } finally {
      setPluginBusy(false);
    }
  };
  const removePlugin = async (packageName: string) => {
    if (
      !window.confirm(
        t("Remove {package} from profile {profile}?", { package: packageName, profile: active }),
      )
    )
      return;
    const token = latestPlugin.current.begin();
    setPluginBusy(true);
    setPluginError(null);
    setPluginResult(null);
    try {
      const value = await proxyRequest<JsonObject>("/v1/profiles", "POST", {
        action: "plugin_remove",
        profile: active,
        package: packageName,
      });
      if (latestPlugin.current.isCurrent(token)) setPluginResult(value);
    } catch (cause) {
      if (latestPlugin.current.isCurrent(token)) setPluginError(errorMessage(cause));
    } finally {
      if (latestPlugin.current.isCurrent(token)) setPluginBusy(false);
      await refresh();
    }
  };
  return (
    <Panel title={t("Plugin inventory")} icon={<Package size={18} />}>
      <p className="field-help">
        {t(
          "Plugin choices apply to the isolated profile on the next compatibility check. Nothing is uninstalled, the source profile stays unchanged, and running Harness is not changed immediately.",
        )}
      </p>
      <p className="field-help">
        {t(
          "Undo restores only the last saved plugin order. Removing a plugin requires reinstalling it; configuration snapshots do not restore deleted dependencies.",
        )}
      </p>
      {orderUndoId && (
        <ActionButton
          disabled={gate.disabled || !!sourceProfile}
          onClick={() =>
            void runAction(t("Undo plugin order"), "/v1/profiles", {
              action: "plugin_undo_move",
              profile: active,
              target: orderUndoId,
            })
          }
        >
          {t("Undo plugin order")}
        </ActionButton>
      )}
      {(booleanValue(recovery, "harness_stop_required") || gate.reason === "not_stopped") && (
        <div className="notice degraded">
          <WarningCircle size={17} />
          <span>
            {t(
              "Harness must be stopped before profile, plugin, or rollback changes. Diagnostics remain available.",
            )}
          </span>
          <ActionButton
            disabled={busyAction !== null}
            onClick={() => void runAction(t("Harness stop"), "/v1/harness", { action: "stop" })}
          >
            {t("Stop Harness")}
          </ActionButton>
        </div>
      )}
      <p className="panel-description">
        {t(
          "Built-in plugins come from the profile template. Installed plugins are dependency-managed even when included in the load list.",
        )}
      </p>
      <p className="field-help">
        {t(
          "Drag plugins to change loading order, or use the arrow buttons. dsh-base and dsh-web-app stay in positions 1 and 2.",
        )}
      </p>
      {sourceProfile && (
        <p className="notice">
          {t(
            "This is a generated isolation profile. Edit plugin order in source profile {profile}.",
            { profile: sourceProfile },
          )}
        </p>
      )}
      {!plugins.length ? (
        <EmptyState
          title={t("No plugins reported")}
          detail={t("Select a valid native profile to inspect its inventory.")}
        />
      ) : (
        <div className="data-list">
          {plugins.map((item) => {
            const packageName = stringValue(item, "package") || "";
            const builtin = booleanValue(item, "builtin"),
              removable = booleanValue(item, "removable");
            const index = order.indexOf(packageName),
              fixed = FIXED_PROFILE_PLUGINS.includes(packageName);
            const isolation = pluginIsolationChoice(
              asObject(snapshot.profiles),
              active,
              packageName,
              gate.disabled || snapshot.startup?.available !== true,
            );
            const movable = index >= 0 && !fixed && !gate.disabled && !sourceProfile;
            return (
              <div
                key={packageName}
                className={`data-row plugin-row${dropTarget === packageName ? " plugin-drop-target" : ""}`}
                data-plugin={packageName}
                onDragOver={(event) => {
                  if (movable && draggedPlugin.current && draggedPlugin.current !== packageName) {
                    event.preventDefault();
                    event.dataTransfer.dropEffect = "move";
                    setDropTarget(packageName);
                  }
                }}
                onDragLeave={() =>
                  setDropTarget((current) => (current === packageName ? null : current))
                }
                onDrop={(event) => {
                  event.preventDefault();
                  const source = draggedPlugin.current;
                  draggedPlugin.current = null;
                  setDropTarget(null);
                  if (source && movable) void movePlugin(source, packageName);
                }}
              >
                <div>
                  <span
                    className="plugin-drag-handle"
                    draggable={movable}
                    title={
                      movable
                        ? t("Drag to reorder")
                        : fixed
                          ? t("Fixed load position")
                          : t("Loading order unavailable")
                    }
                    onDragStart={(event) => {
                      if (!movable) {
                        event.preventDefault();
                        return;
                      }
                      draggedPlugin.current = packageName;
                      event.dataTransfer.effectAllowed = "move";
                      event.dataTransfer.setData("text/plain", packageName);
                    }}
                    onDragEnd={() => {
                      draggedPlugin.current = null;
                      setDropTarget(null);
                    }}
                    aria-hidden="true"
                  >
                    {fixed ? "●" : index >= 0 ? "⠿" : "·"}
                  </span>
                  {index >= 0 && <span className="plugin-position">{index + 1}</span>}
                  <strong>{packageName}</strong>
                  <StatusPill
                    label={builtin ? t("Built-in") : removable ? t("Removable") : t("Protected")}
                    tone={removable ? "warn" : "neutral"}
                  />
                  {fixed && <StatusPill label={t("Fixed load position")} tone="neutral" />}
                  {index < 0 && <span>{t("Dependency only; not in the load list")}</span>}
                  <span>{stringValue(item, "version") || t("Unknown version")}</span>
                </div>
                <span className="row-meta button-row">
                  {isolation.eligible &&
                    (isolation.known ? (
                      <ActionButton
                        disabled={!isolation.command}
                        onClick={() => {
                          if (isolation.command)
                            void runAction(
                              t(
                                isolation.disabled
                                  ? "Enable on next check"
                                  : "Disable on next check",
                              ),
                              "/v1/profiles",
                              isolation.command,
                            );
                        }}
                      >
                        {t(isolation.disabled ? "Enable on next check" : "Disable on next check")}
                      </ActionButton>
                    ) : (
                      <span className="field-help">
                        {t(
                          "Select the source profile and run its compatibility check to manage plugin choices.",
                        )}
                      </span>
                    ))}
                  {index >= 0 && !fixed && (
                    <>
                      <ActionButton
                        title={t("Move up")}
                        disabled={
                          !movable ||
                          index === 0 ||
                          FIXED_PROFILE_PLUGINS.includes(order[index - 1])
                        }
                        onClick={() => void movePlugin(packageName, order[index - 1])}
                      >
                        ↑
                      </ActionButton>
                      <ActionButton
                        title={t("Move down")}
                        disabled={
                          !movable ||
                          index === order.length - 1 ||
                          FIXED_PROFILE_PLUGINS.includes(order[index + 1])
                        }
                        onClick={() => void movePlugin(packageName, order[index + 1])}
                      >
                        ↓
                      </ActionButton>
                    </>
                  )}
                  {removable && (
                    <ActionButton
                      tone="danger"
                      disabled={gate.disabled || !!sourceProfile}
                      onClick={() => void removePlugin(packageName)}
                    >
                      {pluginBusy ? t("Working") : t("Remove")}
                    </ActionButton>
                  )}
                </span>
              </div>
            );
          })}
        </div>
      )}
      {orderNotice && <p role="status">{orderNotice}</p>}
      {pluginError && (
        <p className="form-error">
          <WarningCircle size={15} />
          {pluginError}{" "}
          <button className="button subtle" onClick={() => setPluginError(null)}>
            {t("Dismiss")}
          </button>
        </p>
      )}
      {pluginResult && (
        <pre className="output-block">
          {[stringValue(pluginResult, "stdout"), stringValue(pluginResult, "stderr")]
            .filter(Boolean)
            .join("\n") || t("Plugin removed. Inventory refreshed.")}
        </pre>
      )}
    </Panel>
  );
}
