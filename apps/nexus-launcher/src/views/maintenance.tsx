import { type ViewProps, type JsonObject } from "../app-types";
import { PageIntro, Panel, ActionButton, StatusPill, DataList } from "../ui-components";
import { DiagnosticsView, RecoveryRecordWizard, LiveLogRetention } from "./recovery";
import {
  stringValue,
  harnessRuntimeValue,
  booleanValue,
  numberValue,
  asObject,
  arrayValue,
  isObject,
} from "../json-values";
import { useI18n } from "../i18n";
import { proxyRequest } from "../agent-bridge";
import { errorMessage, localizedRuntimeState } from "../display-format";
import { useState, useRef, useEffect, useCallback } from "react";
import { createLatestRequest, hasHarnessSource } from "../control-state";
import { Cpu, Package, WarningCircle } from "@phosphor-icons/react";
import {
  type CleanupSelection,
  cleanupSelectedIds,
  cleanupGroups,
  toggleCleanupGroup,
} from "../settings-state";

export function MaintenanceView(props: ViewProps & { activity?: React.ReactNode }) {
  const { t } = useI18n();
  return (
    <>
      <PageIntro
        kicker={t("Maintenance")}
        title={t("Maintenance")}
        detail={t("Inspect errors, collect diagnostics, and recover from startup failures.")}
      />
      {props.activity}
      <DiagnosticsView {...props} embedded />
      <CanaryPanel {...props} />
      <RecoveryRecordWizard
        disabled={props.busyAction !== null || props.snapshot.startup?.available !== true}
        stopHarness={async () =>
          ["stopped", "detached"].includes(
            stringValue(harnessRuntimeValue(props.snapshot.harnessRuntime), "state") || "",
          ) || props.runAction(t("Stop Harness"), "/v1/harness", { action: "stop" })
        }
        restartAgent={() =>
          props.runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })
        }
      />
      <SpaceMaintenancePanel {...props} />
    </>
  );
}

