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

type JsonObject = Record<string, unknown>;
type IconComponent = React.ComponentType<IconProps>;

type StartupStatus = {
  available: boolean;
  api_base?: string;
  helper_path?: string;
  data_root_id?: string;
  launcher_instance_id?: string;
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

type SnapshotEndpoint = Exclude<keyof Snapshot, "startup" | "endpointErrors">;

const endpointMap: Record<SnapshotEndpoint, string> = {
  status: "/launcher/status",
  health: "/launcher/agent-api/v1/health",
  state: "/launcher/agent-api/v1/state",
  harnessRuntime: "/launcher/agent-api/v1/harness",
  harnessUi: "/launcher/harness",
  profiles: "/launcher/agent-api/v1/profiles",
  checkpoints: "/launcher/agent-api/v1/checkpoints",
  releases: "/launcher/agent-api/v1/releases",
  updates: "/launcher/agent-api/v1/updates",
  diagnostics: "/launcher/agent-api/v1/diagnostics",
  config: "/launcher/agent-api/v1/config",
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
  const pid = numberValue(runtime, "pid");
  return (
    !credentialInvalidationPending &&
    stringValue(runtime, "state") === "running" &&
    pid !== undefined && pid > 0 &&
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

function formatTimestamp(value: unknown): string {
  if (typeof value !== "number" || value <= 0) return "Not available";
  return new Date(value * 1000).toLocaleString();
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "The native bridge returned an unknown error";
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
  return (
    <div className="state-card loading-state" role="status" aria-live="polite">
      <Pulse size={22} className="spin" aria-hidden="true" />
      <div><strong>Connecting to Nexus</strong><span>Waiting for the local control plane.</span></div>
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

function ErrorState({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="state-card error-state" role="alert">
      <WarningCircle size={25} aria-hidden="true" />
      <div className="state-copy"><strong>Launcher bridge unavailable</strong><span>{message}</span></div>
      <button className="button subtle" onClick={onRetry}><ArrowClockwise size={16} />Retry</button>
    </div>
  );
}

function DegradedNotice({ errors }: { errors: Record<string, string> }) {
  const details = Object.entries(errors)
    .map(([path, message]) => `${path}: ${compactError(message)}`)
    .join(" | ");
  return <div className="notice degraded" role="status" aria-live="polite"><WarningCircle size={17} /> <span>Some workspace data is unavailable. {details}</span></div>;
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
  const [activeModule, setActiveModule] = useState<ModuleId>("overview");
  const [themeMode, setThemeMode] = useState<ThemeMode>(storedTheme);
  const [systemThemeMode, setSystemThemeMode] = useState<"light" | "dark">(systemTheme);
  const [snapshot, setSnapshot] = useState<Snapshot>(emptySnapshot);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
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
            const next: Snapshot = { ...emptySnapshot, startup };
            if (!startup.available) {
              harnessPollState.current = undefined;
              setSnapshot(next);
              setNotice(null);
              setError(startup.message || "Set NEXUS_LAUNCHER_BIN or build the Rust launcher helper.");
              continue;
            }
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
              setError("The Launcher API is not responding on its loopback port.");
            }
          } catch (cause) {
            harnessPollState.current = undefined;
            setSnapshot(failClosedSnapshot(emptySnapshot));
            setNotice(null);
            setError(errorMessage(cause));
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
  }, []);

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

  const runAction = useCallback(async (label: string, path: string, body: JsonObject) => {
    if (snapshot.startup?.available !== true) {
      setError("Launcher controls are disabled until the native helper identity is verified.");
      return;
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
    try {
      await proxyRequest(path, "POST", body);
      setNotice(`${label} complete`);
    } catch (cause) {
      actionError = `${label} failed: ${errorMessage(cause)}`;
    } finally {
      // Refresh after both successful and failed POSTs. The Agent may have
      // advanced a generation before returning an error (for example an
      // unattached stop), and the UI must not leave the prior snapshot visible.
      await refresh();
      if (actionError) setError(actionError);
      setBusyAction(null);
    }
  }, [refresh, snapshot]);

  const launcherStatus = asObject(snapshot.status);
  const isRunning = launcherStatus.running === true;
  const agentState = nestedValue(snapshot.state, "state");
  const connectionLabel = snapshot.status ? (isRunning ? "Agent online" : "Agent stopped") : "Bridge offline";
  const connectionTone = snapshot.status ? (isRunning ? "good" : "warn") : "bad";
  const contentMode = launcherContentMode(error, loading, snapshot.status !== null);

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
  }, [activeModule, busyAction, credentialInvalidationPending, refresh, runAction, snapshot, themeMode]);

  return (
    <div className="app-shell">
      <aside className="sidebar" aria-label="Nexus modules">
        <div className="brand-lockup">
          <div className="brand-mark" aria-hidden="true"><RocketLaunch size={20} weight="fill" /></div>
          <div className="brand-copy"><strong>NEXUS</strong><span>LOCAL CONTROL</span></div>
        </div>
        <nav className="module-nav">
          {modules.map(({ id, label, icon: Icon }) => (
            <button
              className={`nav-item ${activeModule === id ? "active" : ""}`}
              key={id}
              onClick={() => setActiveModule(id)}
              aria-current={activeModule === id ? "page" : undefined}
              title={label}
            >
              <Icon size={19} weight={activeModule === id ? "fill" : "regular"} aria-hidden="true" />
              <span>{label}</span>
            </button>
          ))}
        </nav>
        <div className="sidebar-footer"><ShieldCheck size={16} /><span>Loopback only</span></div>
      </aside>

      <main className="workspace">
        <header className="topbar">
          <div className="breadcrumbs"><span>Nexus Launcher</span><span className="crumb-separator">/</span><strong>{modules.find((item) => item.id === activeModule)?.label}</strong></div>
          <div className="topbar-actions">
            <StatusPill label={connectionLabel} tone={connectionTone} />
            <button className="icon-button" onClick={() => void refresh()} aria-label="Refresh launcher status" title="Refresh launcher status"><ArrowsClockwise size={19} /></button>
          </div>
        </header>

        {notice && <div className="notice" role="status"><CheckCircle size={17} />{notice}<button onClick={() => setNotice(null)} aria-label="Dismiss notice"><X size={15} /></button></div>}
        {!error && Object.keys(snapshot.endpointErrors).length > 0 && <DegradedNotice errors={snapshot.endpointErrors} />}
        {contentMode === "error"
          ? <ErrorState message={error ?? "The native bridge is unavailable."} onRetry={() => void refresh()} />
          : contentMode === "loading"
            ? <LoadingState />
            : <section className="page-content">{content}</section>}

        <footer className="workspace-footer">
          <span><Cpu size={15} />Agent {stringValue(snapshot.health, "api_version") || "v1"}</span>
          <span><Key size={15} />No credentials leave this device</span>
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
  runAction: (label: string, path: string, body: JsonObject) => Promise<void>;
  refresh: () => Promise<void>;
  themeMode: ThemeMode;
  setThemeMode: (mode: ThemeMode) => void;
};

function OverviewView({ snapshot, busyAction, runAction }: ViewProps) {
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
      <div className="page-heading"><div><span className="kicker">RUNTIME / OVERVIEW</span><h1>Local control plane</h1><p>Observe and operate the independent Agent and its immutable Harness runtime.</p></div><StatusPill label={agentRunning ? "Running" : "Standby"} tone={agentRunning ? "good" : "warn"} /></div>
      <div className="metric-grid">
        <Metric label="Agent lifecycle" value={stringValue(state, "lifecycle") || "Unknown"} detail={stringValue(health, "status") || "No health response"} />
        <Metric label="Harness" value={stringValue(harness, "state") || "Detached"} detail={stringValue(harness, "pid") ? `PID ${stringValue(harness, "pid")}` : "No child process"} />
        <Metric label="Active profile" value={stringValue(state, "profile") || "None selected"} detail={`${profiles.length} profiles available`} />
        <Metric label="Checkpoints" value={String(checkpoints.length)} detail={stringValue(update, "state") || "Update queue idle"} />
      </div>
      <div className="grid-two">
        <Panel title="Agent operations" icon={<Pulse size={18} />}>
          <p className="panel-description">The Agent remains a separate process. Launcher controls are explicit and recoverable.</p>
          <div className="button-row">
            <ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction("Agent start", "/launcher/agent", { action: "start" })}><CheckCircle size={16} />Start Agent</ActionButton>
            <ActionButton disabled={controlsDisabled} onClick={() => void runAction("Agent restart", "/launcher/agent", { action: "restart" })}><ArrowsClockwise size={16} />Restart</ActionButton>
            <ActionButton tone="danger" disabled={controlsDisabled} onClick={() => void runAction("Agent stop", "/launcher/agent", { action: "stop" })}><StopCircle size={16} />Stop Agent</ActionButton>
          </div>
        </Panel>
        <Panel title="Runtime boundary" icon={<ShieldCheck size={18} />}>
          <dl className="detail-list">
            <div><dt>Data root</dt><dd>{stringValue(status, "data_root") || "Not reported"}</dd></div>
            <div><dt>Agent API</dt><dd>{stringValue(status, "agent_api") || "Loopback unavailable"}</dd></div>
            <div><dt>Agent PID</dt><dd>{stringValue(status, "agent_pid") || "Not reported"}</dd></div>
          </dl>
        </Panel>
      </div>
      <Panel title="Activity signal" icon={<Pulse size={18} />}>
        {snapshot.startup?.helper_path ? <div className="signal-line"><CheckCircle size={17} />Headless helper ready<span>{snapshot.startup.helper_path}</span></div> : <EmptyState title="Helper path pending" detail="The native side will report the resolved launcher helper after startup." />}
      </Panel>
    </>
  );
}

