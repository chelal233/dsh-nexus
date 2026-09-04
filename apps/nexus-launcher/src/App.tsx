import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import type { IconProps } from "@phosphor-icons/react";
import {
  ArrowClockwise,
  ArrowsClockwise,
  Bell,
  BracketsCurly,
  CheckCircle,
  ClipboardText,
  CloudArrowUp,
  Cpu,
  Gear,
  House,
  Key,
  ListChecks,
  MonitorPlay,
  Package,
  Pulse,
  RocketLaunch,
  ShieldCheck,
  SlidersHorizontal,
  StopCircle,
  TerminalWindow,
  WarningCircle,
  X,
} from "@phosphor-icons/react";
import {
  failClosedSnapshot,
  harnessControlGate,
  invalidatesHarnessCredentials,
  launcherContentMode,
} from "./control-state";
import { useI18n, type Locale } from "./i18n";

type JsonObject = Record<string, unknown>;
type IconComponent = React.ComponentType<IconProps>;

type StartupStatus = {
  available: boolean;
  running: boolean;
  api_base?: string;
  agent_program?: string;
  agent_pid?: number;
  data_root?: string;
  data_root_id?: string;
  instance_id?: string;
  message?: string;
};

type Snapshot = {
  startup: StartupStatus | null;
  endpointErrors: Record<string, string>;
  status: JsonObject | null;
  health: JsonObject | null;
  state: JsonObject | null;
  harnessRuntime: JsonObject | null;
  harnessUi: JsonObject | null;
  profiles: JsonObject | null;
  checkpoints: JsonObject | null;
  releases: JsonObject | null;
  updates: JsonObject | null;
  diagnostics: JsonObject | null;
  config: JsonObject | null;
};

type ModuleId =
  | "overview"
  | "harness"
  | "profiles"
  | "checkpoints"
  | "updates"
  | "diagnostics"
  | "settings";

type ThemeMode = "system" | "light" | "dark";

type ModuleDefinition = {
  id: ModuleId;
  label: string;
  icon: IconComponent;
};

const modules: ModuleDefinition[] = [
  { id: "overview", label: "Overview", icon: House },
  { id: "harness", label: "Harness", icon: MonitorPlay },
  { id: "profiles", label: "Profiles", icon: SlidersHorizontal },
  { id: "checkpoints", label: "Checkpoints", icon: ListChecks },
  { id: "updates", label: "Updates", icon: CloudArrowUp },
  { id: "diagnostics", label: "Diagnostics", icon: TerminalWindow },
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
  config: null,
};

type SnapshotEndpoint = Exclude<keyof Snapshot, "startup" | "status" | "endpointErrors">;

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
  config: "/v1/config",
};

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asObject(value: unknown): JsonObject {
  return isObject(value) ? value : {};
}

function stringValue(value: unknown, key: string): string | undefined {
  const item = asObject(value)[key];
  if (typeof item === "string" && item.trim()) return item;
  if (typeof item === "number" || typeof item === "boolean") return String(item);
  return undefined;
}

function numberValue(value: unknown, key: string): number | undefined {
  const item = asObject(value)[key];
  return typeof item === "number" && Number.isFinite(item) ? item : undefined;
}

function arrayValue(value: unknown, key: string): unknown[] {
  const item = asObject(value)[key];
  return Array.isArray(item) ? item : [];
}

function nestedValue(value: unknown, key: string): JsonObject {
  return asObject(asObject(value)[key]);
}

function harnessRuntimeValue(value: unknown): JsonObject {
  const response = asObject(value);
  const nested = asObject(response.harness);
  return Object.keys(nested).length ? nested : response;
}

function harnessUiMatchesRuntime(
  runtimeValue: unknown,
  uiValue: unknown,
  credentialInvalidationPending = false,
): boolean {
  const response = asObject(runtimeValue);
  const runtime = harnessRuntimeValue(runtimeValue);
  const info = asObject(uiValue);
  return (
    !credentialInvalidationPending &&
    stringValue(runtime, "state") === "running" &&
    info.available === true &&
    numberValue(response, "generation") !== undefined &&
    numberValue(response, "generation") === numberValue(info, "generation") &&
    stringValue(response, "log_session_run_id") !== undefined &&
    stringValue(response, "log_session_run_id") === stringValue(info, "run_id")
  );
}

function harnessSessionKey(snapshot: Snapshot): string | undefined {
  const runtime = asObject(snapshot.harnessRuntime);
  const generation = numberValue(runtime, "generation");
  const runId = stringValue(runtime, "log_session_run_id");
  return generation !== undefined && runId !== undefined ? `${generation}:${runId}` : undefined;
}

export function credentialInvalidationCanSettle(
  snapshot: Snapshot,
  previousSessionKey: string | undefined,
): boolean {
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const state = stringValue(harness, "state");
  const pid = numberValue(harness, "pid");
  if ((state === "stopped" || state === "detached") && pid === undefined) return true;
  if (!harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi)) return false;
  const nextSessionKey = harnessSessionKey(snapshot);
  return previousSessionKey === undefined || nextSessionKey !== previousSessionKey;
}

function formatTimestamp(value: unknown, unavailable = "Not available"): string {
  if (typeof value !== "number" || value <= 0) return unavailable;
  return new Date(value * 1000).toLocaleString();
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "The native bridge returned an unknown error";
}

