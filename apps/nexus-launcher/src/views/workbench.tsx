import {
  type HarnessPanelProps,
  type Snapshot,
  type ViewProps,
  type HarnessAuthPanelProps,
  type HarnessWebPanelProps,
} from "../app-types";
import {
  booleanValue,
  stringValue,
  asObject,
  nestedValue,
  harnessRuntimeValue,
  arrayValue,
  numberValue,
} from "../json-values";
import { ActionButton, StatusPill, Metric, Modal, Panel, EmptyState } from "../ui-components";
import { useI18n, type Translator } from "../i18n";
import {
  needsHarnessInstall,
  hasHarnessSource,
  externalHarnessRoot,
  harnessControlGate,
} from "../control-state";
import {
  TerminalWindow,
  ArrowsClockwise,
  Info,
  CheckCircle,
  StopCircle,
  Key,
  ClipboardText,
  RocketLaunch,
  MonitorPlay,
} from "@phosphor-icons/react";
import {
  localizedRuntimeState,
  updateStateLabel,
  localizeBackendError,
  formatTimestamp,
} from "../display-format";
import { RecoveryLogTail } from "./recovery";
import { useState, useEffect } from "react";
import { harnessUiMatchesRuntime } from "../harness-session";
import { isLoopbackUrl } from "../harness-config";

