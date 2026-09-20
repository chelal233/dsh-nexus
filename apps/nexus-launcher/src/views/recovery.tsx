import { confirmAction } from "../confirmation";
import { type HarnessPanelProps, type JsonObject, type ViewProps } from "../app-types";
import { PageIntro, Panel, ActionButton, DataList, EmptyState } from "../ui-components";
import { stringValue, arrayValue, asObject, booleanValue, numberValue } from "../json-values";
import { useI18n } from "../i18n";
import { ShieldCheck, TerminalWindow, Pulse, ListChecks } from "@phosphor-icons/react";
import { proxyRequest } from "../agent-bridge";
import { errorMessage, localizedRuntimeState, formatTimestamp } from "../display-format";
import { useState, useEffect, useRef } from "react";
import { createLatestRequest } from "../control-state";
import { sharedRequestClient, mergeRequestHistory } from "../request-client";

export function ReadOnlyRecoveryView({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  return (
    <>
      <PageIntro
        kicker={t("Recovery")}
        title={t("Agent is online in read-only recovery")}
        detail={t(
          "Choose a valid recovery time to restore Nexus records. If no supported recovery point is available, export diagnostics. Normal editing and Harness startup remain blocked.",
        )}
      />
      <Panel title={t("Recovery")} icon={<ShieldCheck size={18} />}>
        <p className="field-help">{stringValue(snapshot.health, "recovery_reason")}</p>
        <div className="button-row">
          <ActionButton
            tone="primary"
            disabled={busyAction !== null}
            onClick={() =>
              void runAction(t("Export diagnostics"), "/v1/diagnostics", { action: "export" })
            }
          >
            {t("Export diagnostics")}
          </ActionButton>
          <ActionButton
            disabled={busyAction !== null}
            onClick={() =>
              void runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })
            }
          >
            {t("Force restart Agent")}
          </ActionButton>
        </div>
      </Panel>
      <RecoveryRecordWizard
        disabled={busyAction !== null}
        restartAgent={() => runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })}
      />
    </>
  );
}