function localizeBackendError(message: string, t: (key: string) => string): string {
  const normalized = message.toLowerCase();
  if (normalized.includes("harness is not configured")) {
    return t("Harness is not configured. Open Settings to configure it.");
  }
  if (normalized.includes("harness must be stopped")) {
    return t("Harness must be stopped before changing its configuration.");
  }
  if (normalized.includes("harness is detached")) {
    return t("Harness is detached. Configure it in Settings, then start it from the control panel.");
  }
  if (normalized.includes("harness is already running")) {
    return t("Harness is already running; no lifecycle change was made.");
  }
  if (normalized.includes("harness is running but is not attached")) {
    return t("Harness is running outside this Agent. Its status is read-only until it reconnects.");
  }
  if (normalized.includes("harness log session marker is not available")) {
    return t("Harness log session is unavailable. Restart Harness to establish a safe token boundary.");
  }
  if (normalized.includes("harness log session marker is invalid")) {
    return t("Harness log session is invalid. Restart Harness to establish a safe token boundary.");
  }
  if (normalized.includes("current harness authentication token not found")) {
    return t("No current Harness authentication token was found for this run.");
  }
  if (normalized.includes("harness changed state while its token was being observed")) {
    return t("Harness changed state while its token was being observed. Refresh after it is running.");
  }
  if (normalized.includes("agent is not responding at") || normalized.includes("agent api is not responding")) {
    return t("The Agent API is not responding on its loopback port.");
  }
  if (normalized.includes("native bridge returned an unknown error")) {
    return t("The native bridge returned an unknown error");
  }
  return message;
}

function localizedRuntimeState(value: unknown, t: (key: string) => string): string {
  const state = typeof value === "string" ? value.toLowerCase() : "";
  switch (state) {
    case "ok": return t("Healthy");
    case "running": return t("Running");
    case "starting": return t("Starting");
    case "stopping": return t("Stopping");
    case "stopped": return t("Stopped");
    case "failed": return t("Failed");
    case "detached": return t("Detached");
    case "idle": return t("idle");
    case "ready": return t("Ready");
    case "pending": return t("Pending");
    case "queued": return t("Queued");
    case "registered": return t("Registered");
    case "available": return t("Available");
    default: return typeof value === "string" && value ? value : t("Unknown");
  }
}

function updateStateLabel(update: JsonObject, t: (key: string) => string): string {
  const state = stringValue(update, "state");
  return state ? localizedRuntimeState(state, t) : t("Update queue idle");
}

function isRecoverableNoopError(message: string): boolean {
  const normalized = message.toLowerCase();
  return normalized.includes("already running")
    || normalized.includes("already stopped")
    || normalized.includes("not attached")
    || normalized.includes("unattached")
    || normalized.includes("not configured");
}

function compactError(value: string): string {
  return value.length > 180 ? `${value.slice(0, 177)}...` : value;
}

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

function isLoopbackUrl(value: string | undefined): value is string {
  if (!value) return false;
  try {
    const url = new URL(value);
    return (
      url.protocol === "http:" &&
      ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) &&
      !url.username &&
      !url.password &&
      !/[\u0000-\u001f\u007f]/.test(value)
    );
  } catch {
    return false;
  }
}

type HarnessConfigDraft = {
  program: string;
  args: string;
  workingDir: string;
  readinessUrl: string;
  timeout: string;
  argsRedacted: boolean;
  replaceRedactedArgs: boolean;
};

const emptyHarnessDraft: HarnessConfigDraft = {
  program: "",
  args: "",
  workingDir: "",
  readinessUrl: "",
  timeout: "",
  argsRedacted: false,
  replaceRedactedArgs: false,
};

function harnessDraftFromConfig(config: JsonObject): HarnessConfigDraft {
  const harness = nestedValue(config, "harness");
  const args = arrayValue(harness, "args").filter((item): item is string => typeof item === "string");
  const argsRedacted = args.some((item) => item.includes("[REDACTED]"));
  return {
    program: stringValue(harness, "program") || "",
    args: args.join("\n"),
    workingDir: stringValue(harness, "working_dir") || "",
    readinessUrl: stringValue(harness, "readiness_url") || "",
    timeout: numberValue(harness, "readiness_timeout_secs")?.toString() || "",
    argsRedacted,
    replaceRedactedArgs: false,
  };
}

async function proxyRequest<T = JsonObject>(
  path: string,
  method = "GET",
  body?: JsonObject,
): Promise<T> {
  return invoke<T>("proxy_request", {
    method,
    path,
    body: body ?? null,
  });
}

function StatusPill({ label, tone = "neutral" }: { label: string; tone?: "good" | "warn" | "bad" | "neutral" }) {
  return <span className={`status-pill ${tone}`}><span className="status-dot" />{label}</span>;
}

function LoadingState() {
  const { t } = useI18n();
  return (
    <div className="state-card loading-state" role="status" aria-live="polite">
      <Pulse size={22} className="spin" aria-hidden="true" />
      <div><strong>{t("Connecting to Nexus")}</strong><span>{t("Waiting for the local control plane.")}</span></div>
    </div>
  );
}

function EmptyState({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="state-card empty-state">
      <BracketsCurly size={24} aria-hidden="true" />
      <div><strong>{title}</strong><span>{detail}</span></div>
    </div>
  );
}

function ErrorState({ message, onRetry, title }: { message: string; onRetry: () => void; title?: string }) {
  const { t } = useI18n();
  return (
    <div className="state-card error-state" role="alert">
      <WarningCircle size={25} aria-hidden="true" />
      <div className="state-copy"><strong>{title || t("Launcher bridge unavailable")}</strong><span>{localizeBackendError(message, t)}</span></div>
      <button className="button subtle" onClick={onRetry}><ArrowClockwise size={16} />{t("Retry")}</button>
    </div>
  );
}

function DegradedNotice({ errors }: { errors: Record<string, string> }) {
  const { t } = useI18n();
  const details = Object.entries(errors)
    .map(([path, message]) => `${path}: ${compactError(localizeBackendError(message, t))}`)
    .join(" | ");
  return <div className="notice degraded" role="status" aria-live="polite"><WarningCircle size={17} /> <span>{t("Some workspace data is unavailable.")} {details}</span></div>;
}