export function CanaryPanel({ snapshot, busyAction, runAction, openWorkbench }: ViewProps) {
  const { t } = useI18n();
  const [status, setStatus] = useState<JsonObject>({});
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);
  const generation = useRef(0);
  const actionPending = useRef(false);
  const [pollEpoch, setPollEpoch] = useState(0);
  const [historyRecord, setHistoryRecord] = useState<JsonObject | null>(null);
  const [historyError, setHistoryError] = useState("");
  const historyRequest = useRef(createLatestRequest());
  useEffect(() => () => historyRequest.current.cancel(), []);
  const openHistory = async (id: string) => {
    const token = historyRequest.current.begin();
    setHistoryError("");
    setHistoryRecord(null);
    try {
      const value = await proxyRequest("/v1/canary", "POST", {
        action: "history",
        operation_id: id,
      });
      if (historyRequest.current.isCurrent(token)) setHistoryRecord(value);
    } catch (e) {
      if (historyRequest.current.isCurrent(token)) setHistoryError(errorMessage(e));
    }
  };
  const available =
    snapshot.startup?.available === true && !booleanValue(snapshot.health, "degraded");
  useEffect(() => {
    if (!available) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    let inFlight = false;
    const poll = async () => {
      if (disposed || inFlight || actionPending.current) return;
      clearTimeout(timer);
      inFlight = true;
      let active = false;
      const requestGeneration = generation.current;
      try {
        const value = await proxyRequest("/v1/canary");
        active =
          value.phase === "running" ||
          value.phase === "cancelling" ||
          value.cleanup_pending === true;
        if (!disposed && requestGeneration === generation.current) {
          setStatus(value);
          setError("");
        }
      } catch (e) {
        if (!disposed && requestGeneration === generation.current) setError(errorMessage(e));
      } finally {
        inFlight = false;
      }
      if (!disposed)
        timer = setTimeout(
          () => void poll(),
          document.visibilityState === "hidden" ? 15000 : active ? 2000 : 15000,
        );
    };
    const wake = () => {
      if (document.visibilityState !== "hidden") void poll();
    };
    window.addEventListener("focus", wake);
    document.addEventListener("visibilitychange", wake);
    void poll();
    return () => {
      disposed = true;
      clearTimeout(timer);
      window.removeEventListener("focus", wake);
      document.removeEventListener("visibilitychange", wake);
    };
  }, [available, pollEpoch]);
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const harnessState = stringValue(harness, "state") || "unknown";
  const stopped =
    ["stopped", "detached", "failed"].includes(harnessState) && !numberValue(harness, "pid");
  const startReady = available && stopped && hasHarnessSource(snapshot.config, snapshot.releases);
  const running =
    status.phase === "running" || status.phase === "cancelling" || status.cleanup_pending === true;
  const act = async (mode?: string) => {
    generation.current += 1;
    actionPending.current = true;
    setPending(true);
    setError("");
    try {
      const value = await proxyRequest(
        "/v1/canary",
        "POST",
        mode ? { action: "start", mode } : { action: "cancel", operation_id: status.operation_id },
      );
      setStatus(value);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      actionPending.current = false;
      setPending(false);
      setPollEpoch((value) => value + 1);
    }
  };
  return (
    <Panel title={t("Canary diagnostics")} icon={<Cpu size={18} />}>
      <p className="field-help">
        {t(
          "Tests a temporary profile and home. Plugins still have system and network access. Stop Harness first.",
        )}{" "}
        {t(
          "Feature interactions are not verified. Results never disable plugins or modify the production profile.",
        )}
      </p>
      {!stopped && (
        <div className="notice degraded">
          <WarningCircle size={17} />
          <p>
            {t(
              "Stop Harness explicitly before running diagnostics. Your selection and settings are kept.",
            )}
          </p>
          <div className="notice-actions">
            <ActionButton
              disabled={
                !available ||
                busyAction !== null ||
                pending ||
                !["running", "starting", "failed"].includes(harnessState)
              }
              onClick={() =>
                void runAction(t("Stop Harness for diagnostics"), "/v1/harness", { action: "stop" })
              }
            >
              {t("Stop Harness for diagnostics")}
            </ActionButton>
            <ActionButton onClick={() => openWorkbench?.()}>
              {t("Return to Workbench")}
            </ActionButton>
          </div>
        </div>
      )}
      <div className="button-row">
        <ActionButton
          disabled={!startReady || pending || running || busyAction !== null}
          onClick={() => void act("diagnostic_only")}
        >
          {t("Run isolated diagnostic")}
        </ActionButton>
        <ActionButton
          disabled={!startReady || pending || running || busyAction !== null}
          onClick={() => void act("bisect")}
        >
          {t("Find failing plugin combination")}
        </ActionButton>
        <ActionButton disabled={!available || pending || !running} onClick={() => void act()}>
          {t("Cancel and clean up")}
        </ActionButton>
        {Boolean(status.phase) && (
          <span role="status" className="field-help">
            {t("Canary phase")}: {localizedRuntimeState(stringValue(status, "phase"), t)}
          </span>
        )}
      </div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {Boolean(status.report) && <CanaryReport report={asObject(status.report)} />}
      {Boolean(status.progress) && (
        <details open={running}>
          <summary>{t("Probe details")}</summary>
          <CanaryProgress progress={asObject(status.progress)} running={running} />
        </details>
      )}
      {Boolean(status.error) && <pre>{String(status.error)}</pre>}
      {Boolean(status.cleanup_error) && <pre>{String(status.cleanup_error)}</pre>}
      {Boolean(status.report) && (
        <details>
          <summary>{t("Canary report and original errors")}</summary>
          <pre>{JSON.stringify(status.report, null, 2)}</pre>
        </details>
      )}
      {Boolean(status.history_error) && (
        <p className="form-error" role="alert">
          {String(status.history_error)}
        </p>
      )}
      {arrayValue(status, "history").length > 0 && (
        <details>
          <summary>{t("Recent Canary diagnostics")}</summary>
          <ul>
            {arrayValue(status, "history").map((item) => {
              const record = asObject(item);
              return (
                <li key={String(record.operation_id)}>
                  <ActionButton
                    disabled={!available}
                    onClick={() => void openHistory(String(record.operation_id))}
                  >
                    {String(record.source_profile || "")} ·{" "}
                    {localizedRuntimeState(stringValue(record, "phase"), t)} ·{" "}
                    {new Date(Number(record.finished_at_unix) * 1000).toLocaleString()}
                  </ActionButton>
                </li>
              );
            })}
          </ul>
          {historyError && (
            <p className="form-error" role="alert">
              {historyError}
            </p>
          )}
          {historyRecord && (
            <>
              <CanaryReport report={asObject(historyRecord.report)} />
              <pre>{JSON.stringify(historyRecord, null, 2)}</pre>
            </>
          )}
        </details>
      )}
    </Panel>
  );
}