export function HarnessTerminalButton({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  const disabled =
    busyAction !== null ||
    !!snapshot.lifecycleBusy ||
    booleanValue(snapshot.health, "degraded") ||
    snapshot.startup?.available !== true ||
    needsHarnessInstall(snapshot.config, snapshot.releases) ||
    !hasHarnessSource(snapshot.config, snapshot.releases) ||
    !stringValue(snapshot.profiles, "active_profile");

  return (
    <ActionButton
      disabled={disabled}
      onClick={() =>
        void runAction(t("Open DSH terminal"), "/v1/profiles", { action: "open_terminal" })
      }
    >
      <TerminalWindow size={16} />
      {t("Open DSH terminal")}
    </ActionButton>
  );
}

export function activeProgramSource(
  snapshot: Pick<Snapshot, "config" | "releases">,
  t: Translator,
): string {
  const external = externalHarnessRoot(snapshot.config);
  if (external) return external;
  const document = asObject(asObject(snapshot.config).config ?? snapshot.config);
  const program = stringValue(asObject(document.harness), "program");
  if (program) return t("Configured command: {program}", { program });
  return stringValue(snapshot.releases, "current_release") || t("Not configured");
}

export function OverviewView({
  snapshot,
  busyAction,
  credentialInvalidationPending,
  runAction,
}: ViewProps) {
  const { t } = useI18n();
  const status = asObject(snapshot.status);
  const health = asObject(snapshot.health);
  const state = nestedValue(snapshot.state, "state");
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const profiles = arrayValue(snapshot.profiles, "profiles");
  const checkpoints = arrayValue(snapshot.checkpoints, "checkpoints");
  const update = nestedValue(snapshot.updates, "update");
  const agentRunning = status.running === true;
  const agentLifecycle = stringValue(state, "lifecycle");
  const agentStarting = agentLifecycle === "starting";
  const agentStopping = agentLifecycle === "stopping";
  const agentControlsUnavailable = busyAction !== null || snapshot.startup === null;
  const agentRestartDisabled = agentControlsUnavailable || agentStarting || agentStopping;
  const [failLogOpen, setFailLogOpen] = useState(false);
  return (
    <>
      <div className="page-heading">
        <div>
          <span className="kicker">{t("Workbench")}</span>
          <h1>{t("Workbench")}</h1>
          <p>
            {t(
              "Service status at a glance: Agent, Harness, active profile, and the Harness web UI.",
            )}
          </p>
        </div>
        <StatusPill
          label={agentRunning ? t("Running") : t("Standby")}
          tone={agentRunning ? "good" : "warn"}
        />
      </div>
      {(() => {
        const harnessState = stringValue(harness, "state");
        const controlGate = harnessControlGate(
          harnessState,
          numberValue(harness, "pid"),
          busyAction !== null,
          snapshot.startup?.available === true,
        );
        const startDisabled =
          controlGate.controlsDisabled ||
          harnessState === "running" ||
          harnessState === "starting" ||
          harnessState === "stopping";
        const restartDisabled =
          controlGate.controlsDisabled ||
          harnessState === "starting" ||
          harnessState === "stopping" ||
          harnessState === "detached";
        const stopDisabled =
          controlGate.controlsDisabled ||
          !["running", "starting", "failed"].includes(harnessState || "");
        const harnessAction = (action: string) =>
          void runAction(t(`Harness ${action}`), "/v1/harness", { action });
        return (
          <div className="metric-grid">
            <Metric
              label={t("Agent lifecycle")}
              value={localizedRuntimeState(stringValue(state, "lifecycle"), t)}
              detail={localizedRuntimeState(stringValue(health, "status"), t)}
              actions={
                <ActionButton
                  tone="primary"
                  disabled={agentRestartDisabled}
                  onClick={() =>
                    void runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })
                  }
                >
                  <ArrowsClockwise size={16} />
                  {t("Force restart Agent")}
                </ActionButton>
              }
            />
            <Metric
              label={
                <>
                  {t("Harness")}{" "}
                  <span className="source-hint">
                    <button
                      type="button"
                      className="icon-button"
                      aria-label={t("Active program source")}
                    >
                      <Info size={16} />
                    </button>
                    <span role="tooltip">
                      {t("Active program source")}: {activeProgramSource(snapshot, t)}
                    </span>
                  </span>
                </>
              }
              value={localizedRuntimeState(harnessState, t)}
              detail={
                stringValue(harness, "pid")
                  ? t("PID {pid}", { pid: stringValue(harness, "pid") || "" })
                  : t("No child process")
              }
              actions={
                <>
                  {harnessState !== "running" && (
                    <ActionButton
                      tone="primary"
                      disabled={startDisabled}
                      onClick={() => harnessAction("start")}
                    >
                      <CheckCircle size={16} />
                      {t("Start")}
                    </ActionButton>
                  )}
                  {(harnessState === "running" || harnessState === "failed") && (
                    <ActionButton
                      disabled={restartDisabled}
                      onClick={() => harnessAction("restart")}
                    >
                      <ArrowsClockwise size={16} />
                      {t("Restart")}
                    </ActionButton>
                  )}
                  {(harnessState === "running" || harnessState === "starting") && (
                    <ActionButton
                      tone="danger"
                      disabled={stopDisabled}
                      onClick={() => harnessAction("stop")}
                    >
                      <StopCircle size={16} />
                      {t("Stop")}
                    </ActionButton>
                  )}
                </>
              }
            >
              {harnessState === "failed" && (
                <div className="button-row">
                  <ActionButton onClick={() => setFailLogOpen(true)}>
                    {t("Show startup log")}
                  </ActionButton>
                </div>
              )}
              {harnessState === "failed" && failLogOpen && (
                <Modal title={t("Startup log tail")} onClose={() => setFailLogOpen(false)}>
                  <RecoveryLogTail snapshot={snapshot} />
                </Modal>
              )}
            </Metric>
            <Metric
              label={t("Active profile")}
              value={stringValue(state, "profile") || t("None selected")}
              detail={t("{count} profiles available", { count: profiles.length })}
            />
            <Metric
              label={t("Checkpoints")}
              value={String(checkpoints.length)}
              detail={updateStateLabel(update, t)}
            />
          </div>
        );
      })()}
      <HarnessAuthPanel
        snapshot={snapshot}
        busyAction={busyAction}
        credentialInvalidationPending={credentialInvalidationPending}
        runAction={runAction}
      />
      <HarnessWebPanel
        snapshot={snapshot}
        credentialInvalidationPending={credentialInvalidationPending}
        busyAction={busyAction}
        runAction={runAction}
      />
    </>
  );
}