export function RecoveryRecordWizard({
  disabled,
  restartAgent,
  stopHarness,
}: {
  disabled: boolean;
  restartAgent?: () => Promise<boolean | void>;
  stopHarness?: () => Promise<boolean | void>;
}) {
  const { t } = useI18n();
  const [history, setHistory] = useState<JsonObject | null>(null);
  const [selected, setSelected] = useState("");
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<JsonObject | null>(null);
  const inspect = async () => {
    setWorking(true);
    setError("");
    try {
      setHistory(
        await proxyRequest<JsonObject>("/v1/recovery/records", "POST", { action: "list" }),
      );
      setSelected("");
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setWorking(false);
    }
  };
  useEffect(() => {
    if (!disabled && !history && !working) void inspect();
  }, [disabled]);
  const restore = async () => {
    if (!selected || !history || working || disabled) return;
    setWorking(true);
    setError("");
    setResult(null);
    try {
      if (stopHarness && !(await stopHarness()))
        throw new Error(t("Harness could not be stopped. No record was restored."));
      setResult(
        await proxyRequest<JsonObject>("/v1/recovery/records", "POST", {
          action: "restore",
          point_id: selected,
          expected_revision: history.expected_revision,
        }),
      );
      setSelected("");
      if (restartAgent && !(await restartAgent()))
        setError(t("Record restored. Agent restart failed; retry restarting Agent."));
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setWorking(false);
    }
  };
  const points = arrayValue(history, "recovery_points").map(asObject);
  return (
    <Panel title={t("Restore Nexus records")} icon={<ShieldCheck size={18} />}>
      <p>
        {t(
          "Choose a time and restore. This restores the active profile and known profile names only; Harness files, plugins and conversations are not changed.",
        )}
      </p>
      <label className="form-field">
        <span>{t("Recovery time")}</span>
        <select
          className="form-input"
          value={selected}
          disabled={disabled || working || !points.length}
          onChange={(event) => setSelected(event.target.value)}
        >
          <option value="">{t("Choose a recovery time")}</option>
          {points.map((point) => (
            <option key={String(point.id)} value={String(point.id)}>
              {new Date(Number(point.created_at_unix) * 1000).toLocaleString()} ·{" "}
              {String(point.active_profile)} ·{" "}
              {t("{count} profiles", { count: Number(point.profile_count) })}
            </option>
          ))}
        </select>
      </label>
      {history && !history.history_error && !points.length && (
        <p>
          {t(
            "No valid recovery history is available. Nexus cannot restore a time that was never backed up.",
          )}
        </p>
      )}
      <p className="field-help">
        {t("The current record is backed up first. Harness remains stopped after recovery.")}
      </p>
      <div className="button-row">
        <ActionButton
          tone="primary"
          disabled={
            disabled ||
            working ||
            !selected ||
            !history?.expected_revision ||
            !!history?.history_error
          }
          onClick={() => void restore()}
        >
          {t(working ? "Working…" : "Restore with one click")}
        </ActionButton>
        <ActionButton disabled={disabled || working} onClick={() => void inspect()}>
          {t("Refresh")}
        </ActionButton>
      </div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {Boolean(history?.history_error) && (
        <p className="form-error" role="alert">
          {String(history?.history_error)}
        </p>
      )}
      {result && <p role="status">{t("Nexus record restored. Harness has not been started.")}</p>}
      {Boolean(result || history?.restore_blocked) && (
        <details>
          <summary>{t("Technical details")}</summary>
          {Boolean(history?.restore_blocked) && !result && (
            <p>{String(history?.restore_blocked)}</p>
          )}
          {result && (
            <>
              <p>
                {t("Private backup")}: {String(result.backup_path)}
              </p>
              {Boolean(result.state_warning) && <p>{String(result.state_warning)}</p>}
            </>
          )}
        </details>
      )}
    </Panel>
  );
}

export function RecoveryLogTail({ snapshot }: Pick<ViewProps, "snapshot">) {
  const { t } = useI18n();
  const recovery = asObject(snapshot.recovery);
  const tails = arrayValue(recovery, "log_tail");
  return (
    <Panel title={t("Bounded redacted log tail")} icon={<TerminalWindow size={18} />}>
      <DataList
        items={tails}
        emptyTitle={t("No recovery log tail")}
        emptyDetail={t("No current Nexus-owned Harness log session is available.")}
        render={(item) => (
          <div className="snapshot-file">
            <strong>{stringValue(item, "stream")}</strong>
            <pre>{stringValue(item, "content")}</pre>
            {booleanValue(item, "truncated") && (
              <small className="truncation-note">
                {t("Log truncated by the Agent response limit.")}
              </small>
            )}
          </div>
        )}
      />
    </Panel>
  );
}

function RecoveryDiagnostics({
  snapshot,
  busyAction,
  runAction,
  refresh,
}: Pick<ViewProps, "snapshot" | "busyAction" | "runAction" | "refresh">) {
  const { t } = useI18n();
  const recovery = asObject(snapshot.recovery);
  const errors = arrayValue(recovery, "diagnostic_errors").map(String);
  return (
    <>
      <Panel title={t("Startup recovery status")} icon={<Pulse size={18} />}>
        <dl className="detail-list">
          <div>
            <dt>{t("Harness state")}</dt>
            <dd>{localizedRuntimeState(stringValue(asObject(recovery.harness), "state"), t)}</dd>
          </div>
          <div>
            <dt>{t("Startup error")}</dt>
            <dd>{stringValue(recovery, "startup_error") || t("None reported")}</dd>
          </div>
          <div>
            <dt>{t("Fatal prefix observed")}</dt>
            <dd>
              {booleanValue(recovery, "fatal_prefix_observed") ? t("Yes, advisory only") : t("No")}
            </dd>
          </div>
        </dl>
        {errors.map((item) => (
          <p className="form-error" key={item}>
            {item}
          </p>
        ))}
        <div className="button-row">
          <ActionButton disabled={busyAction !== null} onClick={() => void refresh()}>
            {t("Refresh")}
          </ActionButton>
          <ActionButton
            disabled={busyAction !== null}
            onClick={() =>
              void runAction(t("Export diagnostics"), "/v1/diagnostics", {
                action: "export",
                note: t("Manual recovery collection"),
              })
            }
          >
            {t("Export diagnostics")}
          </ActionButton>
        </div>
      </Panel>
      <RecoveryLogTail snapshot={snapshot} />
    </>
  );
}