export function CanaryProgress({ progress, running }: { progress: JsonObject; running: boolean }) {
  const { t } = useI18n();
  const stages: Record<string, string> = {
    planning: t("Checking copy space"),
    copying_and_probing: t("Copying and probing"),
    round_finished: t("Round finished"),
  };
  const elapsed =
    running && progress.round_started_at_unix
      ? Math.max(0, Math.floor(Date.now() / 1000) - Number(progress.round_started_at_unix))
      : null;
  return (
    <div className="canary-summary">
      <p role="status">
        {t("Canary round {round} of at most {limit}", {
          round: Number(progress.round_index || 0) + 1,
          limit: Number(progress.round_limit || 20),
        })}{" "}
        · {stages[String(progress.stage)] || ""}
        {elapsed !== null ? ` · ${elapsed}s` : ""}
      </p>
      <p>
        {t("Current plugin combination")}:{" "}
        {progress.enabled_bundles === null
          ? t("All enabled third-party plugins")
          : arrayValue(progress, "enabled_bundles").map(String).join(", ") ||
            t("No third-party plugins")}
      </p>
      <details open={running}>
        <summary>
          {t("Completed probe rounds")} ({arrayValue(progress, "completed_rounds").length})
        </summary>
        <ul>
          {arrayValue(progress, "completed_rounds").map((item, index) => {
            const r = asObject(item);
            return (
              <li key={index}>
                {index + 1}. {localizedRuntimeState(stringValue(r, "outcome"), t)} ·{" "}
                {t("{seconds} seconds", { seconds: Math.round(Number(r.duration_ms || 0) / 1000) })}{" "}
                ·{" "}
                {arrayValue(r, "enabled_bundles").map(String).join(", ") ||
                  t("No third-party plugins")}
              </li>
            );
          })}
        </ul>
      </details>
    </div>
  );
}

export function CanaryReport({ report }: { report: JsonObject }) {
  const { t } = useI18n();
  const checks = asObject(report.checks);
  const labels: Record<string, string> = {
    passed: t("Passed"),
    failed: t("Failed"),
    inconclusive: t("Inconclusive"),
    unsupported: t("Not verified"),
  };
  const checksToShow = [
    ["startup", t("Harness startup")],
    ["loader_and_web_document", t("Plugin loading and Web document")],
    ["feature", t("Commands, panels and interactions")],
  ];
  return (
    <div className="canary-summary">
      <dl className="detail-list">
        {checksToShow.map(([key, label]) => (
          <div key={key}>
            <dt>{label}</dt>
            <dd>{labels[String(checks[key])] || t("Not available")}</dd>
          </div>
        ))}
      </dl>
      <p className="field-help">
        {t("{count} probe rounds", { count: arrayValue(report, "rounds").length })}
      </p>
      {arrayValue(report, "suspect_combination").length > 0 && (
        <p>
          {t("Reproduced plugin combination")}:{" "}
          {arrayValue(report, "suspect_combination").map(String).join(", ")}
        </p>
      )}
    </div>
  );
}