export function HarnessView({ snapshot, busyAction, credentialInvalidationPending, runAction }: ViewProps) {
  const info = asObject(snapshot.harnessUi);
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const harnessRunning = stringValue(harness, "state") === "running";
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
  const safeUrl = isLoopbackUrl(uiUrl) ? uiUrl : undefined;
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  const controlGate = harnessControlGate(
    stringValue(harness, "state"),
    numberValue(harness, "pid"),
    busyAction !== null,
    snapshot.startup?.available === true,
  );
  useEffect(() => {
    setRevealedSessionKey((revealed) => revealed === sessionKey ? revealed : undefined);
  }, [sessionKey]);
  const harnessAction = (action: string) => void runAction(`Harness ${action}`, "/launcher/agent-api/v1/harness", { action });
  const openSystemBrowser = () => void runAction("Open Harness", "/launcher/harness", { action: "open" });
  return (
    <>
      <div className="page-heading"><div><span className="kicker">RUNTIME / HARNESS</span><h1>Harness workspace</h1><p>Harness is an immutable external runtime. Nexus only supervises its process.</p></div><StatusPill label={stringValue(harness, "state") || "Detached"} tone={harnessRunning ? "good" : "neutral"} /></div>
      <div className="grid-two harness-grid">
        <Panel title="Harness controls" icon={<MonitorPlay size={18} />}>
          <div className="button-row">
            <ActionButton tone="primary" disabled={controlGate.controlsDisabled} onClick={() => harnessAction("start")}><CheckCircle size={16} />Start</ActionButton>
            <ActionButton disabled={controlGate.controlsDisabled} onClick={() => harnessAction("restart")}><ArrowsClockwise size={16} />Restart</ActionButton>
            <ActionButton tone="danger" disabled={controlGate.controlsDisabled} onClick={() => harnessAction("stop")}><StopCircle size={16} />Stop</ActionButton>
          </div>
          {controlGate.externallyManaged && <p className="field-help" role="status">Harness is running outside this Agent process. Manage it from its owning Agent; lifecycle controls are disabled here.</p>}
          <dl className="detail-list compact-details">
            <div><dt>Process ID</dt><dd>{stringValue(harness, "pid") || "Not attached"}</dd></div>
            <div><dt>Exit code</dt><dd>{stringValue(harness, "exit_code") || "Not exited"}</dd></div>
            <div><dt>Last error</dt><dd>{stringValue(harness, "error") || "None reported"}</dd></div>
          </dl>
        </Panel>
        <Panel title="Authentication metadata" icon={<Key size={18} />}>
          {token ? <>
            <label className="field-label" htmlFor="harness-token">Latest loopback token</label>
            <div className="token-row"><input id="harness-token" readOnly type={showToken ? "text" : "password"} value={token} aria-describedby="token-help" /><button className="button subtle" onClick={() => setRevealedSessionKey(showToken ? undefined : sessionKey)}>{showToken ? "Hide" : "Reveal"}</button></div>
            <p className="field-help" id="token-help">Read from a bounded Nexus-owned Harness log tail. It is not written to Nexus state.</p>
          </> : <EmptyState title="No token observed" detail={stringValue(info, "message") || "Start Harness and refresh when its loopback URL is ready."} />}
          <div className="metadata-grid"><div><span>Source</span><strong>{stringValue(info, "source") || "Not available"}</strong></div><div><span>Observed</span><strong>{formatTimestamp(numberValue(info, "observed_at_unix"))}</strong></div></div>
          <div className="button-row"><ActionButton disabled={!token || controlsDisabled} onClick={() => void navigator.clipboard?.writeText(token || "")}><ClipboardText size={16} />Copy token</ActionButton><ActionButton tone="primary" disabled={!uiUrl || controlsDisabled} onClick={openSystemBrowser}><RocketLaunch size={16} />Open in system browser</ActionButton></div>
        </Panel>
      </div>
      <Panel title="Embedded Harness Web" icon={<MonitorPlay size={18} />}>
        {safeUrl ? <iframe className="harness-frame" title="Harness Web interface" src={safeUrl} referrerPolicy="no-referrer" sandbox="allow-forms allow-scripts allow-same-origin" /> : <EmptyState title="Harness view is not ready" detail="A validated loopback HTTP URL will appear here when Harness reports its web interface." />}
      </Panel>
    </>
  );
}

