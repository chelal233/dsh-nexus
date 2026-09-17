import { BusyOverlay } from "./confirmation";
import {
  type IconProps,
  RocketLaunch,
  House,
  PuzzlePiece,
  Bell,
  MonitorPlay,
  Info,
  Package,
  SlidersHorizontal,
  Pulse,
  Gear,
  ShieldCheck,
  ArrowsClockwise,
  WarningCircle,
  X,
  Cpu,
  Key,
} from "@phosphor-icons/react";
import { type ModuleId, type Snapshot, type ThemeMode, type JsonObject } from "./app-types";
import { proxyRequest, commandStartupStatus, isBrowserPreview } from "./agent-bridge";
import {
  stringValue,
  asObject,
  harnessRuntimeValue,
  booleanValue,
  numberValue,
} from "./json-values";
import { errorMessage, localizeBackendError, isRecoverableNoopError } from "./display-format";
import { credentialInvalidationCanSettle, harnessSessionKey } from "./harness-session";
import {
  currentStartupFailure,
  GuideView,
  StartupOperationPanel,
  CompatibilityDialog,
} from "./views/startup";
import { ReadOnlyRecoveryView, RecoveryModePanel } from "./views/recovery";
import { UpdatesView } from "./views/updates";
import { BuiltinPluginsView } from "./views/market";
import { ProfilesView } from "./views/profiles";
import { MaintenanceView } from "./views/maintenance";
import { OperationStatusPanel, ToastNotice, requiresErrorBanner } from "./operation-notices";
import { SettingsView } from "./views/settings";
import { OverviewView, HarnessTerminalButton } from "./views/workbench";
import {
  ActionButton,
  StatusPill,
  AgentUnavailableNotice,
  MissingReleaseNotice,
  DegradedNotice,
  ErrorState,
  LoadingState,
} from "./ui-components";
import { useRef, useState, useEffect, useCallback, useMemo } from "react";
import { createDraftMemory, DraftMemoryContext } from "./draft-memory";
import { sharedRequestClient, requiresRequestId } from "./request-client";
import { useI18n } from "./i18n";
import {
  createFailureNoticeTracker,
  startupRepairTarget,
  isLifecycleBusyError,
  coldOperationIsTerminal,
  lifecycleBusySnapshot,
  failClosedSnapshot,
  launcherPollDelay,
  needsHarnessInstall,
  invalidatesHarnessCredentials,
  actionNoticeKey,
  externalHarnessRoot,
  validStartupCheck,
  isMissingHarnessError,
  harnessControlGate,
  hasHarnessSource,
  launcherContentMode,
} from "./control-state";
import {
  advanceOperationNotices,
  operationSummaries,
  operationResponseNotice,
  operationNoticeKind,
} from "./operation-status";
import { invoke } from "./desktop";
import { useDesktopUpdate } from "./desktop-update";
import { notificationsEnabledPreference, notify } from "./notifications";
import { listen } from "./desktop";
import { setDisplayZoom, displayZoom, ZOOM_LEVELS } from "./display-preferences";
import { diagnosticExportResult, harnessFailureKeys } from "./settings-state";
import { flushSync } from "react-dom";
import { apiErrorInfo, errorWithExplanation } from "./api-errors";

type IconComponent = React.ComponentType<IconProps>;

type ModuleDefinition = {
  id: ModuleId;
  label: string;
  icon: IconComponent;
};

const modules: ModuleDefinition[] = [
  { id: "guide", label: "Setup guide", icon: RocketLaunch },
  { id: "workbench", label: "Workbench", icon: House },
  { id: "versions", label: "Updates", icon: Package },
  { id: "profiles", label: "Profiles and plugins", icon: SlidersHorizontal },
  { id: "plugins", label: "Built-in plugins", icon: PuzzlePiece },
  { id: "maintenance", label: "Maintenance", icon: Pulse },
  { id: "settings", label: "Settings", icon: Gear },
];

const emptySnapshot: Snapshot = {
  startup: null,
  endpointErrors: {},
  status: null,
  health: null,
  state: null,
  harnessRuntime: null,
  harnessUi: null,
  profiles: null,
  checkpoints: null,
  releases: null,
  updates: null,
  diagnostics: null,
  maintenance: null,
  recovery: null,
  config: null,
};

type SnapshotEndpoint = Exclude<
  keyof Snapshot,
  "startup" | "status" | "endpointErrors" | "endpointFailures" | "lifecycleBusy"
>;

const endpointMap: Record<SnapshotEndpoint, string> = {
  health: "/v1/health",
  state: "/v1/state",
  harnessRuntime: "/v1/harness",
  // The Agent owns Harness URL/token observation. Older Agents may not expose
  // this optional endpoint, in which case the UI remains credential-closed.
  harnessUi: "/v1/harness/ui",
  profiles: "/v1/profiles",
  checkpoints: "/v1/checkpoints",
  releases: "/v1/releases",
  updates: "/v1/updates",
  diagnostics: "/v1/diagnostics",
  maintenance: "/v1/maintenance",
  recovery: "/v1/recovery",
  config: "/v1/config",
};

function storedTheme(): ThemeMode {
  try {
    const value = window.localStorage.getItem("nexus.launcher.theme");
    return value === "light" || value === "dark" || value === "system" ? value : "system";
  } catch {
    return "system";
  }
}