export function SpaceMaintenancePanel({ busyAction, snapshot }: ViewProps) {
  const { t } = useI18n();
  const [status, setStatus] = useState<JsonObject>(() => asObject(snapshot.maintenance));
  const [days, setDays] = useState("30");
  const [cleanupSelection, setCleanupSelection] = useState<CleanupSelection>({
    previewId: "",
    ids: [],
  });
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const preview = asObject(status.preview),
    result = asObject(status.result);
  const previewScan = asObject(status.preview_scan);
  const scanning = previewScan.state === "running";
  const selected = cleanupSelectedIds(cleanupSelection, preview.preview_id);
  const load = useCallback(async (clearError = false) => {
    try {
      const value = await proxyRequest("/v1/maintenance");
      if (value.error)
        throw new Error(stringValue(asObject(value.error), "message") || errorMessage(value.error));
      setStatus(value);
      if (clearError) setError("");
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => {
    if (!scanning) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      await load();
      if (!disposed) timer = setTimeout(poll, 1500);
    };
    timer = setTimeout(poll, 1500);
    return () => {
      disposed = true;
      clearTimeout(timer);
    };
  }, [scanning, load]);
  const execute = async (cleanup: boolean) => {
    if (scanning) return;
    if (cleanup && selected.length === 0) return;
    setPending(true);
    setError("");
    try {
      const value = await proxyRequest(
        "/v1/maintenance",
        "POST",
        cleanup
          ? { action: "cleanup", preview_id: preview.preview_id, item_ids: selected }
          : { action: "preview", retention_days: Number(days) },
      );
      if (value.error)
        throw new Error(stringValue(asObject(value.error), "message") || errorMessage(value.error));
      setStatus(value);
      setCleanupSelection({ previewId: "", ids: [] });
      if (cleanup) {
        const refreshed = await proxyRequest("/v1/maintenance", "POST", {
          action: "preview",
          retention_days: Number(days),
        });
        if (refreshed.error) throw new Error(errorMessage(refreshed.error));
        setStatus(refreshed);
      }
    } catch (e) {
      setError(errorMessage(e));
      await load();
    } finally {
      setPending(false);
    }
  };
  const bytes = (value: unknown) =>
    typeof value === "number" ? `${(value / 1024 / 1024).toFixed(1)} MiB` : t("Unknown");
  const disabled =
    pending || scanning || busyAction !== null || snapshot.startup?.available !== true;
  return (
    <section id="maintenance-cleanup">
      <Panel title={t("Data and disk space")} icon={<Package size={18} />}>
        <p>
          {t(
            "Preview disk use and select old files to remove. Harness data, project files, recovery backups, and active versions are protected. No data is moved.",
          )}
        </p>
        <p className="field-help">
          {t(
            "Sizes are logical file sizes. Overlapping directories are shown separately and must not be added together. Unknown means inspection was incomplete.",
          )}
        </p>
        <div className="field-grid">
          <label>
            {t("Keep logs and diagnostics for at least (days)")}
            <input
              type="number"
              min="1"
              max="3650"
              value={days}
              disabled={pending || scanning}
              onChange={(e) => setDays(e.target.value)}
            />
          </label>
        </div>
        <div className="button-row">
          <ActionButton
            disabled={
              disabled || !Number.isInteger(Number(days)) || Number(days) < 1 || Number(days) > 3650
            }
            onClick={() => void execute(false)}
          >
            {pending || scanning ? t("Working…") : t("Preview cleanup")}
          </ActionButton>
        </div>
        {scanning && (
          <p role="status">
            {t(
              "Cleanup preview is scanning in the background. Its saved result will appear automatically; no files are being removed.",
            )}
          </p>
        )}
        {typeof previewScan.wait_message === "string" && (
          <details>
            <summary>{t("Details")}</summary>
            <p>{previewScan.wait_message}</p>
          </details>
        )}
        {previewScan.state === "failed" && typeof previewScan.error === "string" && (
          <p className="form-error" role="alert">
            {previewScan.error}
          </p>
        )}
        {error && <p className="form-error">{error}</p>}
        {!scanning && !arrayValue(preview, "areas").length && (
          <p role="status">
            {t("No current disk preview. Scan to refresh usage and cleanup items.")}
          </p>
        )}
        {!arrayValue(preview, "areas").length && (
          <LiveLogRetention
            value={
              isObject(snapshot.diagnostics?.log_retention)
                ? snapshot.diagnostics.log_retention
                : null
            }
          />
        )}
        <div className="storage-tree">
          {cleanupGroups(
            arrayValue(preview, "areas").map(asObject),
            arrayValue(preview, "items").map(asObject),
          ).map(({ area, items }) => {
            const eligible = items.filter((item) => item.eligible === true);
            const all =
              eligible.length > 0 && eligible.every((item) => selected.includes(String(item.id)));
            const used = result.preview_id === preview.preview_id;
            return (
              <details className="storage-group" key={String(area.path || area.kind)}>
                <summary>
                  <span>{t(String(area.kind))}</span>
                  <span>{bytes(area.bytes)}</span>
                  {!eligible.length && <StatusPill label={t("Protected")} tone="neutral" />}
                </summary>
                <p className="field-help storage-path">{String(area.path || "")}</p>
                {Boolean(area.error) && <p className="form-error">{String(area.error)}</p>}
                {area.kind === "Logs" && (
                  <LiveLogRetention
                    value={
                      isObject(snapshot.diagnostics?.log_retention)
                        ? snapshot.diagnostics.log_retention
                        : null
                    }
                  />
                )}
                {items.length > 0 ? (
                  <>
                    <label className="form-check">
                      <input
                        type="checkbox"
                        checked={all}
                        disabled={disabled || used || !eligible.length}
                        onChange={(event) => {
                          const checked = event.target.checked;
                          setCleanupSelection((current) => ({
                            previewId: String(preview.preview_id),
                            ids: toggleCleanupGroup(
                              cleanupSelectedIds(current, preview.preview_id),
                              items,
                              checked,
                            ),
                          }));
                        }}
                      />
                      <span>{t("Select all removable items")}</span>
                    </label>
                    <div className="storage-children">
                      {items.map((item) => (
                        <label className="storage-item" key={String(item.id)}>
                          <input
                            type="checkbox"
                            checked={selected.includes(String(item.id))}
                            disabled={disabled || used || item.eligible !== true}
                            onChange={(event) => {
                              const checked = event.target.checked;
                              setCleanupSelection((current) => ({
                                previewId: String(preview.preview_id),
                                ids: toggleCleanupGroup(
                                  cleanupSelectedIds(current, preview.preview_id),
                                  [item],
                                  checked,
                                ),
                              }));
                            }}
                          />
                          <span className="storage-item-name">
                            {String(item.name)}
                            <small>{t(String(item.reason || "Can be removed"))}</small>
                          </span>
                          <span>{bytes(item.bytes)}</span>
                          {item.eligible !== true && (
                            <StatusPill label={t("Protected")} tone="neutral" />
                          )}
                        </label>
                      ))}
                    </div>
                  </>
                ) : (
                  <p className="field-help">{t("No removable items in this category")}</p>
                )}
              </details>
            );
          })}
        </div>
        {typeof preview.preview_id === "string" && (
          <>
            <p className="field-help">
              {t(
                "This preview expires after 15 minutes. Changed files are preserved. Stop Harness before cleanup. The newest logs and latest failure diagnostics are always retained.",
              )}
            </p>
            <ActionButton
              tone="danger"
              disabled={
                disabled || selected.length === 0 || result.preview_id === preview.preview_id
              }
              onClick={() => void execute(true)}
            >
              {t("Remove selected files")} ({selected.length})
            </ActionButton>
          </>
        )}
        {result.state ? (
          <>
            <h3>
              {t("Last cleanup result")} · {localizedRuntimeState(result.state, t)}
            </h3>
            <DataList
              items={arrayValue(result, "items")}
              emptyTitle={t("No files selected")}
              emptyDetail=""
              render={(item) => (
                <>
                  <strong>{stringValue(item, "name")}</strong>
                  <span>{localizedRuntimeState(stringValue(item, "state"), t)}</span>
                  {asObject(item).error ? (
                    <p className="form-error">{stringValue(item, "error")}</p>
                  ) : null}
                </>
              )}
            />
          </>
        ) : null}
      </Panel>
    </section>
  );
}