function HarnessAuthPanel({
  snapshot,
  busyAction,
  credentialInvalidationPending,
  runAction,
}: HarnessAuthPanelProps) {
  const { locale, t } = useI18n();
  const info = asObject(snapshot.harnessUi);
  const currentUiAvailable = harnessUiMatchesRuntime(
    snapshot.harnessRuntime,
    snapshot.harnessUi,
    credentialInvalidationPending,
  );
  const uiUrl = currentUiAvailable ? stringValue(info, "url") : undefined;
  const token = currentUiAvailable ? stringValue(info, "token") : undefined;
  const sessionKey = currentUiAvailable
    ? `${numberValue(info, "generation")}:${stringValue(info, "run_id")}`
    : undefined;
  const [revealedSessionKey, setRevealedSessionKey] = useState<string | undefined>(undefined);
  const showToken = sessionKey !== undefined && revealedSessionKey === sessionKey;
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  useEffect(() => {
    setRevealedSessionKey((revealed) => (revealed === sessionKey ? revealed : undefined));
  }, [sessionKey]);
  const openSystemBrowser = () =>
    void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" });
  return (
    <Panel title={t("Authentication metadata")} icon={<Key size={18} />}>
      {token ? (
        <>
          <label className="field-label" htmlFor="harness-token">
            {t("Latest loopback token")}
          </label>
          <div className="token-row">
            <input
              id="harness-token"
              readOnly
              type={showToken ? "text" : "password"}
              value={token}
              aria-describedby="token-help"
            />
            <button
              className="button subtle"
              onClick={() => setRevealedSessionKey(showToken ? undefined : sessionKey)}
            >
              {showToken ? t("Hide") : t("Reveal")}
            </button>
          </div>
          <p className="field-help" id="token-help">
            {t(
              "Read from a bounded Nexus-owned Harness log tail. It is not written to Nexus state.",
            )}
          </p>
        </>
      ) : (
        <EmptyState
          title={t("No token observed")}
          detail={
            info.message
              ? localizeBackendError(stringValue(info, "message") || "", t)
              : t("Start Harness and refresh when its loopback URL is ready.")
          }
        />
      )}
      <div className="metadata-grid">
        <div>
          <span>{t("Source")}</span>
          <strong>{stringValue(info, "source") || t("Not available")}</strong>
        </div>
        <div>
          <span>{t("Observed")}</span>
          <strong>
            {formatTimestamp(numberValue(info, "observed_at_unix"), t("Not available"), locale)}
          </strong>
        </div>
      </div>
      <div className="button-row">
        <ActionButton
          disabled={!token || controlsDisabled}
          onClick={() => void navigator.clipboard?.writeText(token || "")}
        >
          <ClipboardText size={16} />
          {t("Copy token")}
        </ActionButton>
        <ActionButton
          tone="primary"
          disabled={!uiUrl || controlsDisabled}
          onClick={openSystemBrowser}
        >
          <RocketLaunch size={16} />
          {t("Open in system browser")}
        </ActionButton>
      </div>
    </Panel>
  );
}

export function HarnessWebPanel({
  snapshot,
  credentialInvalidationPending,
  busyAction,
  runAction,
}: HarnessWebPanelProps) {
  const { t } = useI18n();
  const info = asObject(snapshot.harnessUi);
  const currentUiAvailable = harnessUiMatchesRuntime(
    snapshot.harnessRuntime,
    snapshot.harnessUi,
    credentialInvalidationPending,
  );
  const uiUrl = currentUiAvailable ? stringValue(info, "url") : undefined;
  const tokenMode = stringValue(info, "token") !== undefined;
  const safeUrl = isLoopbackUrl(uiUrl) ? uiUrl : undefined;
  const browserActionDisabled =
    busyAction !== null || snapshot.startup?.available !== true || !currentUiAvailable;
  const openSystemBrowser = () =>
    void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" });
  return (
    <Panel title={t("Embedded Harness Web")} icon={<MonitorPlay size={18} />}>
      {tokenMode ? (
        <div className="status-block">
          <strong>{t("Harness authentication requires a system browser")}</strong>
          <span>
            {t(
              "This session needs top-level browser authentication. Open the validated Harness page in your system browser to sign in.",
            )}
          </span>
          <div className="button-row">
            <ActionButton
              tone="primary"
              disabled={browserActionDisabled}
              onClick={openSystemBrowser}
            >
              <RocketLaunch size={16} />
              {t("Open in system browser")}
            </ActionButton>
          </div>
        </div>
      ) : safeUrl ? (
        <iframe
          className="harness-frame"
          title={t("Harness Web interface")}
          src={safeUrl}
          referrerPolicy="no-referrer"
          sandbox="allow-forms allow-scripts allow-same-origin allow-downloads allow-popups allow-popups-to-escape-sandbox"
        />
      ) : (
        <EmptyState
          title={t("Harness view is not ready")}
          detail={t(
            "A validated loopback HTTP URL will appear here when Harness reports its web interface.",
          )}
        />
      )}
    </Panel>
  );
}