function systemTheme(): "light" | "dark" {
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

function App() {
  const topbarRef = useRef<HTMLElement>(null);
  useEffect(() => {
    const bar = topbarRef.current;
    if (!bar) return;
    const measure = () =>
      bar.parentElement?.style.setProperty("--topbar-height", `${bar.offsetHeight}px`);
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(bar);
    return () => observer.disconnect();
  }, []);
  const draftMemory = useRef(createDraftMemory());
  const draftRoot = useRef("unresolved");
  const postAction = (path: string, body: JsonObject) => {
    if (!requiresRequestId(path, body)) return proxyRequest<JsonObject>(path, "POST", body);
    const root = stringValue(snapshot.startup, "data_root_id") || "";
    return sharedRequestClient(
      window.localStorage,
      (route, method, payload) => proxyRequest<JsonObject>(route, method, payload),
      root,
    ).post(path, body);
  };
  const { locale, t } = useI18n();
  const [activeModule, setActiveModule] = useState<ModuleId>("workbench");
  const [repairReturn, setRepairReturn] = useState<{ module: ModuleId; modal: boolean } | null>(
    null,
  );
  const [recheckEpoch, setRecheckEpoch] = useState(0);
  const [activeSettingsSection, setActiveSettingsSection] = useState("display");
  const [repairSection, setRepairSection] = useState<{ section: string; id: number } | undefined>();

  const [checkpointFocus, setCheckpointFocus] = useState<{ profile: string; id: number }>();
  const [operationAnchor, setOperationAnchor] = useState<string | null>(null);
  useEffect(() => {
    if (operationAnchor) {
      document.getElementById(operationAnchor)?.scrollIntoView({ block: "start" });
      setOperationAnchor(null);
    }
  }, [activeModule, operationAnchor]);
  const [themeMode, setThemeMode] = useState<ThemeMode>(storedTheme);
  const [systemThemeMode, setSystemThemeMode] = useState<"light" | "dark">(systemTheme);
  const [snapshot, setSnapshot] = useState<Snapshot>(emptySnapshot);
  const [loading, setLoading] = useState(true);
  const [controlPlaneReady, setControlPlaneReady] = useState(false);
  const [error, setErrorMessage] = useState<string | null>(null);
  const [errorSequence, setErrorSequence] = useState(0);
  const setError = useCallback((message: string | null) => {
    setErrorMessage(message);
    setErrorSequence((value) => value + 1);
  }, []);
  const [errorGuidance, setErrorGuidance] = useState<{
    message: string;
    actions: string[];
    code?: string | null;
  } | null>(null);
  const [bridgeError, setBridgeError] = useState<string | null>(null);
  const [agentUnavailable, setAgentUnavailable] = useState<string | null>(null);
  const [checkOpen, setCheckOpen] = useState(false);
  const [checkPending, setCheckPending] = useState(false);
  const [basicCheckResult, setBasicCheckResult] = useState<JsonObject | null>(null);
  const [basicCheckError, setBasicCheckError] = useState("");
  const checkEvents = useRef(createFailureNoticeTracker());
  const [notice, setNoticeMessage] = useState<string | null>(null);
  const [noticeKind, setNoticeKind] = useState<"success" | "warning" | "info">("info");
  const [noticeSequence, setNoticeSequence] = useState(0);
  const setNotice = useCallback(
    (message: string | null, kind: "success" | "warning" | "info" = "info") => {
      setNoticeMessage(message);
      setNoticeKind(kind);
      setNoticeSequence((value) => value + 1);
    },
    [],
  );
  const previousOperations = useRef(new Map<string, boolean>());
  useEffect(() => {
    if (!snapshot.startup?.available) return;
    const { pending, notices } = advanceOperationNotices(
      operationSummaries(snapshot as unknown as JsonObject),
      previousOperations.current,
    );
    previousOperations.current = pending;
    if (notices.length) {
      setNotice(
        notices.map((item) => `${t(item.title)} · ${t(item.status)}`).join("\n"),
        notices.some((item) => !["Completed", "Cancelled"].includes(item.status))
          ? "warning"
          : notices.every((item) => item.status === "Completed")
            ? "success"
            : "info",
      );
    }
  }, [
    snapshot.updates,
    snapshot.checkpoints,
    snapshot.maintenance,
    snapshot.recovery,
    snapshot.startup?.available,
    snapshot.releases,
    setNotice,
    t,
  ]);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const desktopUpdate = useDesktopUpdate();
  const updateRuntime = harnessRuntimeValue(snapshot.harnessRuntime);
  const updateNeedsHarnessStop =
    stringValue(updateRuntime, "state") === "running" ||
    (numberValue(updateRuntime, "pid") ?? 0) > 0;
  const actionInFlight = useRef(false);
  const [credentialInvalidationPending, setCredentialInvalidationPending] = useState(false);
  const refreshInFlight = useRef<Promise<void> | null>(null);
  const refreshPending = useRef(false);
  const harnessPollState = useRef<string | undefined>(undefined);
  const credentialInvalidation = useRef<{ previousSessionKey: string | undefined } | null>(null);

  useEffect(() => {
    // Keep tray labels and minimize notifications in lockstep with the
    // webview language. The command is intentionally best-effort so the same
    // React bundle remains usable in a browser-only development preview.
    void invoke("set_native_locale", { locale }).catch(() => undefined);
  }, [locale]);

  useEffect(() => {
    // These messages are rendered strings rather than translation keys. Clear
    // them on a language switch so a previous locale never remains visible.
    setNotice(null);
    setError(null);
  }, [locale]);

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: light)");
    const sync = () => setSystemThemeMode(media.matches ? "light" : "dark");
    sync();
    media.addEventListener?.("change", sync);
    return () => media.removeEventListener?.("change", sync);
  }, []);

  useEffect(() => {
    const resolved = themeMode === "system" ? systemThemeMode : themeMode;
    document.documentElement.dataset.theme = resolved;
    try {
      window.localStorage.setItem("nexus.launcher.theme", themeMode);
    } catch {
      // A restricted webview can disable storage. The current choice still applies.
    }
  }, [systemThemeMode, themeMode]);

  const navigateRepair = (id: string) => {
    const target = startupRepairTarget(id);
    setRepairReturn({ module: activeModule, modal: checkOpen });
    setCheckOpen(false);
    setActiveModule(target.module);
    if (target.section) setRepairSection({ section: target.section, id: Date.now() });
  };
  const refresh = useCallback(async () => {
    if (refreshInFlight.current) {
      refreshPending.current = true;
      await refreshInFlight.current;
      return;
    }
    let drain!: Promise<void>;
    drain = (async () => {
      try {
        do {
          refreshPending.current = false;
          setLoading(true);
          try {
            const startup = await commandStartupStatus();
            setControlPlaneReady(startup.available);
            const next: Snapshot = {
              ...emptySnapshot,
              startup,
              status: startup,
            };
            if (!startup.available) {
              harnessPollState.current = undefined;
              setSnapshot(next);
              setNotice(null);
              // The native Electron shell is still alive when the independent
              // Agent is offline. Keep the workspace visible so Settings and
              // Diagnostics remain useful instead of showing a false bridge
              // failure page.
              setBridgeError(null);
              setAgentUnavailable(
                startup.message || t("Set NEXUS_AGENT_BIN or build the Rust Agent."),
              );
              continue;
            }
            setBridgeError(null);
            setAgentUnavailable(null);
            const endpointErrors: Record<string, string> = {};
            const endpointFailures: NonNullable<Snapshot["endpointFailures"]> = {};
            let lifecycleBusy = false;
            // These reads share the Agent lifecycle gate. Serialize them within a
            // refresh so our own reads do not look like an active mutation.
            const lifecycleReads = new Set([
              "state",
              "harnessRuntime",
              "harnessUi",
              "profiles",
              "checkpoints",
              "releases",
              "recovery",
            ]);
            let readQueue: Promise<unknown> = Promise.resolve();
            const entries = await Promise.all(
              Object.entries(endpointMap).map(([key, path]) => {
                const read = async () => {
                  try {
                    const value = await proxyRequest<Snapshot[SnapshotEndpoint]>(path);
                    const warnings = asObject(value).warnings;
                    if (path === "/v1/profiles" && Array.isArray(warnings)) {
                      const messages = warnings.filter(
                        (item): item is string => typeof item === "string",
                      );
                      if (messages.length) endpointErrors[path] = messages.join("\n");
                    }
                    return [key as SnapshotEndpoint, value] as const;
                  } catch (cause) {
                    const message = errorMessage(cause);
                    if (isLifecycleBusyError(message)) lifecycleBusy = true;
                    else {
                      endpointErrors[path] = message;
                      endpointFailures[path] = apiErrorInfo(cause);
                    }
                    return [key as SnapshotEndpoint, null] as const;
                  }
                };
                if (!lifecycleReads.has(key)) return read();
                const pending = readQueue.then(read);
                readQueue = pending;
                return pending;
              }),
            );
            Object.assign(next, Object.fromEntries(entries), { endpointErrors, endpointFailures });
            const coldPhase = stringValue(asObject(asObject(next.updates).operation), "phase");
            harnessPollState.current =
              lifecycleBusy || (!!coldPhase && !coldOperationIsTerminal(coldPhase))
                ? "busy"
                : stringValue(harnessRuntimeValue(next.harnessRuntime), "state");
            setSnapshot((previous) => {
              if (!lifecycleBusy) return next;
              const instance = stringValue(next.health, "instance_id");
              const root = stringValue(next.health, "data_root_id");
              const sameOwner =
                !!instance &&
                !!root &&
                instance === stringValue(previous.health, "instance_id") &&
                root === stringValue(previous.health, "data_root_id");
              return lifecycleBusySnapshot(next, previous, sameOwner);
            });
            if (
              !lifecycleBusy &&
              credentialInvalidation.current !== null &&
              credentialInvalidationCanSettle(
                next,
                credentialInvalidation.current.previousSessionKey,
              )
            ) {
              credentialInvalidation.current = null;
              setCredentialInvalidationPending(false);
            }
          } catch (cause) {
            setControlPlaneReady(false);
            harnessPollState.current = undefined;
            setSnapshot(failClosedSnapshot(emptySnapshot));
            setNotice(null);
            setAgentUnavailable(null);
            setBridgeError(errorMessage(cause));
          } finally {
            setLoading(false);
          }
        } while (refreshPending.current);
      } finally {
        if (refreshInFlight.current === drain) {
          refreshInFlight.current = null;
        }
      }
    })();
    refreshInFlight.current = drain;
    await drain;
  }, [t]);

  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;
    let failures = 0;
    const poll = async () => {
      await refresh();
      if (cancelled) return;
      const interval = launcherPollDelay(
        harnessPollState.current,
        failures,
        document.visibilityState === "hidden",
      );
      failures = harnessPollState.current === "failed" ? failures + 1 : 0;
      timer = window.setTimeout(() => void poll(), interval);
    };
    void poll();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [refresh]);

  useEffect(() => {
    const wake = () => {
      if (document.visibilityState !== "hidden") void refresh();
    };
    window.addEventListener("focus", wake);
    document.addEventListener("visibilitychange", wake);
    return () => {
      window.removeEventListener("focus", wake);
      document.removeEventListener("visibilitychange", wake);
    };
  }, [refresh]);

  useEffect(() => {
    void invoke("set_native_notifications", { enabled: notificationsEnabledPreference() }).catch(
      () => undefined,
    );
    const unlisten = listen<string>("nexus-native-error", (event) => setError(event.payload)).catch(
      () => () => undefined,
    );
    setDisplayZoom(displayZoom());
    const zoomKey = (event: KeyboardEvent) => {
      if (
        !(event.ctrlKey || event.metaKey) ||
        event.altKey ||
        !["+", "=", "-", "0"].includes(event.key)
      )
        return;
      event.preventDefault();
      const current = displayZoom();
      const index = ZOOM_LEVELS.indexOf(current);
      setDisplayZoom(
        event.key === "0"
          ? 100
          : ZOOM_LEVELS[
              Math.max(0, Math.min(ZOOM_LEVELS.length - 1, index + (event.key === "-" ? -1 : 1)))
            ],
      );
    };
    window.addEventListener("keydown", zoomKey);
    return () => {
      window.removeEventListener("keydown", zoomKey);
      void unlisten.then((stop) => stop());
    };
  }, []);

  const retryStartup = useCallback(async () => {
    try {
      if (!isBrowserPreview) {
        await invoke("retry_startup");
      }
    } catch (cause) {
      setBridgeError(errorMessage(cause));
    }
    await refresh();
  }, [refresh]);

  const runAction = useCallback(
    async (label: string, path: string, body: JsonObject): Promise<boolean> => {
      if (busyAction !== null || actionInFlight.current) return false;
      actionInFlight.current = true;
      try {
        const showHarnessInstall = () => {
          setActiveModule("workbench");
          setCheckOpen(false);
          setError(null);
          setNotice(t("No local Harness is installed. Select a version here to install it."));
        };
        if (
          booleanValue(snapshot.health, "degraded") &&
          !(
            (path === "/v1/diagnostics" && body.action === "export") ||
            (path === "/v1/agent" && body.action === "restart")
          )
        ) {
          setError(t("Agent is online in read-only recovery"));
          return false;
        }
        if (
          path === "/v1/diagnostics" &&
          body.action === "export" &&
          snapshot.startup?.available !== true &&
          !isBrowserPreview
        ) {
          setBusyAction(label);
          setError(null);
          setNotice(null);
          try {
            const result = diagnosticExportResult(
              await invoke<JsonObject>("export_startup_diagnostics", {
                observedError: snapshot.startup?.message ?? "Agent is unavailable",
              }),
            );
            setNotice(t("Diagnostic file exported: {path}", { path: result.path }));
            return true;
          } catch (cause) {
            setError(errorMessage(cause));
            return false;
          } finally {
            setBusyAction(null);
          }
        }
        if (snapshot.lifecycleBusy && !(path === "/v1/updates" && body.action === "cancel")) {
          setNotice(
            t(
              "Version or startup operation in progress. Please wait; update progress remains available.",
            ),
          );
          return false;
        }
        const isNativeAgentLifecycle = path === "/v1/agent";
        if (
          snapshot.startup === null ||
          (snapshot.startup.available !== true && !isNativeAgentLifecycle)
        ) {
          setError(t("Launcher controls are disabled until the Agent identity is verified."));
          return false;
        }
        if (
          path === "/v1/profiles" &&
          body.action === "select" &&
          !booleanValue(snapshot.recovery, "paused") &&
          needsHarnessInstall(snapshot.config, snapshot.releases)
        ) {
          showHarnessInstall();
          return false;
        }
        const startsHarness =
          path === "/v1/harness" && ["start", "restart"].includes(String(body.action));
        if (startsHarness) {
          setBasicCheckError("");
          setBasicCheckResult(null);
        }
        const compatibilityAction =
          (path === "/v1/profiles" &&
            ["select", "compatibility_check"].includes(String(body.action))) ||
          (path === "/v1/releases" && ["promote", "rollback"].includes(String(body.action))) ||
          (path === "/v1/updates" &&
            ["switch", "confirm", "offline_import"].includes(String(body.action))) ||
          startsHarness;
        const invalidatesCredentials = invalidatesHarnessCredentials(path, body.action);
        const beginAction = () => {
          setBusyAction(label);
          if (compatibilityAction) setCheckPending(true);
          setNotice(null);
          setError(null);
          if (invalidatesCredentials) {
            credentialInvalidation.current = {
              previousSessionKey: harnessSessionKey(snapshot),
            };
            setCredentialInvalidationPending(true);
            setSnapshot((current) => ({ ...current, harnessUi: null }));
          }
        };
        if (invalidatesCredentials) {
          // Commit removal of credentials and the iframe before the native bridge
          // is allowed to transmit a lifecycle request.
          flushSync(beginAction);
        } else {
          beginAction();
        }
        let failure: {
          message: string;
          original: string;
          cause: unknown;
          info: ReturnType<typeof apiErrorInfo>;
        } | null = null;
        let actionSucceeded = false;
        try {
          const response = await postAction(path, body);
          const receipt = asObject(response.request);
          const exported =
            path === "/v1/diagnostics" && body.action === "export"
              ? diagnosticExportResult(response)
              : null;
          actionSucceeded = receipt.state !== "running";
          if (path === "/v1/config") setSnapshot((current) => ({ ...current, config: response }));
          if (
            path === "/v1/updates" &&
            (response.operation || response.install_operation || body.action === "clear_finished")
          ) {
            setSnapshot((current) => ({ ...current, updates: response }));
          }
          const noticeKey =
            receipt.state === "running"
              ? "The original request is still running. Check its progress before retrying."
              : receipt.state === "completed"
                ? receipt.http_status === 202
                  ? "The original request was accepted. Check the operation for its final result."
                  : "The original request already completed; it was not run again."
                : (operationResponseNotice(path, response) ?? actionNoticeKey(path, body.action));
          const externalVersionOperation =
            !!externalHarnessRoot(snapshot.config) &&
            ((path === "/v1/releases" && ["promote", "rollback"].includes(String(body.action))) ||
              (path === "/v1/updates" && ["switch", "retry"].includes(String(body.action))));
          setNotice(
            externalVersionOperation
              ? t("Version-slot operation accepted. The external program source remains active.")
              : exported
                ? t(
                    exported.manual
                      ? "Diagnostic file exported; open its folder manually: {path}"
                      : "Diagnostic file exported: {path}",
                    { path: exported.path },
                  )
                : noticeKey === "complete"
                  ? `${label} ${t(noticeKey)}`
                  : t(noticeKey),
            operationNoticeKind(noticeKey, exported),
          );
        } catch (cause) {
          const original = errorMessage(cause);
          const info = apiErrorInfo(cause);
          if (info.code === "harness_preflight_blocked") {
            const report = asObject(cause).preflight;
            if (validStartupCheck(report)) setBasicCheckResult(asObject(report));
            else
              setBasicCheckError(
                t("Invalid startup check response. Retry the check or export diagnostics."),
              );
            setCheckOpen(true);
          }
          const explanation =
            info.code === "config_revision_conflict"
              ? t(
                  "Configuration changed elsewhere. Your draft is retained. Cancel edits to load the saved values before trying again.",
                )
              : info.code === "harness_start_paused"
                ? t(
                    "Harness startup is paused. Repair the profile in Recovery, then check it before starting.",
                  )
                : null;
          const message = `${label} ${t("failed")}: ${explanation ? errorWithExplanation(original, explanation, t("Original error")) : localizeBackendError(original, t)}`;
          failure = { message, original, cause, info };
          setErrorGuidance({ message, actions: info.actions, code: info.code });
        } finally {
          // Refresh after both successful and failed POSTs. The Agent may have
          // advanced a generation before returning an error (for example an
          // unattached stop), and the UI must not leave the prior snapshot visible.
          await refresh();
          if (failure) {
            // A rejected no-op (for example Start while Harness is already
            // running) did not cross a lifecycle boundary. Restore the current
            // session instead of leaving the token/iframe locked forever.
            // Match the stable backend English error before localization. The
            // localized text intentionally changes with the selected UI language
            // and must never affect lifecycle/credential state handling.
            if (
              invalidatesCredentials &&
              (isRecoverableNoopError(failure.cause) ||
                failure.info.code === "harness_preflight_blocked")
            ) {
              credentialInvalidation.current = null;
              setCredentialInvalidationPending(false);
            }
            if (failure.original && !failure.info.code && isMissingHarnessError(failure.original)) {
              showHarnessInstall();
            } else setError(failure.message);
          }
          setBusyAction(null);
          if (compatibilityAction) setCheckPending(false);
        }
        return actionSucceeded;
      } finally {
        actionInFlight.current = false;
      }
    },
    [busyAction, refresh, snapshot, t],
  );
  useEffect(() => {
    if (isBrowserPreview) return;
    const publish = () => {
      const harness = harnessRuntimeValue(snapshot.harnessRuntime);
      const state = stringValue(harness, "state") || "unknown";
      const operation = asObject(asObject(snapshot.updates).operation);
      const available =
        snapshot.startup?.available === true &&
        !booleanValue(snapshot.health, "degraded") &&
        !snapshot.lifecycleBusy &&
        busyAction === null &&
        !(operation.phase && !coldOperationIsTerminal(operation.phase)) &&
        !operation.cleanup_pending;
      const gate = harnessControlGate(state, numberValue(harness, "pid"), !available, available);
      const installed = !needsHarnessInstall(snapshot.config, snapshot.releases);
      void invoke("update_tray", {
        controls: {
          state: snapshot.startup?.available ? state : "unknown",
          start:
            installed &&
            !gate.controlsDisabled &&
            ["stopped", "failed", "detached"].includes(state),
          stop: !gate.controlsDisabled && ["running", "starting", "failed"].includes(state),
          web: available && state === "running",
          terminal:
            available &&
            installed &&
            hasHarnessSource(snapshot.config, snapshot.releases) &&
            !!stringValue(snapshot.profiles, "active_profile"),
        },
      }).catch(() => undefined);
    };
    publish();
    // Only fresh snapshots renew native state; a stalled poll expires in the tray.
  }, [snapshot, busyAction]);
  const trayActionHandler = useRef<(action: string) => void>(() => {});
  trayActionHandler.current = (action) => {
    if (busyAction !== null) return;
    if (action === "start" || action === "stop")
      void runAction(t(`Harness ${action}`), "/v1/harness", { action });
    else if (action === "web")
      void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" });
    else if (action === "terminal")
      void runAction(t("Open DSH terminal"), "/v1/profiles", { action: "open_terminal" });
  };
  useEffect(() => {
    if (isBrowserPreview) return;
    const unlisten = listen<string>("nexus-tray-action", (event) =>
      trayActionHandler.current(event.payload),
    ).catch(() => () => undefined);
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    if (!snapshot.startup?.available || snapshot.lifecycleBusy) return;
    const report = asObject(asObject(snapshot.profiles).compatibility);
    const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
    const startupError = snapshot.startup.harness_startup_error || "";
    const runtimeError = stringValue(runtime, "error") || "";
    const failed = currentStartupFailure(snapshot);
    const keys = [
      startupError ? `bootstrap:${startupError}` : "",
      failed
        ? `failure:${stringValue(snapshot.harnessRuntime, "log_session_run_id")}:${numberValue(runtime, "updated_at_unix")}:${runtimeError}`
        : "",
      ["failed", "needs_choice"].includes(stringValue(report, "status") || "")
        ? `check:${stringValue(report, "source_profile")}:${stringValue(report, "release_id")}:${numberValue(report, "checked_at_unix")}`
        : "",
    ].filter(Boolean);
    // Wait for a complete initial snapshot; cached failures are history.
    if (!snapshot.profiles || !snapshot.harnessRuntime) return;
    if (!checkEvents.current.observe(keys)) return;
    if (isMissingHarnessError(startupError) || isMissingHarnessError(runtimeError)) {
      setActiveModule("workbench");
      setCheckOpen(false);
      setNotice(t("No local Harness is installed. Select a version here to install it."));
    } else {
      setNotice(null);
      setCheckOpen(true);
    }
  }, [snapshot, t]);

  const launcherStatus = asObject(snapshot.status);
  const isRunning = launcherStatus.running === true;
  const connectionLabel = snapshot.status
    ? isRunning
      ? t("Agent online")
      : t("Agent stopped")
    : t("Bridge offline");
  const connectionTone = snapshot.status ? (isRunning ? "good" : "warn") : "bad";
  const contentMode = launcherContentMode(bridgeError, loading, snapshot.status !== null);

  const failureNotices = useRef(createFailureNoticeTracker());
  useEffect(() => {
    if (!snapshot.harnessRuntime) return;
    const fresh = failureNotices.current.observe(harnessFailureKeys(snapshot.harnessRuntime));
    if (fresh) {
      const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
      const detail = stringValue(runtime, "error");
      const exitCode = runtime.exit_code;
      void notify(
        "Nexus Launcher",
        [
          detail || t("Harness failed to start or crashed. Check the Overview page for details."),
          typeof exitCode === "number" ? `Exit code: ${exitCode}` : "",
        ]
          .filter(Boolean)
          .join(" — "),
      );
    }
  }, [snapshot.harnessRuntime, t]);

  // A disconnect retains only editor memory. Live state remains fail-closed.
  const observedDraftRoot = stringValue(snapshot.startup, "data_root_id");
  if (observedDraftRoot) draftRoot.current = observedDraftRoot;
  const content = useMemo(() => {
    const common = {
      snapshot,
      actionPending: busyAction !== null,
      busyAction: busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null),
      credentialInvalidationPending,
      runAction,
      refresh,
      themeMode,
      setThemeMode,
      openSettings: () => {
        setActiveModule("settings");
        setRepairSection({ section: "harness", id: Date.now() });
      },
      openWorkbench: () => setActiveModule("workbench"),
      openProfiles: () => {
        setCheckpointFocus(undefined);
        setActiveModule("profiles");
        window.scrollTo({ top: 0, behavior: "instant" });
      },
      openCheckpoints: () => {
        const profile = stringValue(snapshot.profiles, "active_profile");
        setCheckpointFocus(profile ? { profile, id: Date.now() } : undefined);
        setActiveModule("profiles");
      },
      checkpointFocus,
      onRepair: navigateRepair,
      recheckEpoch,
      repairSection,
      onSettingsSectionChange: setActiveSettingsSection,
    };
    if (booleanValue(snapshot.health, "degraded")) return <ReadOnlyRecoveryView {...common} />;
    switch (activeModule) {
      case "guide":
        return <GuideView {...common} />;
      case "versions":
        return <UpdatesView {...common} />;
      case "plugins":
        return <BuiltinPluginsView {...common} />;
      case "profiles":
        return <ProfilesView {...common} />;
      case "maintenance":
        return (
          <MaintenanceView
            {...common}
            activity={
              <OperationStatusPanel
                snapshot={snapshot}
                onOpen={(module, anchor) => {
                  setActiveModule(module);
                  setOperationAnchor(anchor);
                }}
              />
            }
          />
        );
      case "settings":
        return <SettingsView {...common} />;
      default:
        return <OverviewView {...common} />;
    }
  }, [
    activeModule,
    busyAction,
    credentialInvalidationPending,
    refresh,
    runAction,
    snapshot,
    t,
    themeMode,
    repairSection,
    recheckEpoch,
    checkpointFocus,
  ]);

  return (
    <DraftMemoryContext.Provider
      key={draftRoot.current}
      value={{
        store: draftMemory.current,
        scope: JSON.stringify([draftRoot.current, activeModule]),
      }}
    >
      <div className="app-shell">
        <aside className="sidebar" aria-label={t("Nexus modules")}>
          <div className="brand-lockup">
            <div className="brand-mark" aria-hidden="true">
              <RocketLaunch size={20} weight="fill" />
            </div>
            <div className="brand-copy">
              <strong>{t("NEXUS")}</strong>
              <span>{t("LOCAL CONTROL")}</span>
            </div>
          </div>
          <nav className="module-nav">
            {[
              modules.filter((item) => item.id === "guide"),
              modules.filter((item) => item.id !== "guide"),
            ].map((group, index) => (
              <div className={`nav-group ${index === 0 ? "nav-group-setup" : ""}`} key={index}>
                {group.map(({ id, label, icon: Icon }) => (
                  <button
                    className={`nav-item ${activeModule === id ? "active" : ""}`}
                    key={id}
                    disabled={booleanValue(snapshot.health, "degraded")}
                    onClick={() => {
                      flushSync(() => setActiveModule(id));
                      window.scrollTo({ top: 0, behavior: "instant" });
                    }}
                    aria-current={activeModule === id ? "page" : undefined}
                    title={t(label)}
                  >
                    <Icon
                      size={19}
                      weight={activeModule === id ? "fill" : "regular"}
                      aria-hidden="true"
                    />
                    <span>{t(label)}</span>
                  </button>
                ))}
              </div>
            ))}
            {activeModule === "settings" && (
              <div className="settings-subnav" aria-label={t("Settings sections")}>
                {[
                  { id: "display", label: "Appearance and display", icon: MonitorPlay },
                  { id: "harness", label: "Harness configuration", icon: SlidersHorizontal },
                  { id: "runtime", label: "Runtime and launch", icon: Cpu },
                  { id: "notifications", label: "Notifications", icon: Bell },
                  { id: "repair", label: "Repair & reset", icon: ArrowsClockwise },
                  { id: "application", label: "Application and about", icon: Info },
                ].map(({ id, label, icon: Icon }) => (
                  <button
                    type="button"
                    key={id}
                    className="settings-subitem"
                    title={t(label)}
                    aria-label={t(label)}
                    aria-controls={`settings-${id}`}
                    aria-current={activeSettingsSection === id ? "page" : undefined}
                    disabled={booleanValue(snapshot.health, "degraded")}
                    onClick={() => {
                      setRepairSection({ section: id, id: Date.now() });
                      window.scrollTo({ top: 0, behavior: "instant" });
                    }}
                  >
                    <Icon size={16} aria-hidden="true" />
                    <span>{t(label)}</span>
                  </button>
                ))}
              </div>
            )}
          </nav>
          <div className="sidebar-footer">
            <ShieldCheck size={16} />
            <span>{t("Loopback only")}</span>
            {desktopUpdate &&
              ["available", "downloading", "ready", "installing"].includes(desktopUpdate.phase) && (
                <button
                  className="button secondary small"
                  title={t(
                    updateNeedsHarnessStop
                      ? "Stop Harness before updating; running tasks will be interrupted."
                      : "Update and restart",
                  )}
                  disabled={desktopUpdate.phase !== "ready"}
                  onClick={() => {
                    if (updateNeedsHarnessStop) {
                      setError(
                        t("Stop Harness before updating; running tasks will be interrupted."),
                      );
                      return;
                    }
                    void invoke("update_install").catch((error) =>
                      setError(error.message || String(error)),
                    );
                  }}
                >
                  {desktopUpdate.phase === "ready"
                    ? t("Update")
                    : desktopUpdate.phase === "installing"
                      ? t("Installing")
                      : t("Downloading {percent}%", {
                          percent: Math.min(
                            100,
                            Math.max(0, Math.floor(desktopUpdate.percent ?? 0)),
                          ),
                        })}
                </button>
              )}
            {desktopUpdate &&
              updateNeedsHarnessStop &&
              ["available", "downloading", "ready"].includes(desktopUpdate.phase) && (
                <span className="desktop-update-warning">
                  {t("Stop Harness before updating; running tasks will be interrupted.")}
                </span>
              )}
          </div>
        </aside>

        <main className="workspace">
          <header className="topbar" ref={topbarRef}>
            <div className="breadcrumbs">
              <span>{t("Nexus Launcher")}</span>
              <span className="crumb-separator">/</span>
              <strong>
                {t(modules.find((item) => item.id === activeModule)?.label || "Overview")}
              </strong>
            </div>
            <div className="topbar-actions">
              <HarnessTerminalButton
                snapshot={snapshot}
                busyAction={busyAction}
                runAction={runAction}
              />
              {snapshot.startup?.available && !booleanValue(snapshot.health, "degraded") && (
                <RecoveryModePanel
                  snapshot={snapshot}
                  busyAction={
                    busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null)
                  }
                  runAction={runAction}
                  compact
                />
              )}
              <ActionButton
                disabled={booleanValue(snapshot.health, "degraded")}
                onClick={() => setCheckOpen(true)}
              >
                {t("Startup compatibility check")}
              </ActionButton>
              {snapshot.startup?.available !== true && !isBrowserPreview && (
                <ActionButton
                  disabled={busyAction !== null}
                  onClick={() =>
                    void runAction(t("Export diagnostics"), "/v1/diagnostics", { action: "export" })
                  }
                >
                  {t("Export diagnostics")}
                </ActionButton>
              )}
              <StatusPill label={connectionLabel} tone={connectionTone} />
              <button
                className="icon-button"
                onClick={() => void refresh()}
                aria-label={t("Refresh launcher status")}
                title={t("Refresh launcher status")}
              >
                <ArrowsClockwise size={19} />
              </button>
            </div>
          </header>
          {booleanValue(snapshot.health, "degraded") && (
            <section className="notice action-error" role="alert">
              <WarningCircle size={18} />
              <div>
                <strong>{t("Agent is online in read-only recovery")}</strong>
                <p>{stringValue(snapshot.health, "recovery_reason")}</p>
                <p>
                  {t(
                    "Choose a valid recovery time to restore Nexus records. If no supported recovery point is available, export diagnostics. Normal editing and Harness startup remain blocked.",
                  )}
                </p>
              </div>
              <ActionButton
                disabled={busyAction !== null}
                onClick={() =>
                  void runAction(t("Export diagnostics"), "/v1/diagnostics", { action: "export" })
                }
              >
                {t("Export diagnostics")}
              </ActionButton>
            </section>
          )}

          <div className="toast-stack">
            {notice && (
              <ToastNotice key={`notice:${noticeSequence}`} message={notice} kind={noticeKind} />
            )}
            {error &&
              contentMode !== "error" &&
              !requiresErrorBanner(
                errorGuidance?.message === error ? errorGuidance?.code : null,
              ) && (
                <ToastNotice
                  key={`error:${errorSequence}`}
                  message={error}
                  kind="error"
                  onDetails={() => setActiveModule("maintenance")}
                />
              )}
          </div>
          {error &&
            contentMode !== "error" &&
            (activeModule === "maintenance" ||
              requiresErrorBanner(
                errorGuidance?.message === error ? errorGuidance?.code : null,
              )) && (
              <div className="notice action-error" role="alert">
                <WarningCircle size={17} />
                <span style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{error}</span>
                <button
                  onClick={() =>
                    void navigator.clipboard
                      .writeText(error)
                      .catch(() => setNotice(t("Select the error text and copy it manually.")))
                  }
                >
                  {t("Copy error")}
                </button>
                {errorGuidance?.message === error &&
                  errorGuidance.actions.includes("open_settings") && (
                    <button onClick={() => setActiveModule("settings")}>{t("Settings")}</button>
                  )}
                {errorGuidance?.message === error &&
                  errorGuidance.actions.includes("enter_recovery") && (
                    <button onClick={() => setActiveModule("maintenance")}>{t("Recovery")}</button>
                  )}
                <button onClick={() => setError(null)} aria-label={t("Dismiss error")}>
                  <X size={15} />
                </button>
              </div>
            )}
          {agentUnavailable && contentMode !== "error" && (
            <AgentUnavailableNotice
              message={agentUnavailable}
              onRetry={() => void retryStartup()}
            />
          )}
          {repairReturn && (
            <div className="notice">
              <span>
                {t(
                  "Your drafts are retained. Return after fixing the issue to run the check again.",
                )}
              </span>
              <ActionButton
                onClick={() => {
                  setActiveModule(repairReturn.module);
                  setCheckOpen(repairReturn.modal);
                  setRepairReturn(null);
                  setRecheckEpoch((value) => value + 1);
                }}
              >
                {t("Return and recheck")}
              </ActionButton>
            </div>
          )}
          {snapshot.startup?.available && !externalHarnessRoot(snapshot.config) && (
            <MissingReleaseNotice
              releases={asObject(snapshot.releases)}
              onReinstall={() => setActiveModule("guide")}
            />
          )}
          {!booleanValue(snapshot.health, "degraded") &&
            snapshot.startup?.available &&
            (booleanValue(snapshot.recovery, "paused") ||
              stringValue(snapshot.recovery, "pause_error")) && (
              <RecoveryModePanel
                snapshot={snapshot}
                busyAction={busyAction}
                runAction={runAction}
              />
            )}
          {snapshot.lifecycleBusy && (
            <div className="notice" role="status">
              <ArrowsClockwise size={17} />
              <span>
                {t(
                  "Version or startup operation in progress. Showing the last confirmed catalogs; Harness access is temporarily unavailable. Update progress continues to refresh.",
                )}
              </span>
            </div>
          )}
          {!busyAction && !snapshot.lifecycleBusy && (
            <StartupOperationPanel
              available={
                snapshot.startup?.available === true && !booleanValue(snapshot.health, "degraded")
              }
              identity={`${stringValue(snapshot.health, "instance_id")}:${stringValue(snapshot.health, "data_root_id")}`}
            />
          )}
          {!booleanValue(snapshot.health, "degraded") && activeModule !== "maintenance" && (
            <OperationStatusPanel
              snapshot={snapshot}
              attentionOnly
              onOpen={(module, anchor) => {
                setActiveModule(module);
                setOperationAnchor(anchor);
              }}
            />
          )}
          {!error && Object.keys(snapshot.endpointErrors).length > 0 && (
            <DegradedNotice
              errors={snapshot.endpointErrors}
              failures={snapshot.endpointFailures}
              onNavigate={setActiveModule}
              onRetry={() => void refresh()}
              readOnlyRecovery={booleanValue(snapshot.health, "read_only")}
            />
          )}
          {contentMode === "error" ? (
            <ErrorState
              message={bridgeError ?? t("The native bridge is unavailable.")}
              onRetry={() => void retryStartup()}
            />
          ) : contentMode === "loading" ? (
            <LoadingState connected={controlPlaneReady} />
          ) : (
            <section className="page-content">{content}</section>
          )}

          {checkOpen && !booleanValue(snapshot.health, "degraded") && (
            <CompatibilityDialog
              snapshot={snapshot}
              busyAction={
                busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null)
              }
              runAction={runAction}
              pending={checkPending || !!snapshot.lifecycleBusy}
              basicResult={basicCheckResult}
              basicError={basicCheckError}
              onRepair={navigateRepair}
              recheckEpoch={recheckEpoch}
              onClose={() => setCheckOpen(false)}
            />
          )}
          <BusyOverlay
            label={busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null)}
          >
            <StartupOperationPanel
              available={
                snapshot.startup?.available === true && !booleanValue(snapshot.health, "degraded")
              }
              identity={`${stringValue(snapshot.health, "instance_id")}:${stringValue(snapshot.health, "data_root_id")}`}
            />
            {error && (
              <p className="form-error" role="alert">
                {error}
              </p>
            )}
            {snapshot.lifecycleBusy && (
              <>
                <OperationStatusPanel snapshot={snapshot} />
                {stringValue(asObject(asObject(snapshot.updates).operation), "operation_id") &&
                  !coldOperationIsTerminal(
                    stringValue(asObject(asObject(snapshot.updates).operation), "phase"),
                  ) && (
                    <ActionButton
                      disabled={
                        busyAction !== null ||
                        stringValue(asObject(asObject(snapshot.updates).operation), "phase") ===
                          "cancelling"
                      }
                      onClick={() =>
                        void runAction(t("Cancel"), "/v1/updates", {
                          action: "cancel",
                          operation_id: stringValue(
                            asObject(asObject(snapshot.updates).operation),
                            "operation_id",
                          ),
                        })
                      }
                    >
                      {t("Cancel")}
                    </ActionButton>
                  )}
              </>
            )}
          </BusyOverlay>
          <footer className="workspace-footer">
            <span>
              <Cpu size={15} />
              {t("Agent {version}", {
                version: stringValue(snapshot.health, "api_version") || "v1",
              })}
            </span>
            <span>
              <Key size={15} />
              {t("No credentials leave this device")}
            </span>
            {snapshot.startup?.api_base && (
              <span className="api-address">{snapshot.startup.api_base}</span>
            )}
          </footer>
        </main>
      </div>
    </DraftMemoryContext.Provider>
  );
}

export default App;