function AgentUnavailableNotice({ message, onRetry }: { message: string; onRetry: () => void }) {
  const { t } = useI18n();
  return <div className="notice action-error" role="status" aria-live="polite"><WarningCircle size={17} /><span><strong>{t("Agent unavailable")}</strong> {localizeBackendError(message, t)}</span><button className="button subtle" onClick={onRetry}>{t("Retry")}</button></div>;
}

function Metric({ label, value, detail }: { label: string; value: string; detail?: string }) {
  return <div className="metric"><span>{label}</span><strong>{value}</strong>{detail && <small>{detail}</small>}</div>;
}

function ActionButton({
  children,
  onClick,
  disabled = false,
  tone = "default",
  title,
}: {
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
  tone?: "default" | "primary" | "danger";
  title?: string;
}) {
  return <button className={`button ${tone}`} onClick={onClick} disabled={disabled} title={title}>{children}</button>;
}

function DataList({
  items,
  emptyTitle,
  emptyDetail,
  render,
}: {
  items: unknown[];
  emptyTitle: string;
  emptyDetail: string;
  render: (item: unknown, index: number) => React.ReactNode;
}) {
  if (!items.length) return <EmptyState title={emptyTitle} detail={emptyDetail} />;
  return <div className="data-list">{items.map((item, index) => <div className="data-row" key={index}>{render(item, index)}</div>)}</div>;
}