export function RequestHistory({ busy, dataRootId }: { busy: boolean; dataRootId: string }) {
  const { t, locale } = useI18n();
  const [items, setItems] = useState<unknown[]>([]);
  const [loading, setLoading] = useState(false);
  const [message, setMessage] = useState("");
  const latest = useRef(createLatestRequest());
  useEffect(() => {
    latest.current.cancel();
    setItems([]);
    setLoading(false);
    setMessage("");
    return () => latest.current.cancel();
  }, [dataRootId]);
  const localClient = () =>
    sharedRequestClient(
      window.localStorage,
      (route, method, payload) => proxyRequest<JsonObject>(route, method, payload),
      dataRootId,
    );
  const load = async () => {
    const token = latest.current.begin();
    setLoading(true);
    setMessage("");
    try {
      const server = arrayValue(await proxyRequest<JsonObject>("/v1/requests"), "requests").map(
        asObject,
      );
      if (latest.current.isCurrent(token))
        setItems(mergeRequestHistory(server, localClient().pending()));
    } catch (error) {
      if (latest.current.isCurrent(token)) {
        setItems([]);
        setMessage(errorMessage(error));
      }
    } finally {
      if (latest.current.isCurrent(token)) setLoading(false);
    }
  };
  const release = async (id: string) => {
    if (
      !(await confirmAction(
        t(
          "The previous operation may have changed data. Check the current version and Recovery first. Allow a new attempt with a new request reference?",
        ),
      ))
    )
      return;
    try {
      localClient().forget(id);
      setItems((current) =>
        current.filter(
          (raw) =>
            !(stringValue(raw, "request_id") === id && stringValue(raw, "state") === "unconfirmed"),
        ),
      );
      setMessage(
        t("The retry reference was cleared. The recorded operation and its data were not changed."),
      );
    } catch (error) {
      setMessage(errorMessage(error));
    }
  };
  return (
    <Panel title={t("Recent operation requests")} icon={<ListChecks size={18} />}>
      <p className="field-help">
        {t(
          "After a timeout, the same request checks its original receipt instead of repeating the operation. Accepted installations still have their own progress.",
        )}
      </p>
      <ActionButton disabled={busy || loading} onClick={() => void load()}>
        {t("Check previous requests")}
      </ActionButton>
      {message && <p className="field-help">{message}</p>}
      <div className="table-scroll">
        <table className="request-table">
          <thead>
            <tr>
              <th>{t("Request ID")}</th>
              <th>{t("Action")}</th>
              <th>{t("Status")}</th>
              <th>{t("Time")}</th>
              <th>{t("Details")}</th>
            </tr>
          </thead>
          <tbody>
            {!items.length && (
              <tr>
                <td colSpan={5} className="field-help">
                  {t(loading ? "Loading request history" : "No request records to display")}
                </td>
              </tr>
            )}
            {items
              .slice()
              .reverse()
              .map((raw) => {
                const item = asObject(raw),
                  id = stringValue(item, "request_id") || "",
                  state = stringValue(item, "state");
                const accepted = state === "completed" && item.http_status === 202;
                const label = accepted
                  ? t("Accepted")
                  : state === "running"
                    ? t("Running")
                    : state === "completed"
                      ? t("Completed")
                      : state === "interrupted"
                        ? t("Interrupted")
                        : state === "unconfirmed"
                          ? t("No server receipt found")
                          : t("Failed");
                return (
                  <tr key={id}>
                    <td>
                      <code title={id}>
                        {id.length > 20 ? id.slice(0, 12) + "…" + id.slice(-6) : id}
                      </code>
                    </td>
                    <td>{t(stringValue(item, "kind") || "Unknown")}</td>
                    <td>{label}</td>
                    <td>
                      {formatTimestamp(
                        numberValue(item, "created_at_unix"),
                        t("Not available"),
                        locale,
                      )}
                    </td>
                    <td>
                      <details>
                        <summary>{t("Details")}</summary>
                        <div className="request-details">
                          <code>{id}</code>
                          <p>
                            {stringValue(item, "operation_id") || stringValue(item, "target_id")}
                          </p>
                          {stringValue(item, "error_code") && (
                            <p>{stringValue(item, "error_code")}</p>
                          )}
                          {accepted && (
                            <p>
                              {t(
                                "The original request was accepted. Check the operation for its final result.",
                              )}
                            </p>
                          )}
                          {state === "unconfirmed" && (
                            <p>
                              {t(
                                "A local retry reference exists, but no server receipt was found. This does not prove the operation never ran. Inspect the current version and Recovery before allowing a new attempt.",
                              )}
                            </p>
                          )}
                          {(state === "interrupted" || state === "unconfirmed") && (
                            <ActionButton
                              disabled={busy || loading || !dataRootId}
                              onClick={() => release(id)}
                            >
                              {t("Allow a new attempt")}
                            </ActionButton>
                          )}
                        </div>
                      </details>
                    </td>
                  </tr>
                );
              })}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}

export function LiveLogRetention({ value }: { value: JsonObject | null }) {
  const { t } = useI18n();
  if (!value) return null;
  const state = stringValue(value, "state");
  const label =
    state === "available"
      ? t("Active")
      : state === "catching_up"
        ? t("Catching up")
        : state === "limited"
          ? t("Limited")
          : t("Not available");
  const files = Object.entries(asObject(value.files) ?? {});
  const bytes = (amount: number | undefined) =>
    amount === undefined ? t("Not available") : `${(amount / 1024 / 1024).toFixed(2)} MiB`;
  return (
    <details className="storage-live-logs">
      <summary>
        {t("Live log allocation")} · {label}
      </summary>
      <div>
        <p>
          <strong>{label}</strong>
        </p>
        <p className="field-help">
          {t(
            "Old log contents are reclaimed while keeping the recent failure tail and session access. Logical file size can keep growing; allocated size is the actual disk space used.",
          )}
        </p>
        {state === "catching_up" && (
          <p className="field-help">
            {t(
              "Log scanning is catching up. Unscanned contents are kept until they can be processed safely.",
            )}
          </p>
        )}
        {state === "limited" && (
          <p className="field-help">
            {t(
              "Some logs could not be reclaimed safely. Original logs are kept; export diagnostics to inspect the limitation.",
            )}
          </p>
        )}
        {files.map(([name, raw]) => {
          const file = asObject(raw),
            allocation = asObject(file?.allocation);
          return (
            <div className="summary-row" key={name}>
              <strong>{name}</strong>
              <span>
                {Object.keys(allocation).length
                  ? t("Allocated: {allocated}; logical: {logical}", {
                      allocated: bytes(numberValue(allocation, "allocated_bytes")),
                      logical: bytes(numberValue(allocation, "logical_bytes")),
                    })
                  : t("Not available")}
                {(numberValue(file, "scan_backlog_bytes") ?? 0) > 0 &&
                  ` · ${t("Waiting to scan: {size}", { size: bytes(numberValue(file, "scan_backlog_bytes")) })}`}
              </span>
            </div>
          );
        })}
      </div>
    </details>
  );
}

export function DiagnosticsView({ snapshot, busyAction, runAction, refresh, embedded }: ViewProps) {
  const { locale, t } = useI18n();
  const items = arrayValue(snapshot.diagnostics, "bundles");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  const open = (bundle: string, file?: string) =>
    void runAction(t(file ? "Open file" : "Open file location"), "/v1/diagnostics", {
      action: "open_path",
      bundle,
      ...(file ? { file } : {}),
    });
  return (
    <>
      {!embedded && (
        <PageIntro
          kicker={t("Observability / Diagnostics")}
          title={t("Diagnostics")}
          detail={t(
            "Bundles are bounded, redacted, and limited to Nexus-owned metadata and text logs.",
          )}
        />
      )}
      <Panel title={t("Diagnostic bundles")} icon={<TerminalWindow size={18} />}>
        <p>
          {t(
            "The exported JSON is one portable file containing the redacted diagnostic context and logs.",
          )}
        </p>
        {arrayValue(snapshot.diagnostics, "warnings").map((item, index) => (
          <p className="form-error" role="alert" key={index}>
            {stringValue(item, "bundle_id")}:{" "}
            {t(
              stringValue(item, "reason") ||
                "Unreadable or unsupported diagnostic record was preserved",
            )}
          </p>
        ))}
        <div className="panel-toolbar">
          <span className="toolbar-count">{t("{count} bundles", { count: items.length })}</span>
          <ActionButton
            tone="primary"
            disabled={busyAction !== null}
            onClick={() =>
              void runAction(t("Export diagnostics"), "/v1/diagnostics", {
                action: "export",
                note: t("Native launcher collection"),
              })
            }
          >
            <TerminalWindow size={16} />
            {t("Export diagnostics")}
          </ActionButton>
        </div>
        {!items.length ? (
          <EmptyState
            title={t("No diagnostic bundles")}
            detail={t("Collect a bounded bundle when a runtime issue needs review.")}
          />
        ) : (
          items.map((item) => {
            const bundle = asObject(item),
              id = stringValue(bundle, "id") || "";
            return (
              <details key={id} className="diagnostic-bundle">
                <summary>
                  {id} · {t("{count} files", { count: arrayValue(bundle, "files").length })} ·{" "}
                  {formatTimestamp(
                    numberValue(bundle, "created_at_unix"),
                    t("Not available"),
                    locale,
                  )}
                </summary>
                <p className="field-help">{stringValue(bundle, "directory")}</p>
                <div className="button-row">
                  <ActionButton disabled={controlsDisabled || !id} onClick={() => open(id)}>
                    {t("Open file location")}
                  </ActionButton>
                  <ActionButton
                    disabled={controlsDisabled || !id}
                    onClick={() => open(id, "diagnostics.json")}
                  >
                    {t("Open bundle manifest")}
                  </ActionButton>
                </div>
                <DataList
                  items={arrayValue(bundle, "files")}
                  emptyTitle={t("No files collected")}
                  emptyDetail={t("Open bundle manifest")}
                  render={(file) => {
                    const name = stringValue(file, "name") || "";
                    return (
                      <>
                        <div>
                          <strong>{name}</strong>
                          <span>{numberValue(file, "bytes")} B</span>
                        </div>
                        <ActionButton
                          disabled={controlsDisabled || !id || !name}
                          onClick={() => open(id, name)}
                        >
                          {t("Open file")}
                        </ActionButton>
                      </>
                    );
                  }}
                />
              </details>
            );
          })
        )}
      </Panel>
      <RequestHistory
        busy={busyAction !== null}
        dataRootId={stringValue(snapshot.startup, "data_root_id") || ""}
      />
      <RecoveryDiagnostics
        snapshot={snapshot}
        busyAction={busyAction}
        runAction={runAction}
        refresh={refresh}
      />
    </>
  );
}