function ProfilesView({ snapshot }: ViewProps) {
  const active = stringValue(snapshot.profiles, "active_profile");
  const items = arrayValue(snapshot.profiles, "profiles");
  return <><PageIntro kicker="CONTROL / PROFILES" title="Profiles" detail="Nexus-owned profile names are passed to Harness only through explicit launch configuration." /><Panel title="Profile catalog" icon={<SlidersHorizontal size={18} />}><DataList items={items} emptyTitle="No profiles configured" emptyDetail="The Agent will expose profiles after its catalog is initialized." render={(item) => { const name = typeof item === "string" ? item : stringValue(item, "name") || "Unnamed profile"; return <><div><strong>{name}</strong>{name === active && <StatusPill label="Active" tone="good" />}</div><span className="row-meta">{name === active ? "Selected by Agent" : "Available"}</span></>; }} /></Panel></>;
}

export function CheckpointsView({ snapshot, busyAction, runAction }: ViewProps) {
  const items = arrayValue(snapshot.checkpoints, "checkpoints");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  return <><PageIntro kicker="STATE / CHECKPOINTS" title="Checkpoints" detail="Checkpoint manifests contain only Harness profile/release selection. Agent lifecycle and Harness runtime are never saved or restored." /><Panel title="Saved checkpoints" icon={<ListChecks size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{items.length} saved</span><ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction("Checkpoint creation", "/launcher/agent-api/v1/checkpoints", { action: "create", note: "Native launcher checkpoint" })}><CheckCircle size={16} />Create checkpoint</ActionButton></div><DataList items={items} emptyTitle="No checkpoints yet" emptyDetail="Create a checkpoint after the Agent has a stable profile and release state." render={(item) => <><div><strong>{stringValue(item, "id") || "Checkpoint"}</strong><span>{stringValue(item, "profile") || "No profile"}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"))}</span></>} /></Panel></>;
}

function UpdatesView({ snapshot }: ViewProps) {
  const update = nestedValue(snapshot.updates, "update");
  const release = nestedValue(snapshot.updates, "release");
  const releases = arrayValue(snapshot.releases, "releases");
  return <><PageIntro kicker="RELEASES / UPDATES" title="Updates" detail="Release installation is external and explicit. Promotion stays separate from downloading and verification." /><div className="grid-two"><Panel title="Update status" icon={<CloudArrowUp size={18} />}><div className="status-block"><StatusPill label={stringValue(update, "state") || "idle"} tone={stringValue(update, "state") === "failed" ? "bad" : "neutral"} /><strong>{stringValue(update, "release_id") || "No active update"}</strong><span>{stringValue(update, "error") || "No update error reported"}</span></div></Panel><Panel title="Current release" icon={<Package size={18} />}><dl className="detail-list compact-details"><div><dt>Version</dt><dd>{stringValue(release, "version") || "Not registered"}</dd></div><div><dt>Current slot</dt><dd>{stringValue(snapshot.releases, "current_release") || "None"}</dd></div><div><dt>Last known good</dt><dd>{stringValue(snapshot.releases, "last_known_good") || "None"}</dd></div></dl></Panel></div><Panel title="Release slots" icon={<Package size={18} />}><DataList items={releases} emptyTitle="No release slots" emptyDetail="Register an immutable slot through the Agent API before promotion." render={(item) => <><div><strong>{stringValue(item, "id") || "Release"}</strong><span>{stringValue(item, "version") || "Unknown version"}</span></div><span className="row-meta">{stringValue(item, "status") || "Registered"}</span></>} /></Panel></>;
}

function DiagnosticsView({ snapshot, busyAction, runAction }: ViewProps) {
  const items = arrayValue(snapshot.diagnostics, "bundles");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  return <><PageIntro kicker="OBSERVABILITY / DIAGNOSTICS" title="Diagnostics" detail="Bundles are bounded, redacted, and limited to Nexus-owned metadata and text logs." /><Panel title="Diagnostic bundles" icon={<TerminalWindow size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{items.length} bundles</span><ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction("Diagnostic collection", "/launcher/agent-api/v1/diagnostics", { action: "collect", note: "Native launcher collection" })}><TerminalWindow size={16} />Collect diagnostics</ActionButton></div><DataList items={items} emptyTitle="No diagnostic bundles" emptyDetail="Collect a bounded bundle when a runtime issue needs review." render={(item) => <><div><strong>{stringValue(item, "id") || "Bundle"}</strong><span>{`${arrayValue(item, "files").length} files`}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"))}</span></>} /></Panel></>;
}

function SettingsView({ snapshot, themeMode, setThemeMode }: ViewProps) {
  const config = asObject(snapshot.config);
  const harness = nestedValue(config, "harness");
  const update = nestedValue(config, "update");
  return <><PageIntro kicker="SYSTEM / SETTINGS" title="Settings" detail="Configuration remains Agent-owned. This view intentionally exposes metadata, not credentials or raw environment values." /><div className="grid-two"><Panel title="Appearance" icon={<Gear size={18} />}><label className="field-label" htmlFor="theme-mode">Theme</label><select id="theme-mode" className="theme-select" value={themeMode} onChange={(event) => setThemeMode(event.target.value as ThemeMode)}><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select><p className="field-help">System follows the operating system preference. Your choice is saved locally.</p></Panel><Panel title="Harness configuration" icon={<Gear size={18} />}>{Object.keys(harness).length ? <dl className="detail-list"><div><dt>Program</dt><dd>{stringValue(harness, "program") || "Not configured"}</dd></div><div><dt>Working directory</dt><dd>{stringValue(harness, "working_dir") || "Default"}</dd></div><div><dt>Readiness URL</dt><dd>{isLoopbackUrl(stringValue(harness, "readiness_url")) ? stringValue(harness, "readiness_url") : "Not shown"}</dd></div></dl> : <EmptyState title="Harness is not configured" detail="The Agent remains usable as a control plane until an external Harness is configured." />}</Panel><Panel title="Update configuration" icon={<CloudArrowUp size={18} />}>{Object.keys(update).length ? <dl className="detail-list"><div><dt>Source</dt><dd>{stringValue(update, "source") || "Not shown"}</dd></div><div><dt>Ref</dt><dd>{stringValue(update, "ref_name") || "Default"}</dd></div><div><dt>Git program</dt><dd>{stringValue(update, "git_program") || "git"}</dd></div></dl> : <EmptyState title="Updates are not configured" detail="Release metadata and current runtime remain available without an update source." />}</Panel></div><Panel title="Native integration" icon={<Bell size={18} />}><div className="integration-list"><div><CheckCircle size={18} /><span>Single instance guard</span><strong>Enabled</strong></div><div><Bell size={18} /><span>Desktop notifications</span><strong>Available through Tauri</strong></div><div><Key size={18} /><span>API transport</span><strong>Rust loopback proxy</strong></div></div></Panel></>;
}

function PageIntro({ kicker, title, detail }: { kicker: string; title: string; detail: string }) {
  return <div className="page-heading"><div><span className="kicker">{kicker}</span><h1>{title}</h1><p>{detail}</p></div></div>;
}

function Panel({ title, icon, children }: { title: string; icon: React.ReactNode; children: React.ReactNode }) {
  return <section className="panel"><div className="panel-header"><div className="panel-title">{icon}<h2>{title}</h2></div></div>{children}</section>;
}

export default App;