function App() {
  const { t } = useI18n();
  const [activeModule, setActiveModule] = useState<ModuleId>("overview");
  const [themeMode, setThemeMode] = useState<ThemeMode>(storedTheme);
  const [systemThemeMode, setSystemThemeMode] = useState<"light" | "dark">(systemTheme);
  const [snapshot, setSnapshot] = useState<Snapshot>(emptySnapshot);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [bridgeError, setBridgeError] = useState<string | null>(null);
  const [agentUnavailable, setAgentUnavailable] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [credentialInvalidationPending, setCredentialInvalidationPending] = useState(false);
  const refreshInFlight = useRef<Promise<void> | null>(null);
  const refreshPending = useRef(false);
  const harnessPollState = useRef<string | undefined>(undefined);
  const credentialInvalidation = useRef<{ previousSessionKey: string | undefined } | null>(null);

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
          setError(null);
          try {
            const startup = await invoke<StartupStatus>("startup_status");
            const next: Snapshot = {
              ...emptySnapshot,
              startup,
              status: startup as unknown as JsonObject,
            };
            if (!startup.available) {
              harnessPollState.current = undefined;
              setSnapshot(next);
              setNotice(null);
              // The native Tauri shell is still alive when the independent
              // Agent is offline. Keep the workspace visible so Settings and
              // Diagnostics remain useful instead of showing a false bridge
              // failure page.
              setBridgeError(null);
              setAgentUnavailable(startup.message || t("Set NEXUS_AGENT_BIN or build the Rust Agent."));
              continue;
            }
            setBridgeError(null);
            setAgentUnavailable(null);
            const endpointErrors: Record<string, string> = {};
            const entries = await Promise.all(Object.entries(endpointMap).map(async ([key, path]) => {
              try {
                const value = await proxyRequest<JsonObject>(path);
                return [key as SnapshotEndpoint, value] as const;
              } catch (cause) {
                endpointErrors[path] = errorMessage(cause);
                return [key as SnapshotEndpoint, null] as const;
              }
            }));
            for (const [key, value] of entries) next[key] = value;
            next.endpointErrors = endpointErrors;
            harnessPollState.current = stringValue(harnessRuntimeValue(next.harnessRuntime), "state");
            setSnapshot(next);
            if (
              credentialInvalidation.current !== null &&
              credentialInvalidationCanSettle(
                next,
                credentialInvalidation.current.previousSessionKey,
              )
            ) {
              credentialInvalidation.current = null;
              setCredentialInvalidationPending(false);
            }
            if (!next.status && !next.health) {
              setBridgeError(t("The Agent API is not responding on its loopback port."));
            }
          } catch (cause) {
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
    const poll = async () => {
      await refresh();
      if (cancelled) return;
      const interval = harnessPollState.current === "starting" ? 400 : 8000;
      timer = window.setTimeout(() => void poll(), interval);
    };
    void poll();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [refresh]);

  const retryStartup = useCallback(async () => {
    try {
      await invoke("retry_startup");
    } catch (cause) {
      setBridgeError(errorMessage(cause));
    }
    await refresh();
  }, [refresh]);

  const runAction = useCallback(async (label: string, path: string, body: JsonObject): Promise<boolean> => {
    if (snapshot.startup?.available !== true) {
      setError(t("Launcher controls are disabled until the Agent identity is verified."));
      return false;
    }
    const invalidatesCredentials = invalidatesHarnessCredentials(path, body.action);
    const beginAction = () => {
      setBusyAction(label);
      setNotice(null);
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
    let actionError: string | null = null;
    let actionSucceeded = false;
    try {
      await proxyRequest(path, "POST", body);
      actionSucceeded = true;
      setNotice(`${label} ${t("complete")}`);
    } catch (cause) {
      actionError = `${label} ${t("failed")}: ${localizeBackendError(errorMessage(cause), t)}`;
    } finally {
      // Refresh after both successful and failed POSTs. The Agent may have
      // advanced a generation before returning an error (for example an
      // unattached stop), and the UI must not leave the prior snapshot visible.
      await refresh();
      if (actionError) {
        // A rejected no-op (for example Start while Harness is already
        // running) did not cross a lifecycle boundary. Restore the current
        // session instead of leaving the token/iframe locked forever.
        if (invalidatesCredentials && isRecoverableNoopError(actionError)) {
          credentialInvalidation.current = null;
          setCredentialInvalidationPending(false);
        }
        setError(actionError);
      }
      setBusyAction(null);
    }
    return actionSucceeded;
  }, [refresh, snapshot, t]);

  const launcherStatus = asObject(snapshot.status);
  const isRunning = launcherStatus.running === true;
  const agentState = nestedValue(snapshot.state, "state");
  const connectionLabel = snapshot.status ? (isRunning ? t("Agent online") : t("Agent stopped")) : t("Bridge offline");
  const connectionTone = snapshot.status ? (isRunning ? "good" : "warn") : "bad";
  const contentMode = launcherContentMode(bridgeError, loading, snapshot.status !== null);

  const content = useMemo(() => {
    const common = { snapshot, busyAction, credentialInvalidationPending, runAction, refresh, themeMode, setThemeMode };
    switch (activeModule) {
      case "harness": return <HarnessView {...common} />;
      case "profiles": return <ProfilesView {...common} />;
      case "checkpoints": return <CheckpointsView {...common} />;
      case "updates": return <UpdatesView {...common} />;
      case "diagnostics": return <DiagnosticsView {...common} />;
      case "settings": return <SettingsView {...common} />;
      default: return <OverviewView {...common} />;
    }
  }, [activeModule, busyAction, credentialInvalidationPending, refresh, runAction, snapshot, t, themeMode]);

  return (
    <div className="app-shell">
      <aside className="sidebar" aria-label={t("Nexus modules")}>
        <div className="brand-lockup">
          <div className="brand-mark" aria-hidden="true"><RocketLaunch size={20} weight="fill" /></div>
          <div className="brand-copy"><strong>{t("NEXUS")}</strong><span>{t("LOCAL CONTROL")}</span></div>
        </div>
        <nav className="module-nav">
          {modules.map(({ id, label, icon: Icon }) => (
            <button
              className={`nav-item ${activeModule === id ? "active" : ""}`}
              key={id}
              onClick={() => setActiveModule(id)}
              aria-current={activeModule === id ? "page" : undefined}
              title={t(label)}
            >
              <Icon size={19} weight={activeModule === id ? "fill" : "regular"} aria-hidden="true" />
              <span>{t(label)}</span>
            </button>
          ))}
        </nav>
        <div className="sidebar-footer"><ShieldCheck size={16} /><span>{t("Loopback only")}</span></div>
      </aside>

      <main className="workspace">
        <header className="topbar">
          <div className="breadcrumbs"><span>{t("Nexus Launcher")}</span><span className="crumb-separator">/</span><strong>{t(modules.find((item) => item.id === activeModule)?.label || "Overview")}</strong></div>
          <div className="topbar-actions">
            <StatusPill label={connectionLabel} tone={connectionTone} />
            <button className="icon-button" onClick={() => void refresh()} aria-label={t("Refresh launcher status")} title={t("Refresh launcher status")}><ArrowsClockwise size={19} /></button>
          </div>
        </header>

        {notice && <div className="notice" role="status"><CheckCircle size={17} />{notice}<button onClick={() => setNotice(null)} aria-label={t("Dismiss notice")}><X size={15} /></button></div>}
        {error && contentMode !== "error" && <div className="notice action-error" role="alert"><WarningCircle size={17} /><span>{error}</span><button onClick={() => setError(null)} aria-label={t("Dismiss error")}><X size={15} /></button></div>}
        {agentUnavailable && contentMode !== "error" && <AgentUnavailableNotice message={agentUnavailable} onRetry={() => void retryStartup()} />}
        {!error && Object.keys(snapshot.endpointErrors).length > 0 && <DegradedNotice errors={snapshot.endpointErrors} />}
        {contentMode === "error"
          ? <ErrorState message={bridgeError ?? t("The native bridge is unavailable.")} onRetry={() => void retryStartup()} />
          : contentMode === "loading"
            ? <LoadingState />
            : <section className="page-content">{content}</section>}

        <footer className="workspace-footer">
          <span><Cpu size={15} />{t("Agent {version}", { version: stringValue(snapshot.health, "api_version") || "v1" })}</span>
          <span><Key size={15} />{t("No credentials leave this device")}</span>
          {snapshot.startup?.api_base && <span className="api-address">{snapshot.startup.api_base}</span>}
        </footer>
      </main>
    </div>
  );
}

type ViewProps = {
  snapshot: Snapshot;
  busyAction: string | null;
  credentialInvalidationPending: boolean;
  runAction: (label: string, path: string, body: JsonObject) => Promise<void | boolean>;
  refresh: () => Promise<void>;
  themeMode: ThemeMode;
  setThemeMode: (mode: ThemeMode) => void;
};

type HarnessPanelProps = Pick<ViewProps, "snapshot" | "busyAction" | "runAction">;
type HarnessAuthPanelProps = HarnessPanelProps & Pick<ViewProps, "credentialInvalidationPending">;
type HarnessWebPanelProps = Pick<ViewProps, "snapshot" | "credentialInvalidationPending">;

function OverviewView({ snapshot, busyAction, credentialInvalidationPending, runAction }: ViewProps) {
  const { t } = useI18n();
  const status = asObject(snapshot.status);
  const health = asObject(snapshot.health);
  const state = nestedValue(snapshot.state, "state");
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const profiles = arrayValue(snapshot.profiles, "profiles");
  const checkpoints = arrayValue(snapshot.checkpoints, "checkpoints");
  const update = nestedValue(snapshot.updates, "update");
  const agentRunning = status.running === true;
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  return (
    <>
      <div className="page-heading"><div><span className="kicker">{t("Runtime / Overview")}</span><h1>{t("Local control plane")}</h1><p>{t("Observe and operate the independent Agent and its immutable Harness runtime.")}</p></div><StatusPill label={agentRunning ? t("Running") : t("Standby")} tone={agentRunning ? "good" : "warn"} /></div>
      <div className="metric-grid">
        <Metric label={t("Agent lifecycle")} value={localizedRuntimeState(stringValue(state, "lifecycle"), t)} detail={localizedRuntimeState(stringValue(health, "status"), t)} />
        <Metric label={t("Harness")} value={localizedRuntimeState(stringValue(harness, "state"), t)} detail={stringValue(harness, "pid") ? t("PID {pid}", { pid: stringValue(harness, "pid") || "" }) : t("No child process")} />
        <Metric label={t("Active profile")} value={stringValue(state, "profile") || t("None selected")} detail={t("{count} profiles available", { count: profiles.length })} />
        <Metric label={t("Checkpoints")} value={String(checkpoints.length)} detail={updateStateLabel(update, t)} />
      </div>
      <div className="grid-two">
        <Panel title={t("Agent operations")} icon={<Pulse size={18} />}>
          <p className="panel-description">{t("The Agent remains a separate process. Launcher controls are explicit and recoverable.")}</p>
          <div className="button-row">
          <ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction(t("Agent start"), "/v1/agent", { action: "start" })}><CheckCircle size={16} />{t("Start Agent")}</ActionButton>
            <ActionButton disabled={controlsDisabled} onClick={() => void runAction(t("Agent restart"), "/v1/agent", { action: "restart" })}><ArrowsClockwise size={16} />{t("Restart")}</ActionButton>
            <ActionButton tone="danger" disabled={controlsDisabled} onClick={() => void runAction(t("Agent stop"), "/v1/agent", { action: "stop" })}><StopCircle size={16} />{t("Stop Agent")}</ActionButton>
          </div>
        </Panel>
        <HarnessControlPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
      </div>
      <HarnessAuthPanel snapshot={snapshot} busyAction={busyAction} credentialInvalidationPending={credentialInvalidationPending} runAction={runAction} />
      <HarnessWebPanel snapshot={snapshot} credentialInvalidationPending={credentialInvalidationPending} />
    </>
  );
}

