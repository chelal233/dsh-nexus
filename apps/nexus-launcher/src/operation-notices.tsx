import { useI18n } from "./i18n";
import { useState, useEffect } from "react";
import { CheckCircle, WarningCircle, Info, X, Pulse } from "@phosphor-icons/react";
import { type Snapshot, type JsonObject, type HarnessPanelProps } from "./app-types";
import { Panel, StatusPill, ActionButton } from "./ui-components";
import { formatTimestamp, localizedRuntimeState } from "./display-format";
import { operationSummaries } from "./operation-status";
import { asObject, booleanValue, stringValue } from "./json-values";
import { recoveryMutationGate } from "./control-state";

export function requiresErrorBanner(code?: string | null): boolean {
  return (
    !!code &&
    /^(config_revision_conflict|harness_preflight_blocked|harness_start_paused|patch_|preferences_invalid|source_invalid|profile_invalid|recovery_|checkpoint_)/.test(
      code,
    )
  );
}

export function ToastNotice({
  message,
  kind,
  onDetails,
}: {
  message: string;
  kind: "success" | "warning" | "info" | "error";
  onDetails?: () => void;
}) {
  const { t } = useI18n();
  const [visible, setVisible] = useState(true);
  const [paused, setPaused] = useState(false);
  useEffect(() => {
    if (paused || !visible) return;
    const timer = window.setTimeout(() => setVisible(false), kind === "error" ? 10000 : 5000);
    return () => window.clearTimeout(timer);
  }, [paused, visible, kind]);
  if (!visible) return null;
  return (
    <div
      className={`toast toast-${kind}`}
      role={kind === "error" ? "alert" : "status"}
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
      onFocus={() => setPaused(true)}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setPaused(false);
      }}
    >
      {kind === "success" ? (
        <CheckCircle size={18} />
      ) : kind === "error" || kind === "warning" ? (
        <WarningCircle size={18} />
      ) : (
        <Info size={18} />
      )}
      <span style={{ whiteSpace: "pre-line" }}>{message}</span>
      {onDetails && <button onClick={onDetails}>{t("Open operation details")}</button>}
      <button onClick={() => setVisible(false)} aria-label={t("Dismiss notice")}>
        <X size={16} />
      </button>
    </div>
  );
}

export function OperationStatusPanel({
  snapshot,
  onOpen,
  attentionOnly = false,
}: {
  attentionOnly?: boolean;
  snapshot: Snapshot;
  onOpen: (module: "guide" | "versions" | "profiles" | "maintenance", anchor: string) => void;
}) {
  const { t, locale } = useI18n();
  const records = operationSummaries(snapshot as unknown as JsonObject).filter(
    (item) =>
      !attentionOnly ||
      [
        "Recovery required",
        "Cleanup required",
        "Package exported; cleanup required",
        "Confirmation required",
        "Installed version unavailable",
      ].includes(item.status),
  );
  if (!records.length) return null;
  const ordered = [...records].sort((a, b) => (b.time || 0) - (a.time || 0));
  return (
    <Panel title={t("Recent activity")} icon={<Pulse size={18} />}>
      <div className="activity-list">
        {ordered.map((item) => (
          <div className="activity-row" key={item.id}>
            <time>
              {item.time !== undefined
                ? formatTimestamp(item.time, t("Not available"), locale)
                : t("Not available")}
            </time>
            <strong>{t(item.title)}</strong>
            <StatusPill
              label={t(item.status)}
              tone={item.error ? "warn" : item.phase === "succeeded" ? "good" : "neutral"}
            />
            {item.error && (
              <p className="activity-message">
                {item.error
                  .split("\n")
                  .map((line) => t(line))
                  .join("\n")}
              </p>
            )}
            {[
              "Recovery required",
              "Cleanup required",
              "Package exported; cleanup required",
              "Confirmation required",
            ].includes(item.status) && (
              <ActionButton onClick={() => onOpen(item.module, item.anchor)}>
                {t("Resolve issue")}
              </ActionButton>
            )}
          </div>
        ))}
      </div>
    </Panel>
  );
}

export function RestoreStatusPanel({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  const recovery = asObject(snapshot.recovery),
    harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(
    booleanValue(recovery, "harness_stop_required"),
    harness.state,
    busyAction !== null,
  );
  const disabled = gate.disabled || snapshot.startup?.available !== true;
  const pending = asObject(
    asObject(snapshot.checkpoints).pending_restore ?? recovery.pending_restore,
  );
  const healthyError = stringValue(snapshot.checkpoints, "healthy_capture_error");
  return (
    <section id="restore-status">
      {healthyError && (
        <div className="notice action-error">
          <WarningCircle size={17} />
          <span>
            {t("Healthy snapshot capture failed")}: {healthyError}
          </span>
        </div>
      )}
      {Object.keys(pending).length > 0 && (
        <Panel title={t("Pending restore")} icon={<WarningCircle size={18} />}>
          <dl className="detail-list compact-details">
            <div>
              <dt>{t("Checkpoint")}</dt>
              <dd>{stringValue(pending, "checkpoint_id")}</dd>
            </div>
            <div>
              <dt>{t("State")}</dt>
              <dd>{localizedRuntimeState(stringValue(pending, "state"), t)}</dd>
            </div>
            <div>
              <dt>{t("Last error")}</dt>
              <dd>{stringValue(pending, "error") || t("None reported")}</dd>
            </div>
          </dl>
          <div className="button-row">
            {booleanValue(pending, "retryable") && (
              <ActionButton
                disabled={disabled}
                onClick={() =>
                  void runAction(t("Retry restore"), "/v1/checkpoints", {
                    action: "retry",
                    id: stringValue(pending, "checkpoint_id"),
                  })
                }
              >
                {t("Retry")}
              </ActionButton>
            )}
            {booleanValue(pending, "abortable") && (
              <ActionButton
                tone="danger"
                disabled={disabled}
                onClick={() =>
                  void runAction(t("Abort restore"), "/v1/checkpoints", {
                    action: "abort",
                    id: stringValue(pending, "checkpoint_id"),
                  })
                }
              >
                {t("Abort")}
              </ActionButton>
            )}
          </div>
        </Panel>
      )}
    </section>
  );
}
