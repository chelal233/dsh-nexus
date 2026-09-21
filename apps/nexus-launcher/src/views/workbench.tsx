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
import { ActionButton, Metric, Modal, Panel, EmptyState } from "../ui-components";
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
  StopCircle,
  Key,
  ClipboardText,
  RocketLaunch,
  MonitorPlay,
  Globe,
  Desktop,
  ArrowRight,
} from "@phosphor-icons/react";
import { localizedRuntimeState, localizeBackendError, formatTimestamp } from "../display-format";
import { RecoveryLogTail } from "./recovery";
import { useState, useEffect } from "react";
import { harnessUiMatchesRuntime, harnessBrowserReady } from "../harness-session";
import { BrowserHealth, clientStartupLabel, StartupWarning } from "./browser-health";
import { isLoopbackUrl } from "../harness-config";
import { HarnessDesktopPanel, useHarnessDesktop } from "./harness-desktop";

const DESKTOP_PROFILE_NAME = "desktop";

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
  openProfiles,
  openCheckpoints,
  onRepair,
}: ViewProps) {
  const { t, locale } = useI18n();
  const state = nestedValue(snapshot.state, "state");
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const checkpoints = arrayValue(snapshot.checkpoints, "checkpoints");
  const latestCheckpoint = Math.max(
    0,
    ...checkpoints.map((item) => numberValue(item, "created_at_unix") || 0),
  );
  const desktop = useHarnessDesktop(snapshot);
  const [preferredMode, setPreferredMode] = useState<"web" | "desktop">("web");
  const desktopFailed = desktop.state.phase === "failed" || desktop.state.audit?.state === "failed";
  useEffect(() => {
    if (desktopFailed) setPreferredMode("desktop");
  }, [desktopFailed, desktop.state.operationId]);
  const webActive =
    ["running", "starting", "stopping"].includes(stringValue(harness, "state") || "") ||
    !!numberValue(harness, "pid");
  const mode =
    desktop.active || desktop.starting
      ? "desktop"
      : webActive || !desktop.supported
        ? "web"
        : preferredMode;
  const modeLocked =
    webActive ||
    desktop.active ||
    desktop.starting ||
    busyAction !== null ||
    !!snapshot.lifecycleBusy;
  const [failLogOpen, setFailLogOpen] = useState(false);
  const webUrlAvailable =
    harnessUiMatchesRuntime(
      snapshot.harnessRuntime,
      snapshot.harnessUi,
      credentialInvalidationPending,
    ) && !!stringValue(snapshot.harnessUi, "url");
  return (
    <div className="workbench">
      <div className="page-heading">
        <div>
          <h1>{t("Workbench")}</h1>
        </div>
      </div>
      <section
        className="workbench-profile-bar"
        aria-label={t(mode === "desktop" ? "Desktop profile" : "Browser profile")}
      >
        <div>
          <span>{t(mode === "desktop" ? "Desktop profile" : "Browser profile")}</span>
          <strong>
            {mode === "desktop"
              ? DESKTOP_PROFILE_NAME
              : stringValue(snapshot.profiles, "active_profile") ||
                stringValue(state, "profile") ||
                t("None selected")}
          </strong>
        </div>
        {mode === "desktop" && (
          <small>
            {t(
              "Official Desktop uses its own desktop profile. Web profile changes do not apply here.",
            )}
          </small>
        )}
        {mode === "web" && openProfiles && (
          <ActionButton
            disabled={
              desktop.active || desktop.starting || busyAction !== null || !!snapshot.lifecycleBusy
            }
            onClick={openProfiles}
          >
            {t("Switch profile")}
          </ActionButton>
        )}
      </section>
      <section className="harness-launch-surface" aria-label={t("Harness")}>
        <header className="harness-launch-header">
          <div className="harness-product">
            <MonitorPlay size={24} />
            <h2>{t("Harness")}</h2>
          </div>
          <fieldset className="harness-mode-picker" disabled={modeLocked}>
            <legend className="visually-hidden">{t("Open with")}</legend>
            <label>
              <input
                type="radio"
                name="harness-mode"
                value="web"
                checked={mode === "web"}
                onChange={() => setPreferredMode("web")}
              />
              <span>
                <Globe size={16} />
                {t("Browser mode")}
              </span>
            </label>
            {(desktop.supported || desktop.active || desktop.starting) && (
              <>
                <label>
                  <input
                    type="radio"
                    name="harness-mode"
                    value="desktop"
                    checked={mode === "desktop"}
                    onChange={() => setPreferredMode("desktop")}
                  />
                  <span>
                    <Desktop size={16} />
                    {t("Official desktop mode")}
                  </span>
                </label>
              </>
            )}
          </fieldset>
        </header>
        {desktop.probeFailed && (
          <p className="field-help" role="status">
            {t("Desktop support check unavailable")}
          </p>
        )}
        {mode === "web" ? (
          (() => {
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
              <div className="harness-mode-content">
                <Metric
                  label={null}
                  value={
                    harnessState === "running"
                      ? clientStartupLabel(snapshot, t, true)
                      : localizedRuntimeState(harnessState, t)
                  }
                  detail={t(
                    harnessState === "running" && !harnessBrowserReady(snapshot)
                      ? "The browser opens only after startup checks pass. See the check results below."
                      : "Use Harness in your system browser.",
                  )}
                  actions={
                    <>
                      {harnessState === "running" && webUrlAvailable && (
                        <ActionButton
                          tone="primary"
                          disabled={
                            busyAction !== null ||
                            !!snapshot.lifecycleBusy ||
                            !snapshot.startup?.available ||
                            !harnessBrowserReady(snapshot, credentialInvalidationPending)
                          }
                          onClick={() =>
                            void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" })
                          }
                        >
                          <RocketLaunch size={16} />
                          {t("Open in system browser")}
                        </ActionButton>
                      )}
                      {harnessState !== "running" && (
                        <ActionButton
                          tone="primary"
                          disabled={startDisabled}
                          onClick={() => harnessAction("start")}
                        >
                          {t("Start Harness")}
                          <ArrowRight size={16} />
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
                  {openProfiles && <StartupWarning snapshot={snapshot} onDetails={openProfiles} />}
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
              </div>
            );
          })()
        ) : (
          <HarnessDesktopPanel
            snapshot={snapshot}
            busy={busyAction !== null || !!snapshot.lifecycleBusy}
            controller={desktop}
            onRepair={onRepair}
          />
        )}
        {webActive && (
          <p className="harness-switch-hint">{t("Stop Harness Web before launching Desktop.")}</p>
        )}
        <footer className="harness-context-row">
          <details className="harness-source">
            <summary>{t("Version and source")}</summary>
            <p>{activeProgramSource(snapshot, t)}</p>
          </details>
        </footer>
      </section>
      {mode === "web" && (
        <>
          <BrowserHealth
            showLifecycleControls={false}
            snapshot={snapshot}
            busyAction={busyAction}
            runAction={runAction}
            openProfiles={openProfiles}
            onRepair={onRepair}
          />
          {webActive && (
            <details className="workbench-disclosure">
              <summary>{t("Web connection details")}</summary>
              <HarnessAuthPanel
                snapshot={snapshot}
                busyAction={busyAction}
                credentialInvalidationPending={credentialInvalidationPending}
                runAction={runAction}
              />
            </details>
          )}
        </>
      )}
      <div className="workbench-maintenance">
        <Metric
          label={t("Checkpoints")}
          value={String(checkpoints.length)}
          detail={
            latestCheckpoint
              ? t("Latest checkpoint: {time}", {
                  time: formatTimestamp(latestCheckpoint, t("Not available"), locale),
                })
              : checkpoints.length
                ? t("Not available")
                : t("No checkpoints yet")
          }
          actions={
            openCheckpoints && (
              <ActionButton
                disabled={!stringValue(snapshot.profiles, "active_profile")}
                onClick={openCheckpoints}
              >
                {t("View checkpoints")}
              </ActionButton>
            )
          }
        />
      </div>
    </div>
  );
}

function HarnessAuthPanel({
  snapshot,
  busyAction,
  credentialInvalidationPending,
}: HarnessAuthPanelProps) {
  const { locale, t } = useI18n();
  const info = asObject(snapshot.harnessUi);
  const currentUiAvailable = harnessUiMatchesRuntime(
    snapshot.harnessRuntime,
    snapshot.harnessUi,
    credentialInvalidationPending,
  );
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
            stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state") === "stopped"
              ? t("Start Harness and refresh when its loopback URL is ready.")
              : info.message
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
    busyAction !== null ||
    snapshot.startup?.available !== true ||
    !!snapshot.lifecycleBusy ||
    !harnessBrowserReady(snapshot, credentialInvalidationPending);
  const openSystemBrowser = () =>
    void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" });
  return (
    <Panel title={t("Embedded Harness Web")} icon={<MonitorPlay size={18} />}>
      <BrowserHealth snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
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