function HarnessControlPanel({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const controlGate = harnessControlGate(
    stringValue(harness, "state"),
    numberValue(harness, "pid"),
    busyAction !== null,
    snapshot.startup?.available === true,
  );
  const state = stringValue(harness, "state");
  const controlsDisabled = controlGate.controlsDisabled;
  const startDisabled = controlsDisabled || state === "running" || state === "starting" || state === "stopping";
  const restartDisabled = controlsDisabled || state === "starting" || state === "stopping" || state === "detached";
  const stopDisabled = controlsDisabled || !["running", "starting", "failed"].includes(state || "");
  const harnessAction = (action: string) => void runAction(t(`Harness ${action}`), "/v1/harness", { action });
  return (
    <Panel title={t("Harness controls")} icon={<MonitorPlay size={18} />}>
      <div className="button-row">
        <ActionButton tone="primary" disabled={startDisabled} onClick={() => harnessAction("start")}><CheckCircle size={16} />{t("Start")}</ActionButton>
        <ActionButton disabled={restartDisabled} onClick={() => harnessAction("restart")}><ArrowsClockwise size={16} />{t("Restart")}</ActionButton>
        <ActionButton tone="danger" disabled={stopDisabled} onClick={() => harnessAction("stop")}><StopCircle size={16} />{t("Stop")}</ActionButton>
      </div>
      {controlGate.externallyManaged && <p className="field-help" role="status">{t("Harness is running outside this Agent process. Manage it from its owning Agent; lifecycle controls are disabled here.")}</p>}
      {state === "detached" && <p className="field-help" role="status">{t("Harness is detached. Configure it in Settings, then start it from the control panel.")}</p>}
      <dl className="detail-list compact-details">
        <div><dt>{t("Process ID")}</dt><dd>{stringValue(harness, "pid") || t("Not attached")}</dd></div>
        <div><dt>{t("Exit code")}</dt><dd>{stringValue(harness, "exit_code") || t("Not exited")}</dd></div>
        <div><dt>{t("Last error")}</dt><dd>{harness.error ? localizeBackendError(stringValue(harness, "error") || "", t) : t("None reported")}</dd></div>
      </dl>
    </Panel>
  );
}

function HarnessAuthPanel({ snapshot, busyAction, credentialInvalidationPending, runAction }: HarnessAuthPanelProps) {
  const { t } = useI18n();
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
    setRevealedSessionKey((revealed) => revealed === sessionKey ? revealed : undefined);
  }, [sessionKey]);
  const openSystemBrowser = () => void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" });
  return (
    <Panel title={t("Authentication metadata")} icon={<Key size={18} />}>
          {token ? <>
            <label className="field-label" htmlFor="harness-token">{t("Latest loopback token")}</label>
            <div className="token-row"><input id="harness-token" readOnly type={showToken ? "text" : "password"} value={token} aria-describedby="token-help" /><button className="button subtle" onClick={() => setRevealedSessionKey(showToken ? undefined : sessionKey)}>{showToken ? t("Hide") : t("Reveal")}</button></div>
            <p className="field-help" id="token-help">{t("Read from a bounded Nexus-owned Harness log tail. It is not written to Nexus state.")}</p>
          </> : <EmptyState title={t("No token observed")} detail={info.message ? localizeBackendError(stringValue(info, "message") || "", t) : t("Start Harness and refresh when its loopback URL is ready.")} />}
          <div className="metadata-grid"><div><span>{t("Source")}</span><strong>{stringValue(info, "source") || t("Not available")}</strong></div><div><span>{t("Observed")}</span><strong>{formatTimestamp(numberValue(info, "observed_at_unix"), t("Not available"))}</strong></div></div>
          <div className="button-row"><ActionButton disabled={!token || controlsDisabled} onClick={() => void navigator.clipboard?.writeText(token || "")}><ClipboardText size={16} />{t("Copy token")}</ActionButton><ActionButton tone="primary" disabled={!uiUrl || controlsDisabled} onClick={openSystemBrowser}><RocketLaunch size={16} />{t("Open in system browser")}</ActionButton></div>
    </Panel>
  );
}

function HarnessWebPanel({ snapshot, credentialInvalidationPending }: HarnessWebPanelProps) {
  const { t } = useI18n();
  const info = asObject(snapshot.harnessUi);
  const uiUrl = harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi, credentialInvalidationPending)
    ? stringValue(info, "url")
    : undefined;
  const safeUrl = isLoopbackUrl(uiUrl) ? uiUrl : undefined;
  return <Panel title={t("Embedded Harness Web")} icon={<MonitorPlay size={18} />}>
    {safeUrl ? <iframe className="harness-frame" title={t("Harness Web interface")} src={safeUrl} referrerPolicy="no-referrer" sandbox="allow-forms allow-scripts allow-same-origin" /> : <EmptyState title={t("Harness view is not ready")} detail={t("A validated loopback HTTP URL will appear here when Harness reports its web interface.")} />}
  </Panel>;
}

export function HarnessView({ snapshot, busyAction, credentialInvalidationPending, runAction }: ViewProps) {
  const { t } = useI18n();
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const harnessRunning = stringValue(harness, "state") === "running";
  return <>
    <div className="page-heading"><div><span className="kicker">{t("Runtime / Harness")}</span><h1>{t("Harness workspace")}</h1><p>{t("Harness is an immutable external runtime. Nexus only supervises its process.")}</p></div><StatusPill label={localizedRuntimeState(stringValue(harness, "state"), t)} tone={harnessRunning ? "good" : "neutral"} /></div>
    <div className="grid-two harness-grid"><HarnessControlPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} /><HarnessAuthPanel snapshot={snapshot} busyAction={busyAction} credentialInvalidationPending={credentialInvalidationPending} runAction={runAction} /></div>
    <HarnessWebPanel snapshot={snapshot} credentialInvalidationPending={credentialInvalidationPending} />
  </>;
}

function ProfilesView({ snapshot }: ViewProps) {
  const { t } = useI18n();
  const active = stringValue(snapshot.profiles, "active_profile");
  const items = arrayValue(snapshot.profiles, "profiles");
  return <><PageIntro kicker={t("Control / Profiles")} title={t("Profiles")} detail={t("Nexus-owned profile names are passed to Harness only through explicit launch configuration.")} /><Panel title={t("Profile catalog")} icon={<SlidersHorizontal size={18} />}><DataList items={items} emptyTitle={t("No profiles configured")} emptyDetail={t("The Agent will expose profiles after its catalog is initialized.")} render={(item) => { const name = typeof item === "string" ? item : stringValue(item, "name") || t("Unnamed profile"); return <><div><strong>{name}</strong>{name === active && <StatusPill label={t("Active")} tone="good" />}</div><span className="row-meta">{name === active ? t("Selected by Agent") : t("Available")}</span></>; }} /></Panel></>;
}

export function CheckpointsView({ snapshot, busyAction, runAction }: ViewProps) {
  const { t } = useI18n();
  const items = arrayValue(snapshot.checkpoints, "checkpoints");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  return <><PageIntro kicker={t("State / Checkpoints")} title={t("Checkpoints")} detail={t("Checkpoint manifests contain only Harness profile/release selection. Agent lifecycle and Harness runtime are never saved or restored.")} /><Panel title={t("Saved checkpoints")} icon={<ListChecks size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{t("{count} saved", { count: items.length })}</span><ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction(t("Checkpoint creation"), "/v1/checkpoints", { action: "create", note: "Native launcher checkpoint" })}><CheckCircle size={16} />{t("Create checkpoint")}</ActionButton></div><DataList items={items} emptyTitle={t("No checkpoints yet")} emptyDetail={t("Create a checkpoint after the Agent has a stable profile and release state.")} render={(item) => <><div><strong>{stringValue(item, "id") || t("Checkpoint")}</strong><span>{stringValue(item, "profile") || t("No profile")}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"), t("Not available"))}</span></>} /></Panel></>;
}

function UpdatesView({ snapshot }: ViewProps) {
  const { t } = useI18n();
  const update = nestedValue(snapshot.updates, "update");
  const release = nestedValue(snapshot.updates, "release");
  const releases = arrayValue(snapshot.releases, "releases");
  const updateState = stringValue(update, "state");
  return <><PageIntro kicker={t("Releases / Updates")} title={t("Updates")} detail={t("Release installation is external and explicit. Promotion stays separate from downloading and verification.")} /><div className="grid-two"><Panel title={t("Update status")} icon={<CloudArrowUp size={18} />}><div className="status-block"><StatusPill label={localizedRuntimeState(updateState, t)} tone={updateState === "failed" ? "bad" : "neutral"} /><strong>{stringValue(update, "release_id") || t("No active update")}</strong><span>{stringValue(update, "error") ? localizeBackendError(stringValue(update, "error") || "", t) : t("No update error reported")}</span></div></Panel><Panel title={t("Current release")} icon={<Package size={18} />}><dl className="detail-list compact-details"><div><dt>{t("Version")}</dt><dd>{stringValue(release, "version") || t("Not registered")}</dd></div><div><dt>{t("Current slot")}</dt><dd>{stringValue(snapshot.releases, "current_release") || t("None")}</dd></div><div><dt>{t("Last known good")}</dt><dd>{stringValue(snapshot.releases, "last_known_good") || t("None")}</dd></div></dl></Panel></div><Panel title={t("Release slots")} icon={<Package size={18} />}><DataList items={releases} emptyTitle={t("No release slots")} emptyDetail={t("Register an immutable slot through the Agent API before promotion.")} render={(item) => <><div><strong>{stringValue(item, "id") || t("Release")}</strong><span>{stringValue(item, "version") || t("Unknown version")}</span></div><span className="row-meta">{localizedRuntimeState(stringValue(item, "status"), t)}</span></>} /></Panel></>;
}

function DiagnosticsView({ snapshot, busyAction, runAction }: ViewProps) {
  const { t } = useI18n();
  const items = arrayValue(snapshot.diagnostics, "bundles");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  return <><PageIntro kicker={t("Observability / Diagnostics")} title={t("Diagnostics")} detail={t("Bundles are bounded, redacted, and limited to Nexus-owned metadata and text logs.")} /><Panel title={t("Diagnostic bundles")} icon={<TerminalWindow size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{t("{count} bundles", { count: items.length })}</span><ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction(t("Diagnostic collection"), "/v1/diagnostics", { action: "collect", note: "Native launcher collection" })}><TerminalWindow size={16} />{t("Collect diagnostics")}</ActionButton></div><DataList items={items} emptyTitle={t("No diagnostic bundles")} emptyDetail={t("Collect a bounded bundle when a runtime issue needs review.")} render={(item) => <><div><strong>{stringValue(item, "id") || t("Bundle")}</strong><span>{t("{count} files", { count: arrayValue(item, "files").length })}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"), t("Not available"))}</span></>} /></Panel></>;
}

function SettingsView({ snapshot, themeMode, setThemeMode, busyAction, runAction }: ViewProps) {
  const { locale, setLocale, t } = useI18n();
  const config = asObject(snapshot.config);
  const harness = nestedValue(config, "harness");
  const update = nestedValue(config, "update");
  const hasHarnessConfig = Object.keys(harness).length > 0;
  const harnessRuntime = harnessRuntimeValue(snapshot.harnessRuntime);
  const harnessState = stringValue(harnessRuntime, "state");
  const harnessInTransition = harnessState === "starting" || harnessState === "stopping";
  const configControlsDisabled = busyAction !== null || snapshot.startup?.available !== true || harnessInTransition || harnessState === "running";
  const [editingHarness, setEditingHarness] = useState(!hasHarnessConfig);
  const [draft, setDraft] = useState<HarnessConfigDraft>(() => harnessDraftFromConfig(config));
  const [draftDirty, setDraftDirty] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  useEffect(() => {
    if (!draftDirty) {
      setDraft(harnessDraftFromConfig(asObject(snapshot.config)));
      setEditingHarness(Object.keys(nestedValue(asObject(snapshot.config), "harness")).length === 0);
    }
  }, [draftDirty, snapshot.config]);

  const updateDraft = (field: keyof HarnessConfigDraft, value: string | boolean) => {
    setDraft((current) => ({ ...current, [field]: value }));
    setDraftDirty(true);
    setFormError(null);
  };

  const openEditor = () => {
    setDraft(harnessDraftFromConfig(config));
    // Keep the editor open while the background poll refreshes runtime data.
    setDraftDirty(true);
    setFormError(null);
    setEditingHarness(true);
  };

  const saveHarness = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setFormError(null);
    const program = draft.program.trim();
    if (!program) {
      setFormError(t("A program path is required."));
      return;
    }
    const readinessUrl = draft.readinessUrl.trim();
    if (readinessUrl && !isLoopbackUrl(readinessUrl)) {
      setFormError(t("Readiness URL must be an HTTP loopback URL."));
      return;
    }
    const timeoutText = draft.timeout.trim();
    let timeout: number | undefined;
    if (timeoutText) {
      const parsed = Number(timeoutText);
      if (!Number.isInteger(parsed) || parsed <= 0) {
        setFormError(t("Timeout must be a positive integer."));
        return;
      }
      timeout = parsed;
    }
    if (draft.argsRedacted && !draft.replaceRedactedArgs) {
      setFormError(t("Existing sensitive arguments are hidden. Enable replacement before saving."));
      return;
    }
    const args = draft.args
      .split(/\r?\n/)
      .map((value) => value.trim())
      .filter(Boolean);
    const saved = await runAction(t("Save Harness configuration"), "/v1/config", {
      action: "set_harness",
      harness: {
        program,
        args,
        working_dir: draft.workingDir.trim() || null,
        readiness_url: readinessUrl || null,
        readiness_timeout_secs: timeout ?? null,
      },
    });
    if (saved === true) {
      setDraftDirty(false);
      setEditingHarness(false);
      setFormError(null);
    }
  };

  const clearHarness = async () => {
    if (!window.confirm(t("Remove the Harness launch configuration? Harness must be stopped first."))) return;
    const cleared = await runAction(t("Clear Harness configuration"), "/v1/config", { action: "clear_harness" });
    if (cleared === true) {
      setDraft(emptyHarnessDraft);
      setDraftDirty(false);
      setEditingHarness(true);
      setFormError(null);
    }
  };

  return <>
    <PageIntro kicker={t("System / Settings")} title={t("Settings")} detail={t("Configuration remains Agent-owned. This view intentionally exposes metadata, not credentials or raw environment values.")} />
    <div className="grid-two">
      <Panel title={t("Appearance")} icon={<Gear size={18} />}>
        <label className="field-label" htmlFor="theme-mode">{t("Theme")}</label>
        <select id="theme-mode" className="theme-select" value={themeMode} onChange={(event) => setThemeMode(event.target.value as ThemeMode)}>
          <option value="system">{t("System")}</option><option value="light">{t("Light")}</option><option value="dark">{t("Dark")}</option>
        </select>
        <p className="field-help">{t("System follows the operating system preference. Your choice is saved locally.")}</p>
        <label className="field-label" htmlFor="locale-mode">{t("Language")}</label>
        <select id="locale-mode" className="theme-select" value={locale} onChange={(event) => setLocale(event.target.value as Locale)}>
          <option value="en">{t("English")}</option><option value="zh">{t("Chinese")}</option>
        </select>
        <p className="field-help">{t("Choose the language used by the Launcher interface.")}</p>
      </Panel>
      <Panel title={t("Harness configuration")} icon={<Gear size={18} />}>
        <p className="panel-description">{t("Configure the external Harness here. Editing config.json is only a fallback.")}</p>
        {!editingHarness && hasHarnessConfig ? <>
          <dl className="detail-list"><div><dt>{t("Program")}</dt><dd>{stringValue(harness, "program") || t("Not configured")}</dd></div><div><dt>{t("Working directory")}</dt><dd>{stringValue(harness, "working_dir") || t("Default")}</dd></div><div><dt>{t("Readiness URL")}</dt><dd>{isLoopbackUrl(stringValue(harness, "readiness_url")) ? stringValue(harness, "readiness_url") : t("Not shown")}</dd></div></dl>
          <div className="form-actions"><button type="button" className="button" disabled={configControlsDisabled} onClick={openEditor}>{t("Edit configuration")}</button><button type="button" className="button danger" disabled={configControlsDisabled} onClick={() => void clearHarness()}>{t("Clear configuration")}</button></div>
        </> : <form className="config-form" onSubmit={(event) => void saveHarness(event)}>
          <div className="form-grid">
            <label className="form-field full"><span className="field-label">{t("Program")}</span><input className="form-input" value={draft.program} onChange={(event) => updateDraft("program", event.target.value)} placeholder={t("Program path or command")} disabled={configControlsDisabled} required /></label>
            <label className="form-field"><span className="field-label">{t("Working directory")} <em>{t("Optional")}</em></span><input className="form-input" value={draft.workingDir} onChange={(event) => updateDraft("workingDir", event.target.value)} placeholder={t("Agent default")} disabled={configControlsDisabled} /></label>
            <label className="form-field"><span className="field-label">{t("Readiness timeout (seconds)")} <em>{t("Optional")}</em></span><input className="form-input" inputMode="numeric" value={draft.timeout} onChange={(event) => updateDraft("timeout", event.target.value)} placeholder={t("Agent default")} disabled={configControlsDisabled} /></label>
            <label className="form-field full"><span className="field-label">{t("Readiness URL")} <em>{t("Optional")}</em></span><input className="form-input" type="url" value={draft.readinessUrl} onChange={(event) => updateDraft("readinessUrl", event.target.value)} placeholder="http://127.0.0.1:3080/" disabled={configControlsDisabled} /></label>
            <label className="form-field full"><span className="field-label">{t("Arguments")}</span><textarea className="form-textarea" value={draft.args} onChange={(event) => updateDraft("args", event.target.value)} placeholder={t("One argument per line. Use {profile}, {release}, or {release_root} when needed.")} disabled={configControlsDisabled || (draft.argsRedacted && !draft.replaceRedactedArgs)} /></label>
          </div>
          <p className="field-help">{t("One argument per line. Use {profile}, {release}, or {release_root} when needed.")}</p>
          {draft.argsRedacted && <label className="form-check"><input type="checkbox" checked={draft.replaceRedactedArgs} onChange={(event) => updateDraft("replaceRedactedArgs", event.target.checked)} disabled={configControlsDisabled} /><span>{t("Replace hidden arguments")}</span></label>}
          {formError && <div className="form-error" role="alert"><WarningCircle size={16} />{formError}</div>}
          {harnessState === "running" && <p className="field-help" role="status">{t("Stop Harness before changing its launch configuration.")}</p>}
          <div className="form-actions"><button type="submit" className="button primary" disabled={configControlsDisabled}>{t("Save configuration")}</button>{hasHarnessConfig && <button type="button" className="button" disabled={configControlsDisabled} onClick={() => { setDraftDirty(false); setFormError(null); setEditingHarness(false); }}>{t("Cancel")}</button>}{hasHarnessConfig && <button type="button" className="button danger" disabled={configControlsDisabled} onClick={() => void clearHarness()}>{t("Clear configuration")}</button>}</div>
        </form>}
        {!hasHarnessConfig && !editingHarness && <EmptyState title={t("Harness is not configured")} detail={t("The Agent remains usable as a control plane until an external Harness is configured.")} />}
      </Panel>
      <Panel title={t("Update configuration")} icon={<CloudArrowUp size={18} />}>
        {Object.keys(update).length ? <dl className="detail-list"><div><dt>{t("Source")}</dt><dd>{stringValue(update, "source") || t("Not shown")}</dd></div><div><dt>{t("Ref")}</dt><dd>{stringValue(update, "ref_name") || t("Default")}</dd></div><div><dt>{t("Git program")}</dt><dd>{stringValue(update, "git_program") || "git"}</dd></div></dl> : <EmptyState title={t("Updates are not configured")} detail={t("Release metadata and current runtime remain available without an update source.")} />}
      </Panel>
    </div>
    <Panel title={t("Native integration")} icon={<Bell size={18} />}><div className="integration-list"><div><CheckCircle size={18} /><span>{t("Single instance guard")}</span><strong>{t("Enabled")}</strong></div><div><Bell size={18} /><span>{t("Desktop notifications")}</span><strong>{t("Available through Tauri")}</strong></div><div><Key size={18} /><span>{t("API transport")}</span><strong>{t("Rust loopback proxy")}</strong></div></div></Panel>
  </>;
}

function PageIntro({ kicker, title, detail }: { kicker: string; title: string; detail: string }) {
  return <div className="page-heading"><div><span className="kicker">{kicker}</span><h1>{title}</h1><p>{detail}</p></div></div>;
}

function Panel({ title, icon, children }: { title: string; icon: React.ReactNode; children: React.ReactNode }) {
  return <section className="panel"><div className="panel-header"><div className="panel-title">{icon}<h2>{title}</h2></div></div>{children}</section>;
}

export default App;
