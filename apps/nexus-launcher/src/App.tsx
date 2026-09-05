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
  coldOperationIsTerminal,
  createLatestRequest,
  harnessControlGate,
  invalidatesHarnessCredentials,
  launcherContentMode,
  recoveryMutationGate,
  runtimeSettingsGate,
} from "./control-state";
import { useI18n, type Locale, type Translator } from "./i18n";

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
  recovery: JsonObject | null;
  config: JsonObject | null;
};

type ModuleId =
  | "overview"
  | "profiles"
  | "updates"
  | "diagnostics"
  | "settings";

type ThemeMode = "system" | "light" | "dark";
type HarnessLaunchMode = "direct" | "node";

type ModuleDefinition = {
  id: ModuleId;
  label: string;
  icon: IconComponent;
};

const modules: ModuleDefinition[] = [
  { id: "overview", label: "Overview", icon: House },
  { id: "profiles", label: "Profiles", icon: SlidersHorizontal },
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
  recovery: null,
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
  recovery: "/v1/recovery",
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
  const pid = numberValue(runtime, "pid");
  const attachedProcess = pid !== undefined && pid > 0;
  // A recovered Harness descendant is intentionally PID-less. The Agent may
  // publish its UI credentials only after rotating the durable log boundary;
  // require that reservation marker here as defense in depth for older or
  // racing responses. This never enables lifecycle controls, which retain the
  // separate PID gate in control-state.ts.
  const recoveredProcess = pid === undefined && response.log_session_launch_pending === true;
  return (
    !credentialInvalidationPending &&
    stringValue(runtime, "state") === "running" &&
    (attachedProcess || recoveredProcess) &&
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

function formatTimestamp(value: unknown, unavailable: string, locale: Locale): string {
  if (typeof value !== "number" || value <= 0) return unavailable;
  return new Date(value * 1000).toLocaleString(locale === "zh" ? "zh-CN" : "en-US");
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "The native bridge returned an unknown error";
}

function localizeBackendError(message: string, t: Translator): string {
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
  if (normalized.includes("harness node launch mode requires a node runtime program")) {
    return t("Node mode requires a Node runtime executable.");
  }
  if (normalized.includes("harness node launch mode requires an entry script") || normalized.includes("harness node entry must be non-empty")) {
    return t("A Harness entry is required for Node mode.");
  }
  if (normalized.includes("only http:// or tcp:// loopback readiness urls")) {
    return t("Readiness target must use HTTP or TCP loopback.");
  }
  if (normalized.includes("tcp readiness urls cannot contain a path")) {
    return t("TCP readiness targets cannot contain a path.");
  }
  if (normalized.includes("tcp readiness urls cannot contain a query")) {
    return t("TCP readiness targets cannot contain a query.");
  }
  if (normalized.includes("readiness urls cannot contain a fragment")) {
    return t("Readiness targets cannot contain a fragment.");
  }
  if (normalized.includes("tcp readiness urls must include an explicit port")) {
    return t("TCP readiness targets require an explicit port.");
  }
  if (normalized.includes("readiness url has no host")) {
    return t("Readiness target must include a host.");
  }
  if (normalized.includes("readiness url has an invalid") || normalized.includes("readiness url port must be")) {
    return t("Readiness target has an invalid port or host.");
  }
  if (normalized.includes("readiness url must target localhost")) {
    return t("Readiness target must use a loopback host.");
  }
  if (normalized.includes("harness token-bound readiness requires a readiness url")) {
    return t("Token-bound readiness requires a readiness URL.");
  }
  if (normalized.includes("agent is not responding at") || normalized.includes("agent api is not responding")) {
    return t("The Agent API is not responding on its loopback port.");
  }
  if (normalized.includes("native bridge returned an unknown error")) {
    return t("The native bridge returned an unknown error");
  }
  if (normalized.includes("runtime status response is invalid")) {
    return t("Runtime status response is invalid.");
  }
  return t("Backend error: {message}", { message });
}

function localizedRuntimeState(value: unknown, t: Translator): string {
  const state = typeof value === "string" ? value.toLowerCase() : "";
  switch (state) {
    case "ok": return t("Healthy");
    case "running": return t("Running");
    case "starting": return t("Starting");
    case "stopping": return t("Stopping");
    case "shutting_down": return t("Shutting down");
    case "stopped": return t("Stopped");
    case "failed": return t("Failed");
    case "succeeded": return t("Succeeded");
    case "detached": return t("Detached");
    case "idle": return t("idle");
    case "ready": return t("Ready");
    case "pending": return t("Pending");
    case "queued": return t("Queued");
    case "cloning": return t("Cloning");
    case "planning": return t("Planning");
    case "awaiting_confirmation": return t("Awaiting confirmation");
    case "supplying": return t("Supplying runtimes");
    case "installing": return t("Installing");
    case "building": return t("Building");
    case "verifying": return t("Verifying");
    case "registering": return t("Registering");
    case "promoting": return t("Promoting");
    case "cancelling": return t("Cancelling");
    case "cancelled": return t("Cancelled");
    case "manual": return t("Manual");
    case "healthy": return t("Healthy");
    case "legacy_metadata_only": return t("Legacy metadata only");
    case "materialization_pending": return t("Materialization pending");
    case "prepared": return t("Prepared");
    case "applied": return t("Applied");
    case "committed": return t("Committed");
    case "rolled_back": return t("Rolled back");
    case "registered": return t("Registered");
    case "available": return t("Available");
    default: return typeof value === "string" && value
      ? t("Unknown state: {state}", { state: value })
      : t("Unknown");
  }
}

function updateStateLabel(update: JsonObject, t: Translator): string {
  const state = stringValue(update, "state");
  return state ? localizedRuntimeState(state, t) : t("Update queue idle");
}

export function isRecoverableNoopError(message: string): boolean {
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

export function isLoopbackReadinessTarget(value: string | undefined): value is string {
  if (!value) return false;
  try {
    const url = new URL(value);
    if (!["http:", "tcp:"].includes(url.protocol)
      || !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
      || url.username
      || url.password
      || /[\u0000-\u001f\u007f]/.test(value)) return false;
    const port = Number(url.port || (url.protocol === "http:" ? "80" : "0"));
    if (!Number.isInteger(port) || port <= 0 || port > 65535) return false;
    if (url.protocol === "tcp:") {
      return Boolean(url.port) && (url.pathname === "" || url.pathname === "/") && !url.search && !url.hash;
    }
    return !url.hash;
  } catch {
    return false;
  }
}

function booleanValue(value: unknown, key: string): boolean {
  return asObject(value)[key] === true;
}

function isLoopbackUrl(value: string | undefined): value is string {
  if (!value) return false;
  try {
    const url = new URL(value);
    return url.protocol === "http:"
      && ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
      && !url.username
      && !url.password
      && !/[\u0000-\u001f\u007f]/.test(value);
  } catch {
    return false;
  }
}

export type HarnessConfigDraft = {
  mode: HarnessLaunchMode;
  program: string;
  entry: string;
  args: string;
  workingDir: string;
  readinessUrl: string;
  timeout: string;
  readinessTokenRequired: boolean;
  readinessUrlRedacted: boolean;
  argsRedacted: boolean;
  replaceRedactedArgs: boolean;
};

const emptyHarnessDraft: HarnessConfigDraft = {
  mode: "direct",
  program: "",
  entry: "",
  args: "",
  workingDir: "",
  readinessUrl: "",
  timeout: "",
  readinessTokenRequired: false,
  readinessUrlRedacted: false,
  argsRedacted: false,
  replaceRedactedArgs: false,
};

export function harnessDraftFromConfig(config: JsonObject): HarnessConfigDraft {
  const harness = nestedValue(config, "harness");
  const args = arrayValue(harness, "args").filter((item): item is string => typeof item === "string");
  const argsRedacted = args.some((item) => item.includes("[REDACTED]"));
  const mode = harnessLaunchMode(harness);
  const configuredEntry = stringValue(harness, "entry") || "";
  const entry = mode === "node" ? configuredEntry || args[0] || "" : "";
  const visibleArgs = mode === "node" && !configuredEntry ? args.slice(1) : args;
  return {
    mode,
    program: stringValue(harness, "program") || "",
    entry,
    args: visibleArgs.join("\n"),
    workingDir: stringValue(harness, "working_dir") || "",
    readinessUrl: stringValue(harness, "readiness_url") || "",
    timeout: numberValue(harness, "readiness_timeout_secs")?.toString() || "",
    readinessTokenRequired: booleanValue(harness, "readiness_token_required"),
    readinessUrlRedacted: booleanValue(config, "harness_readiness_url_redacted"),
    argsRedacted,
    replaceRedactedArgs: false,
  };
}

function harnessLaunchMode(value: unknown): HarnessLaunchMode {
  const mode = stringValue(value, "mode")?.toLowerCase();
  return mode === "node" ? "node" : "direct";
}

export type HarnessCandidate = {
  id: string;
  mode: HarnessLaunchMode;
  program: string;
  entry: string;
  args: string[];
  workingDir: string;
  readinessUrl: string;
  readinessTimeout: string;
  readinessTokenRequired: boolean;
  version: string;
  source: string;
  displayName: string;
};

export function harnessCandidates(value: unknown): HarnessCandidate[] {
  const response = asObject(value);
  const nested = nestedValue(response, "harness");
  const items = arrayValue(response, "candidates").length
    ? arrayValue(response, "candidates")
    : arrayValue(nested, "candidates");
  return items.filter(isObject).map((item, index) => {
    const rawMode = stringValue(item, "mode") || stringValue(item, "kind") || stringValue(item, "type");
    const mode: HarnessLaunchMode = rawMode?.toLowerCase().includes("node") ? "node" : "direct";
    const rawArgs = arrayValue(item, "args").filter((arg): arg is string => typeof arg === "string");
    const configuredEntry = stringValue(item, "entry") || stringValue(item, "entry_point") || "";
    const entry = mode === "node" ? configuredEntry || rawArgs[0] || "" : "";
    const args = mode === "node" && !configuredEntry ? rawArgs.slice(1) : rawArgs;
    const program = stringValue(item, "program")
      || stringValue(item, "executable")
      || stringValue(item, "node_executable")
      || "";
    const workingDir = stringValue(item, "working_dir") || stringValue(item, "project_dir") || "";
    const readinessUrl = stringValue(item, "readiness_url") || "";
    const readinessTimeout = numberValue(item, "readiness_timeout_secs")?.toString() || "";
    const readinessTokenRequired = booleanValue(item, "readiness_token_required");
    const id = stringValue(item, "id") || `${mode}:${program}:${entry}:${index}`;
    return {
      id,
      mode,
      program,
      entry,
      args,
      workingDir,
      readinessUrl,
      readinessTimeout,
      readinessTokenRequired,
      version: stringValue(item, "version") || "",
      source: stringValue(item, "source") || "",
      displayName: stringValue(item, "display_name") || stringValue(item, "name") || program || id,
    };
  }).filter((candidate) => candidate.program.length > 0);
}

function candidateModeLabel(mode: HarnessLaunchMode, t: Translator): string {
  return mode === "node" ? t("Node runtime") : t("Direct executable");
}

/** Serialize the editor's split Node fields at the compatibility boundary. */
export function harnessConfigPayloadFromDraft(draft: HarnessConfigDraft): JsonObject {
  const entry = draft.entry.trim();
  const additionalArgs = draft.args
    .split(/\r?\n/)
    .map((value) => value.trim())
    .filter(Boolean);
  return {
    mode: draft.mode,
    program: draft.program.trim(),
    entry: draft.mode === "node" ? entry : null,
    args: additionalArgs,
    args_are_additional: draft.mode === "node",
    working_dir: draft.workingDir.trim() || null,
    readiness_url: draft.readinessUrl.trim() || null,
    readiness_timeout_secs: draft.timeout.trim() ? Number(draft.timeout.trim()) : null,
    readiness_token_required: draft.readinessTokenRequired,
  };
}

function discoverySourceLabel(source: string, t: Translator): string {
  switch (source) {
    case "configured": return t("Configured search root");
    case "current_dir": return t("Current directory");
    case "current_exe": return t("Launcher directory");
    case "data_root": return t("Nexus data directory");
    case "data_root_parent": return t("Nexus data parent");
    case "home": return t("User home");
    case "path": return t("PATH");
    default: return t("Local search");
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

const runtimeToolNames = ["git", "node", "pnpm"] as const;
type RuntimeToolName = typeof runtimeToolNames[number];
type RuntimeToolSource = "system" | "nexus";

export type RuntimeToolStatus = {
  name: RuntimeToolName;
  available: boolean;
  version?: string;
  source?: RuntimeToolSource;
  path?: string;
  reason?: string;
};

export type RuntimeStatusPayload = {
  api_version: string | number;
  tools: RuntimeToolStatus[];
};

export type RuntimeStatusViewState = {
  phase: "idle" | "loading" | "success" | "error";
  status: RuntimeStatusPayload | null;
  error: string | null;
};

export type RuntimeStatusPanelProps = {
  agentAvailable: boolean;
  state: RuntimeStatusViewState;
  onCheck: () => void;
};

export type RuntimeStatusTransport = (path: string, method: "GET") => Promise<unknown>;

export type RuntimeStatusController = {
  getState: () => RuntimeStatusViewState;
  check: (agentAvailable: boolean) => Promise<RuntimeStatusViewState>;
};

function isRuntimeToolName(value: string | undefined): value is RuntimeToolName {
  return value !== undefined && runtimeToolNames.includes(value as RuntimeToolName);
}

function isAbsoluteRuntimePath(value: string): boolean {
  return value.startsWith("/") || /^\\\\[^\\/]+[\\/][^\\/]+/.test(value) || /^[A-Za-z]:[\\/]/.test(value);
}

function optionalRuntimeString(item: JsonObject, key: string): string | undefined {
  const value = item[key];
  if (value === undefined) return undefined;
  if (typeof value !== "string" || !value.trim()) {
    throw new Error("Runtime status response is invalid.");
  }
  return value;
}

export function runtimeStatusFromResponse(value: unknown): RuntimeStatusPayload {
  const response = asObject(value);
  const apiVersion = response.api_version;
  if (
    (typeof apiVersion !== "string" || !apiVersion.trim()) &&
    (typeof apiVersion !== "number" || !Number.isFinite(apiVersion))
  ) {
    throw new Error("Runtime status response is invalid.");
  }

  const rawTools = arrayValue(response, "tools");
  if (rawTools.length !== runtimeToolNames.length) {
    throw new Error("Runtime status response is invalid.");
  }
  const tools = new Map<RuntimeToolName, RuntimeToolStatus>();
  for (const item of rawTools) {
    if (!isObject(item)) throw new Error("Runtime status response is invalid.");
    const name = item.name;
    if (typeof name !== "string" || !isRuntimeToolName(name) || tools.has(name) || typeof item.available !== "boolean") {
      throw new Error("Runtime status response is invalid.");
    }
    const version = optionalRuntimeString(item, "version");
    const sourceValue = optionalRuntimeString(item, "source");
    const path = optionalRuntimeString(item, "path");
    const reason = optionalRuntimeString(item, "reason");
    const source = sourceValue === "system" || sourceValue === "nexus" ? sourceValue : undefined;
    if (sourceValue !== undefined && source === undefined) {
      throw new Error("Runtime status response is invalid.");
    }
    if (path !== undefined && !isAbsoluteRuntimePath(path)) {
      throw new Error("Runtime status response is invalid.");
    }
    if (item.available && (
      version === undefined ||
      source === undefined ||
      path === undefined
    )) {
      throw new Error("Runtime status response is invalid.");
    }
    tools.set(name, { name, available: item.available, version, source, path, reason });
  }
  if (tools.size !== runtimeToolNames.length) {
    throw new Error("Runtime status response is invalid.");
  }
  return {
    api_version: apiVersion,
    tools: runtimeToolNames.map((name) => tools.get(name) as RuntimeToolStatus),
  };
}

export function createRuntimeStatusController(
  transport: RuntimeStatusTransport,
  publish: (state: RuntimeStatusViewState) => void = () => undefined,
): RuntimeStatusController {
  let state: RuntimeStatusViewState = { phase: "idle", status: null, error: null };
  const update = (next: RuntimeStatusViewState): RuntimeStatusViewState => {
    state = next;
    publish(state);
    return state;
  };
  return {
    getState: () => state,
    check: async (agentAvailable) => {
      if (!agentAvailable) return state;
      update({ phase: "loading", status: null, error: null });
      try {
        const response = await transport("/v1/runtime", "GET");
        return update({ phase: "success", status: runtimeStatusFromResponse(response), error: null });
      } catch (cause) {
        return update({ phase: "error", status: null, error: errorMessage(cause) });
      }
    },
  };
}

function runtimeToolLabel(name: RuntimeToolName, t: Translator): string {
  return t(name === "git" ? "Git" : name === "node" ? "Node" : "pnpm");
}

function runtimeToolSourceLabel(source: RuntimeToolSource, t: Translator): string {
  return source === "system" ? t("System source") : t("Nexus source");
}

function runtimeToolReason(reason: string | undefined, t: Translator): string {
  switch (reason) {
    case "not_found":
      return t("Runtime tool was not found. Install it or configure its path, then retry.");
    case "corepack_shim_unverified":
      return t("Corepack shim could not be verified; pnpm status cannot be confirmed.");
    default:
      return t("This runtime could not be verified.");
  }
}

function RuntimeToolRow({ tool }: { tool: RuntimeToolStatus }) {
  const { t } = useI18n();
  return (
    <div className="data-row">
      <div>
        <strong>{runtimeToolLabel(tool.name, t)}</strong>
        <StatusPill label={tool.available ? t("Available") : t("Unavailable")} tone={tool.available ? "good" : "warn"} />
      </div>
      <div>
        {tool.version && <span>{t("Version")}: <code>{tool.version}</code></span>}
        {tool.source && <span>{t("Source")}: <code>{runtimeToolSourceLabel(tool.source, t)}</code></span>}
        {tool.path && <span>{t("Path")}: <code>{tool.path}</code></span>}
        {!tool.available && <span>{runtimeToolReason(tool.reason, t)}</span>}
      </div>
    </div>
  );
}

export function RuntimeStatusPanel({ agentAvailable, state, onCheck }: RuntimeStatusPanelProps) {
  const { t } = useI18n();
  const checkDisabled = !agentAvailable || state.phase === "loading";
  let content: React.ReactNode;
  if (!agentAvailable) {
    content = <EmptyState title={t("Agent unavailable")} detail={t("The Agent is unavailable. Reconnect the Agent before checking runtime status.")} />;
  } else if (state.phase === "idle") {
    content = <EmptyState title={t("Runtime status not checked")} detail={t("Click Check runtime to inspect Git, Node, and pnpm.")} />;
  } else if (state.phase === "loading") {
    content = (
      <div className="state-card loading-state" role="status" aria-live="polite">
        <Pulse size={22} className="spin" aria-hidden="true" />
        <div><strong>{t("Checking runtime...")}</strong><span>{t("Reading the Agent runtime status.")}</span></div>
      </div>
    );
  } else if (state.phase === "error") {
    const message = state.error ? localizeBackendError(state.error, t) : t("Unknown");
    content = (
      <div className="state-card error-state" role="alert">
        <WarningCircle size={25} aria-hidden="true" />
        <div className="state-copy"><strong>{t("Runtime status unavailable")}</strong><span>{t("Runtime status request failed: {message}", { message: compactError(message) })}</span></div>
        <ActionButton onClick={onCheck}><ArrowClockwise size={16} />{t("Retry")}</ActionButton>
      </div>
    );
  } else if (state.status) {
    content = state.status.tools.length ? (
      <div className="data-list" aria-label={t("Runtime tools")}>
        {state.status.tools.map((tool) => <RuntimeToolRow key={tool.name} tool={tool} />)}
      </div>
    ) : <EmptyState title={t("No runtime tools reported")} detail={t("The Agent returned no tool entries to display.")} />;
  } else {
    content = <EmptyState title={t("Runtime status unavailable")} detail={t("This runtime could not be verified.")} />;
  }

  return (
    <Panel title={t("Runtime status")} icon={<Cpu size={18} />}>
      <p className="panel-description">{t("Runtime status is checked manually. It never downloads or installs tools.")}</p>
      <div className="panel-toolbar">
        <span className="toolbar-count">{state.phase === "success" && state.status ? t("API {version}", { version: String(state.status.api_version) }) : t("Manual check")}</span>
        <ActionButton disabled={checkDisabled} onClick={onCheck}>
          {state.phase === "loading" ? <Pulse size={16} className="spin" /> : <ArrowClockwise size={16} />}
          {state.phase === "loading" ? t("Checking runtime...") : state.phase === "success" ? t("Refresh runtime status") : t("Check runtime")}
        </ActionButton>
      </div>
      {content}
    </Panel>
  );
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
  const { locale, t } = useI18n();
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
    const isNativeAgentLifecycle = path === "/v1/agent";
    if (snapshot.startup === null || (snapshot.startup.available !== true && !isNativeAgentLifecycle)) {
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
    let rawActionError: string | null = null;
    let actionSucceeded = false;
    try {
      await proxyRequest(path, "POST", body);
      actionSucceeded = true;
      setNotice(`${label} ${t("complete")}`);
    } catch (cause) {
      rawActionError = errorMessage(cause);
      actionError = `${label} ${t("failed")}: ${localizeBackendError(rawActionError, t)}`;
    } finally {
      // Refresh after both successful and failed POSTs. The Agent may have
      // advanced a generation before returning an error (for example an
      // unattached stop), and the UI must not leave the prior snapshot visible.
      await refresh();
      if (actionError) {
        // A rejected no-op (for example Start while Harness is already
        // running) did not cross a lifecycle boundary. Restore the current
        // session instead of leaving the token/iframe locked forever.
        // Match the stable backend English error before localization. The
        // localized text intentionally changes with the selected UI language
        // and must never affect lifecycle/credential state handling.
        if (invalidatesCredentials && rawActionError !== null && isRecoverableNoopError(rawActionError)) {
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
    const common = { snapshot, busyAction, credentialInvalidationPending, runAction, refresh, themeMode, setThemeMode, openSettings: () => setActiveModule("settings") };
    switch (activeModule) {
      case "profiles": return <ProfilesView {...common} />;
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
  openSettings?: () => void;
};

type HarnessPanelProps = Pick<ViewProps, "snapshot" | "busyAction" | "runAction">;
type HarnessAuthPanelProps = HarnessPanelProps & Pick<ViewProps, "credentialInvalidationPending">;
type HarnessWebPanelProps = Pick<ViewProps, "snapshot" | "credentialInvalidationPending" | "busyAction" | "runAction">;

export function OverviewView({ snapshot, busyAction, credentialInvalidationPending, runAction, openSettings }: ViewProps) {
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
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  const agentStarting = agentLifecycle === "starting";
  const agentStopping = agentLifecycle === "stopping";
  const agentControlsUnavailable = busyAction !== null || snapshot.startup === null;
  const agentRestartDisabled = agentControlsUnavailable || agentStarting || agentStopping;
  const [failLogOpen, setFailLogOpen] = useState(false);
  return (
    <>
      <div className="page-heading"><div><span className="kicker">{t("Runtime / Overview")}</span><h1>{t("Local control plane")}</h1><p>{t("Observe and operate the independent Agent and its immutable Harness runtime.")}</p></div><StatusPill label={agentRunning ? t("Running") : t("Standby")} tone={agentRunning ? "good" : "warn"} /></div>
      {(() => {
        const harnessState = stringValue(harness, "state");
        const controlGate = harnessControlGate(harnessState, numberValue(harness, "pid"), busyAction !== null, snapshot.startup?.available === true);
        const startDisabled = controlGate.controlsDisabled || harnessState === "running" || harnessState === "starting" || harnessState === "stopping";
        const restartDisabled = controlGate.controlsDisabled || harnessState === "starting" || harnessState === "stopping" || harnessState === "detached";
        const stopDisabled = controlGate.controlsDisabled || !["running", "starting", "failed"].includes(harnessState || "");
        const harnessAction = (action: string) => void runAction(t(`Harness ${action}`), "/v1/harness", { action });
        return <div className="metric-grid">
        <div className="metric-with-actions"><Metric label={t("Agent lifecycle")} value={localizedRuntimeState(stringValue(state, "lifecycle"), t)} detail={localizedRuntimeState(stringValue(health, "status"), t)} /><div className="button-row"><ActionButton tone="primary" disabled={agentRestartDisabled} onClick={() => void runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })}><ArrowsClockwise size={16} />{t("Force restart Agent")}</ActionButton></div></div>
        <div className="metric-with-actions"><Metric label={t("Harness")} value={localizedRuntimeState(harnessState, t)} detail={stringValue(harness, "pid") ? t("PID {pid}", { pid: stringValue(harness, "pid") || "" }) : t("No child process")} /><div className="button-row">{harnessState !== "running" && <ActionButton tone="primary" disabled={startDisabled} onClick={() => harnessAction("start")}><CheckCircle size={16} />{t("Start")}</ActionButton>}{(harnessState === "running" || harnessState === "failed") && <ActionButton disabled={restartDisabled} onClick={() => harnessAction("restart")}><ArrowsClockwise size={16} />{t("Restart")}</ActionButton>}{(harnessState === "running" || harnessState === "starting") && <ActionButton tone="danger" disabled={stopDisabled} onClick={() => harnessAction("stop")}><StopCircle size={16} />{t("Stop")}</ActionButton>}</div><dl className="detail-list compact-details"><div><dt>{t("Process ID")}</dt><dd>{stringValue(harness, "pid") || t("Not attached")}</dd></div><div><dt>{t("Exit code")}</dt><dd>{stringValue(harness, "exit_code") || t("Not exited")}</dd></div><div><dt>{t("Last error")}</dt><dd>{harness.error ? localizeBackendError(stringValue(harness, "error") || "", t) : t("None reported")}</dd></div></dl>{harnessState === "failed" && <div className="button-row"><ActionButton onClick={() => setFailLogOpen(!failLogOpen)}>{failLogOpen ? t("Hide startup log tail") : t("Show startup log tail")}</ActionButton></div>}{harnessState === "failed" && failLogOpen && <RecoveryLogTail snapshot={snapshot} />}</div>
        <Metric label={t("Active profile")} value={stringValue(state, "profile") || t("None selected")} detail={t("{count} profiles available", { count: profiles.length })} />
        <Metric label={t("Checkpoints")} value={String(checkpoints.length)} detail={updateStateLabel(update, t)} />
        </div>;
      })()}
      <HarnessAuthPanel snapshot={snapshot} busyAction={busyAction} credentialInvalidationPending={credentialInvalidationPending} runAction={runAction} />
      <HarnessWebPanel snapshot={snapshot} credentialInvalidationPending={credentialInvalidationPending} busyAction={busyAction} runAction={runAction} />
    </>
  );
}

function HarnessControlPanel({ snapshot, busyAction, runAction, openSettings }: HarnessPanelProps & Pick<ViewProps, "openSettings">) {
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
      {state === "detached" && <><p className="field-help" role="status">{t("Harness is detached. Configure it in Settings, then start it from the control panel.")}</p>{openSettings && <div className="button-row"><ActionButton onClick={openSettings}><Gear size={16} />{t("Configure Harness")}</ActionButton></div>}</>}
      <dl className="detail-list compact-details">
        <div><dt>{t("Process ID")}</dt><dd>{stringValue(harness, "pid") || t("Not attached")}</dd></div>
        <div><dt>{t("Exit code")}</dt><dd>{stringValue(harness, "exit_code") || t("Not exited")}</dd></div>
        <div><dt>{t("Last error")}</dt><dd>{harness.error ? localizeBackendError(stringValue(harness, "error") || "", t) : t("None reported")}</dd></div>
      </dl>
    </Panel>
  );
}

function HarnessAuthPanel({ snapshot, busyAction, credentialInvalidationPending, runAction }: HarnessAuthPanelProps) {
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
          <div className="metadata-grid"><div><span>{t("Source")}</span><strong>{stringValue(info, "source") || t("Not available")}</strong></div><div><span>{t("Observed")}</span><strong>{formatTimestamp(numberValue(info, "observed_at_unix"), t("Not available"), locale)}</strong></div></div>
          <div className="button-row"><ActionButton disabled={!token || controlsDisabled} onClick={() => void navigator.clipboard?.writeText(token || "")}><ClipboardText size={16} />{t("Copy token")}</ActionButton><ActionButton tone="primary" disabled={!uiUrl || controlsDisabled} onClick={openSystemBrowser}><RocketLaunch size={16} />{t("Open in system browser")}</ActionButton></div>
    </Panel>
  );
}

export function HarnessWebPanel({ snapshot, credentialInvalidationPending, busyAction, runAction }: HarnessWebPanelProps) {
  const { t } = useI18n();
  const info = asObject(snapshot.harnessUi);
  const currentUiAvailable = harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi, credentialInvalidationPending);
  const uiUrl = currentUiAvailable
    ? stringValue(info, "url")
    : undefined;
  const tokenMode = stringValue(info, "token") !== undefined;
  const safeUrl = isLoopbackUrl(uiUrl) ? uiUrl : undefined;
  const browserActionDisabled = busyAction !== null || snapshot.startup?.available !== true || !currentUiAvailable;
  const openSystemBrowser = () => void runAction(t("Open Harness"), "/v1/harness/ui", { action: "open" });
  return <Panel title={t("Embedded Harness Web")} icon={<MonitorPlay size={18} />}>
    {tokenMode ? <div className="status-block"><strong>{t("Harness authentication requires a system browser")}</strong><span>{t("This session needs top-level browser authentication. Open the validated Harness page in your system browser to sign in.")}</span><div className="button-row"><ActionButton tone="primary" disabled={browserActionDisabled} onClick={openSystemBrowser}><RocketLaunch size={16} />{t("Open in system browser")}</ActionButton></div></div> : safeUrl ? <iframe className="harness-frame" title={t("Harness Web interface")} src={safeUrl} referrerPolicy="no-referrer" sandbox="allow-forms allow-scripts allow-same-origin allow-downloads allow-popups allow-popups-to-escape-sandbox" /> : <EmptyState title={t("Harness view is not ready")} detail={t("A validated loopback HTTP URL will appear here when Harness reports its web interface.")} />}
  </Panel>;
}

export function ProfilesView(props: ViewProps) {
  const { t } = useI18n();
  const { snapshot, busyAction, runAction } = props;
  const active = stringValue(snapshot.profiles, "active_profile");
  const [expandedProfiles, setExpandedProfiles] = useState<string[]>([]);
  const toggleProfile = (name: string) => setExpandedProfiles((current) => current.includes(name) ? current.filter((item) => item !== name) : [...current, name]);
  const manifests = arrayValue(snapshot.profiles, "manifests");
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), harness.state, busyAction !== null);
  return <><PageIntro kicker={t("Control / Profiles")} title={t("Profiles")} detail={t("Profiles own checkpoints and the plugin inventory: select a profile, manage its checkpoints, then adjust its plugins. Profile creation and deletion are unavailable in this release.")} /><Panel title={t("Profile catalog")} icon={<SlidersHorizontal size={18} />}>
    {gate.reason === "stop_required" || gate.reason === "not_stopped" ? <p className="form-error"><WarningCircle size={15} />{t("Stop Harness before switching profiles or removing plugins.")}</p> : null}
    <DataList items={manifests} emptyTitle={t("No valid native profiles")} emptyDetail={t("Only valid profile manifests are selectable.")} render={(item) => { const name = stringValue(item, "name") || t("Unnamed profile"); const expanded = expandedProfiles.includes(name); return <><div className="profile-row-toggle" onClick={() => toggleProfile(name)}><span className="profile-chevron" aria-hidden="true">{expanded ? "▾" : "▸"}</span><strong>{name}</strong>{name === active && <StatusPill label={t("Active")} tone="good" />}<span>{t("{count} bundles", { count: arrayValue(item, "bundles").length })}</span></div><span className="row-meta">{name !== active && <ActionButton disabled={gate.disabled} onClick={() => void runAction(t("Profile selection"), "/v1/profiles", { action: "select", profile: name })}>{t("Select")}</ActionButton>}</span></>; }} />
  </Panel>
  {manifests.map(asObject).map((item) => { const name = stringValue(item, "name"); if (!name || !expandedProfiles.includes(name)) return null; return <div key={name} className="profile-children"><div className="profile-children-title">{t("Belongs to profile")}: {name}</div>
  <CheckpointsView {...props} embedded profileFilter={name} />
  <ProfilePlugins {...props} profile={name} />
  </div>; })}
  </>;
}

export function CheckpointsView({ snapshot, busyAction, runAction, embedded, profileFilter }: ViewProps & { embedded?: boolean; profileFilter?: string }) {
  const { locale, t } = useI18n();
  const items = arrayValue(snapshot.checkpoints, "checkpoints").filter((item) => !profileFilter || stringValue(item, "profile") === profileFilter);
  const snapshots = arrayValue(snapshot.checkpoints, "snapshots").filter((item) => !profileFilter || stringValue(asObject(asObject(item).summary), "profile_name") === profileFilter);
  const [detail, setDetail] = useState<JsonObject | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const latest = useRef(createLatestRequest());
  useEffect(() => () => latest.current.cancel(), []);
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), harness.state, busyAction !== null);
  const pending = asObject(asObject(snapshot.checkpoints).pending_restore);
  const healthyError = stringValue(snapshot.checkpoints, "healthy_capture_error");
  const loadDetail = async (id: string, action: "detail" | "inspect") => {
    const token = latest.current.begin(); setDetailLoading(true); setDetailError(null);
    try { const value = await proxyRequest<JsonObject>("/v1/checkpoints", "POST", { action, id }); if (latest.current.isCurrent(token)) setDetail(value); }
    catch (cause) { if (latest.current.isCurrent(token)) { setDetail(null); setDetailError(errorMessage(cause)); } }
    finally { if (latest.current.isCurrent(token)) setDetailLoading(false); }
  };
  return <>{!embedded && <PageIntro kicker={t("State / Checkpoints")} title={t("Checkpoints")} detail={`${t("Checkpoint manifests contain only Harness profile/release selection. Agent lifecycle and Harness runtime are never saved or restored.")} ${t("Manual checkpoints contain a bounded redacted snapshot. Legacy entries restore selection metadata only.")}`} />}
    {!embedded && healthyError && <div className="notice action-error"><WarningCircle size={17} /><span>{t("Healthy snapshot capture failed")}: {healthyError}</span></div>}
    {!embedded && Object.keys(pending).length > 0 && <Panel title={t("Pending restore")} icon={<WarningCircle size={18} />}><dl className="detail-list compact-details"><div><dt>{t("Checkpoint")}</dt><dd>{stringValue(pending, "checkpoint_id")}</dd></div><div><dt>{t("State")}</dt><dd>{localizedRuntimeState(stringValue(pending, "state"), t)}</dd></div><div><dt>{t("Last error")}</dt><dd>{stringValue(pending, "error") || t("None reported")}</dd></div></dl><div className="button-row">{booleanValue(pending, "retryable") && <ActionButton disabled={gate.disabled} onClick={() => void runAction(t("Retry restore"), "/v1/checkpoints", { action: "retry", id: stringValue(pending, "checkpoint_id") })}>{t("Retry")}</ActionButton>}{booleanValue(pending, "abortable") && <ActionButton tone="danger" disabled={gate.disabled} onClick={() => void runAction(t("Abort restore"), "/v1/checkpoints", { action: "abort", id: stringValue(pending, "checkpoint_id") })}>{t("Abort")}</ActionButton>}</div></Panel>}
    <Panel title={t("Saved checkpoints")} icon={<ListChecks size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{t("{count} saved", { count: items.length })}</span><ActionButton tone="primary" disabled={gate.disabled || snapshot.startup?.available !== true} onClick={() => void runAction(t("Checkpoint creation"), "/v1/checkpoints", { action: "create", note: t("Native launcher checkpoint") })}><CheckCircle size={16} />{t("Create checkpoint")}</ActionButton></div><DataList items={items} emptyTitle={t("No checkpoints yet")} emptyDetail={t("Create a checkpoint after the Agent has a stable profile and release state.")} render={(item) => { const id = stringValue(item, "id") || ""; const reference = asObject(asObject(item).snapshot); const summary = asObject(reference.summary); const legacy = !Object.keys(reference).length; return <><div><strong>{id || t("Checkpoint")}</strong><StatusPill label={legacy ? t("Legacy metadata only") : localizedRuntimeState(stringValue(summary, "kind"), t)} tone={legacy ? "warn" : "good"}/><span>{stringValue(item, "profile") || t("No profile")} · {stringValue(summary, "dsh_version") || stringValue(item, "release") || t("Unknown version")}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"), t("Not available"), locale)} <ActionButton disabled={detailLoading || legacy} onClick={() => void loadDetail(id, "detail")}>{t("Detail")}</ActionButton><ActionButton disabled={detailLoading || legacy} onClick={() => void loadDetail(id, "inspect")}>{t("Inspect")}</ActionButton><ActionButton disabled={gate.disabled} onClick={() => void runAction(t("Restore checkpoint"), "/v1/checkpoints", { action: "restore", id })}>{t("Restore")}</ActionButton></span></>; }} /></Panel>
    <Panel title={t("Snapshot inventory")} icon={<ClipboardText size={18} />}><DataList items={snapshots} emptyTitle={t("No snapshots reported")} emptyDetail={t("Healthy and manual snapshots appear here after capture.")} render={(item) => { const summary = asObject(asObject(item).summary); const id = stringValue(item, "snapshot_id") || stringValue(summary, "snapshot_id") || ""; return <><div><strong>{id}</strong><StatusPill label={localizedRuntimeState(stringValue(summary, "kind"), t)} tone={booleanValue(item, "valid") ? "good" : "bad"}/><span>{stringValue(summary, "profile_name")} · {stringValue(summary, "dsh_version")} · {numberValue(summary, "file_count") ?? 0} {t("files")}</span></div><span className="row-meta"><ActionButton disabled={detailLoading} onClick={() => void loadDetail(id, "detail")}>{t("Detail")}</ActionButton><ActionButton disabled={detailLoading} onClick={() => void loadDetail(id, "inspect")}>{t("Inspect")}</ActionButton></span></>; }} /></Panel>
    {(detailLoading || detailError || detail) && <Panel title={t("Snapshot detail")} icon={<ClipboardText size={18} />}>{detailLoading ? <LoadingState /> : detailError ? <ErrorState title={t("Snapshot detail failed")} message={detailError} onRetry={() => { setDetail(null); setDetailError(null); }} /> : <SnapshotDetail value={detail} />}</Panel>}
  </>;
}

function SnapshotDetail({ value }: { value: JsonObject | null }) {
  const { t } = useI18n();
  const summary = asObject(asObject(value).summary);
  const files = arrayValue(value, "files");
  const errors = arrayValue(value, "errors").map(String);
  return <div className="status-block">
    <dl className="detail-list compact-details"><div><dt>{t("Snapshot")}</dt><dd>{stringValue(value, "snapshot_id") || stringValue(summary, "snapshot_id")}</dd></div><div><dt>{t("Kind")}</dt><dd>{localizedRuntimeState(stringValue(summary, "kind"), t)}</dd></div><div><dt>{t("Version")}</dt><dd>{stringValue(summary, "dsh_version")}</dd></div><div><dt>{t("Files")}</dt><dd>{files.length}</dd></div></dl>
    {errors.map((item) => <p className="form-error" key={item}>{item}</p>)}
    <DataList items={files} emptyTitle={t("No snapshot files")} emptyDetail={t("No bounded file content was returned.")} render={(item) => <div className="snapshot-file"><strong>{stringValue(item, "path")}</strong><span>{localizedRuntimeState(stringValue(item, "state"), t)} · {numberValue(item, "stored_size") ?? 0} B</span>{arrayValue(item, "redacted_paths").length > 0 && <small>{t("Redacted fields")}: {arrayValue(item, "redacted_paths").map(String).join(", ")}</small>}{stringValue(item, "omitted_reason") && <small>{stringValue(item, "omitted_reason")}</small>}{stringValue(item, "content") && <pre>{stringValue(item, "content")}</pre>}{booleanValue(item, "content_truncated") && <small className="truncation-note">{stringValue(item, "content_note") || t("Content truncated by the Agent response limit.")}</small>}</div>} />
  </div>;
}

export function UpdatesView({ snapshot, busyAction, runAction, refresh }: ViewProps) {
  const { t } = useI18n();
  const update = nestedValue(snapshot.updates, "update");
  const operation = nestedValue(snapshot.updates, "operation");
  const release = nestedValue(snapshot.updates, "release");
  const releases = arrayValue(snapshot.releases, "releases");
  const updateState = stringValue(update, "state");
  const runtime = nestedValue(snapshot.config, "runtime");
  const persistedSource = stringValue(runtime, "source") || "official";
  const persistedMode = stringValue(runtime, "mode") || "portable";
  const [source, setSource] = useState(persistedSource);
  const [mode, setMode] = useState(persistedMode);
  const [tagList, setTagList] = useState<JsonObject | null>(null);
  const [selectedTag, setSelectedTag] = useState<string>("");
  const [tagsLoading, setTagsLoading] = useState(false);
  const [tagsError, setTagsError] = useState<string | null>(null);
  const latestTags = useRef(createLatestRequest());
  const runtimeConfigKey = useRef("");
  const [runtimeSetupOpen, setRuntimeSetupOpen] = useState(false);
  const [pins, setPins] = useState<{ node: string; pnpm: string; git: string }>({ node: "", pnpm: "", git: "" });
  const [runtimeTools, setRuntimeTools] = useState<JsonObject[] | null>(null);
  useEffect(() => () => latestTags.current.cancel(), []);
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const value = await proxyRequest<JsonObject>("/v1/runtime");
        if (!cancelled) setRuntimeTools(arrayValue(value, "tools").map((tool) => asObject(tool)));
      } catch {
        if (!cancelled) setRuntimeTools([]);
      }
    })();
    return () => { cancelled = true; };
  }, []);
  useEffect(() => {
    if (runtimeTools && runtimeTools.some((tool) => !booleanValue(tool, "available"))) setRuntimeSetupOpen(true);
  }, [runtimeTools]);
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatusViewState>({
    phase: "idle",
    status: null,
    error: null,
  });
  const runtimeController = useMemo(
    () => createRuntimeStatusController(
      (path, method) => proxyRequest<unknown>(path, method),
      setRuntimeStatus,
    ),
    [],
  );
  const checkRuntime = useCallback(async () => {
    await runtimeController.check(snapshot.startup?.available === true);
  }, [runtimeController, snapshot.startup?.available]);
  useEffect(() => {
    const key = `${persistedSource}\u0000${persistedMode}`;
    if (runtimeConfigKey.current !== key) {
      runtimeConfigKey.current = key;
      setSource(persistedSource);
      setMode(persistedMode);
      setPins({
        node: stringValue(nestedValue(runtime, "node"), "path") || "",
        pnpm: stringValue(nestedValue(runtime, "pnpm"), "path") || "",
        git: stringValue(nestedValue(runtime, "git"), "path") || "",
      });
    }
  }, [persistedMode, persistedSource, runtime]);
  const tags: string[] = tagList ? arrayValue(tagList, "tags").map((tag) => String(tag)) : [];
  const loadTags = useCallback(async () => {
    const token = latestTags.current.begin();
    setTagsLoading(true);
    setTagsError(null);
    try {
      const value = await proxyRequest<JsonObject>("/v1/releases/tags");
      if (latestTags.current.isCurrent(token)) setTagList(value);
    } catch (cause) {
      if (latestTags.current.isCurrent(token)) { setTagList(null); setSelectedTag(""); setTagsError(errorMessage(cause)); }
    } finally {
      if (latestTags.current.isCurrent(token)) setTagsLoading(false);
    }
  }, []);
  const operationPhase = stringValue(operation, "phase");
  const operationId = stringValue(operation, "operation_id");
  const pendingConfirmation = operationPhase === "awaiting_confirmation";
  const supply = nestedValue(operation, "supply_plan");
  const confirmation = stringValue(operation, "confirmation") || stringValue(supply, "supply_plan_id");
  const operationMode = stringValue(operation, "mode") || stringValue(supply, "mode") || mode;
  const pinOrEmpty = (name: "node" | "pnpm" | "git") => pins[name].trim() ? { path: pins[name].trim(), ownership: "system" } : null;
  const runtimePayload = { node: pinOrEmpty("node"), pnpm: pinOrEmpty("pnpm"), git: pinOrEmpty("git"), source, mode };
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const cleanupPending = booleanValue(operation, "cleanup_pending");
  const runtimeGate = runtimeSettingsGate(stringValue(harness, "state"), numberValue(harness, "pid"), updateState, operationPhase, cleanupPending, busyAction !== null);
  const runtimeGateReason = runtimeGate.reason === "harness_not_stopped" ? t("Harness must be positively stopped before saving runtime settings.") : runtimeGate.reason === "update_active" ? t("Wait for the update to become idle before saving runtime settings.") : runtimeGate.reason === "cold_active" ? t("Wait for the cold switch to finish before saving runtime settings.") : runtimeGate.reason === "cleanup_pending" ? t("Retry cold cleanup before saving runtime settings.") : null;
  return <><PageIntro kicker={t("Releases / Updates")} title={t("Updates")} detail={t("Cold switches are asynchronous and never start Harness automatically.")} />
    <Panel title={t("Runtime settings")} icon={<Cpu size={18} />}><div className="status-block"><div className="button-row"><ActionButton onClick={() => setRuntimeSetupOpen((open) => !open)}>{runtimeSetupOpen ? t("Hide source and install mode") : t("Change runtime source or install mode")}</ActionButton></div>{runtimeSetupOpen && <div className="form-grid"><label className="form-field"><span className="field-label">{t("Source")}</span><select className="form-input" value={source} onChange={(e) => setSource(e.target.value)}><option value="official">{t("Official")}</option><option value="npmmirror">npmmirror</option></select></label><label className="form-field"><span className="field-label">{t("Install mode")}</span><select className="form-input" value={mode} onChange={(e) => setMode(e.target.value)}><option value="portable">{t("Portable")}</option><option value="system">{t("System install")}</option></select></label></div>}{runtimeSetupOpen && <p className="field-help">{t("These settings only matter when runtimes must be installed or replaced.")}</p>}<div className="form-grid">{(["node", "pnpm", "git"] as const).map((name) => <label key={name} className="form-field"><span className="field-label">{name} {t("pin")}</span><input className="form-input" value={pins[name]} placeholder={stringValue(nestedValue(runtime, name), "path") || t("Leave blank for automatic discovery")} onChange={(event) => setPins((current) => ({ ...current, [name]: event.target.value }))} /></label>)}</div><p className="field-help">{t("Manual paths are saved as system pins. Leave blank to let Nexus resolve automatically.")}</p><ActionButton disabled={runtimeGate.disabled} onClick={() => void runAction(t("Save runtime settings"), "/v1/config", { action: "set_runtime", runtime: runtimePayload })}>{t("Save runtime settings")}</ActionButton>{runtimeGateReason && <p className="field-help" role="status">{runtimeGateReason}</p>}<hr className="panel-divider" /><RuntimeStatusPanel agentAvailable={snapshot.startup?.available === true} state={runtimeStatus} onCheck={() => void checkRuntime()} /></div></Panel>
    
    <Panel title={t("Upstream tags & cold switch")} icon={<CloudArrowUp size={18} />}><div className="status-block"><ActionButton disabled={tagsLoading} onClick={() => void loadTags()}>{tagsLoading ? t("Listing tags") : t("List upstream tags")}</ActionButton>{tagsError ? <span>{tagsError} · <button className="button subtle" onClick={() => void loadTags()}>{t("Refresh")}</button></span> : <span>{tagList ? `${t("Source")}: ${stringValue(tagList, "source")}` : t("No tags loaded")}</span>}{tags.length > 0 && <select value={selectedTag} onChange={(event) => setSelectedTag(event.target.value)} aria-label={t("Upstream tags")}><option value="">{t("Select a tag")}</option>{tags.map((tag) => <option key={tag} value={tag}>{tag}</option>)}</select>}{selectedTag && <ActionButton tone="primary" disabled={busyAction !== null || tagsLoading || (!!operationId && !coldOperationIsTerminal(operationPhase))} onClick={() => void runAction(t("Switch to tag"), "/v1/updates", { action: "switch", tag: selectedTag, source, mode })}>{releases.some((item) => stringValue(item, "version") === selectedTag) ? t("Switch to tag") : t("Fetch this tag")}</ActionButton>}{selectedTag && <span>{`${t("Selected tag")}: ${selectedTag}`}</span>}</div>{pendingConfirmation && <div className="status-block"><strong>{t("Confirm runtime supply plan")}</strong><SupplyPlanDetails operation={operation} supply={supply} /><p className="form-error"><WarningCircle size={15}/>{operationMode === "system" ? t("System mode may show an installer or elevation prompt and can require restart verification.") : t("Portable mode writes only to the Nexus-owned runtime destination.")}</p><div className="button-row"><ActionButton tone="primary" disabled={!operationId || !confirmation || busyAction !== null} onClick={() => void runAction(t("Confirm cold switch"), "/v1/updates", { action: "confirm", operation_id: operationId, confirmation })}>{t("Confirm exact plan")}</ActionButton><ActionButton tone="danger" disabled={!operationId || busyAction !== null} onClick={() => void runAction(t("Cancel cold switch"), "/v1/updates", { action: "cancel", operation_id: operationId })}>{t("Cancel")}</ActionButton></div></div>}
    {(!!operationId || (updateState && updateState !== "idle")) && <><hr className="panel-divider" /><div className="status-block"><StatusPill label={localizedRuntimeState(operationPhase || updateState, t)} tone={operationPhase === "failed" || updateState === "failed" ? "bad" : pendingConfirmation ? "warn" : operationPhase === "succeeded" ? "good" : "neutral"}/><strong>{operationId || stringValue(update, "release_id") || t("No active update")}</strong>{operationId && <progress max="100" value={numberValue(operation, "progress_percent") || 0}>{numberValue(operation, "progress_percent") || 0}%</progress>}<span>{stringValue(operation, "error") || stringValue(update, "error") || t("No update error reported")}</span>{stringValue(operation, "cleanup_error") && <p className="form-error" role="alert"><WarningCircle size={15}/>{t("Cleanup error")}: {stringValue(operation, "cleanup_error")}</p>}{operationId && <span>{t("Owner quiescent")}: {booleanValue(operation, "owner_quiescent") ? t("Yes") : t("No")}{cleanupPending ? ` · ${t("Cleanup pending")}` : ""}</span>}<div className="button-row"><ActionButton disabled={busyAction !== null} onClick={() => void refresh()}>{t("Refresh")}</ActionButton>{operationId && (!coldOperationIsTerminal(operationPhase) || cleanupPending) && <ActionButton tone="danger" disabled={busyAction !== null || operationPhase === "cancelling"} onClick={() => void runAction(t("Cancel cold switch"), "/v1/updates", { action: "cancel", operation_id: operationId })}>{cleanupPending ? t("Retry cleanup") : t("Cancel")}</ActionButton>}</div></div></>}</Panel>
    <Panel title={t("Release slots")} icon={<Package size={18} />}><DataList items={releases} emptyTitle={t("No release slots")} emptyDetail={t("A successful cold switch registers and promotes its immutable slot without starting Harness.")} render={(item) => { const slotId = stringValue(item, "id") || ""; const current = stringValue(snapshot.releases, "current_release"); const lkg = stringValue(snapshot.releases, "last_known_good"); const protectedSlot = slotId === current || slotId === lkg; return <><div><strong>{slotId || t("Release")}</strong><span>{stringValue(item, "version") || t("Unknown version")}{slotId === current ? ` · ${t("Current")}` : slotId === lkg ? ` · ${t("Last known good")}` : ""}</span></div><span className="row-meta">{slotId !== current && <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Switch to this version"), "/v1/releases", { action: "promote", id: slotId })}>{t("Switch to this version")}</ActionButton>}{!protectedSlot && <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Release slot"), "/v1/releases", { action: "remove", id: slotId })}>{t("Release slot")}</ActionButton>}</span></>; }} /></Panel>
  </>;
}



function SupplyPlanDetails({ operation, supply }: { operation: JsonObject; supply: JsonObject }) {
  const { t } = useI18n();
  const effects = ["node", "pnpm"].map((name) => { const tool = asObject(supply[name]); const artifacts = arrayValue(tool, "artifacts").map((item) => stringValue(item, "kind")).filter(Boolean); return `${name}: ${stringValue(tool, "disposition") || "?"}${artifacts.length ? ` [${artifacts.join(", ")}]` : ""}`; }).join("; ");
  return <><dl className="detail-list"><div><dt>{t("Source")}</dt><dd>{stringValue(supply, "source") || stringValue(operation, "source")}</dd></div><div><dt>{t("Install mode")}</dt><dd>{stringValue(supply, "mode") || stringValue(operation, "mode")}</dd></div><div><dt>{t("Version")}</dt><dd>{stringValue(asObject(supply.node), "version") || stringValue(operation, "tag")}</dd></div><div><dt>{t("Destination")}</dt><dd>{stringValue(supply, "destination_root")}</dd></div><div><dt>{t("System effects")}</dt><dd>{effects}</dd></div><div><dt>Node</dt><dd>{stringValue(asObject(supply.node), "disposition")} · {stringValue(asObject(supply.node), "path")}</dd></div><div><dt>pnpm</dt><dd>{stringValue(asObject(supply.pnpm), "version")} · {stringValue(asObject(supply.pnpm), "disposition")} · {stringValue(asObject(supply.pnpm), "path")}</dd></div><div><dt>{t("Plan ID")}</dt><dd>{stringValue(supply, "supply_plan_id") || stringValue(operation, "confirmation")}</dd></div></dl></>;
}

export function ProfilePlugins({ snapshot, busyAction, runAction, refresh, profile }: ViewProps & { profile?: string }) {
  const { t } = useI18n();
  const [pluginBusy, setPluginBusy] = useState(false);
  const [pluginResult, setPluginResult] = useState<JsonObject | null>(null);
  const [pluginError, setPluginError] = useState<string | null>(null);
  const latestPlugin = useRef(createLatestRequest());
  useEffect(() => () => latestPlugin.current.cancel(), []);
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const active = profile || stringValue(snapshot.profiles, "active_profile") || "";
  const manifests = arrayValue(snapshot.profiles, "manifests");
  const activeManifest = manifests.map(asObject).find((item) => stringValue(item, "name") === active) || {};
  const plugins = arrayValue(activeManifest, "plugins");
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), harness.state, busyAction !== null || pluginBusy);
  const removePlugin = async (packageName: string) => {
    if (!window.confirm(t("Remove {package} from profile {profile}?", { package: packageName, profile: active }))) return;
    const token = latestPlugin.current.begin(); setPluginBusy(true); setPluginError(null); setPluginResult(null);
    try {
      const value = await proxyRequest<JsonObject>("/v1/profiles", "POST", { action: "plugin_remove", profile: active, package: packageName });
      if (latestPlugin.current.isCurrent(token)) setPluginResult(value);
    } catch (cause) {
      if (latestPlugin.current.isCurrent(token)) setPluginError(errorMessage(cause));
    } finally {
      if (latestPlugin.current.isCurrent(token)) setPluginBusy(false);
      await refresh();
    }
  };
  return <Panel title={t("Plugin inventory")} icon={<Package size={18} />}>
    {(booleanValue(recovery, "harness_stop_required") || gate.reason === "not_stopped") && <div className="notice degraded"><WarningCircle size={17}/><span>{t("Harness must be stopped before profile, plugin, or rollback changes. Diagnostics remain available.")}</span><ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Harness stop"), "/v1/harness", { action: "stop" })}>{t("Stop Harness")}</ActionButton></div>}
    <p className="panel-description">{t("Built-in plugins belong to profile bundles. Only packages marked removable can be removed.")}</p>
    <DataList items={plugins} emptyTitle={t("No plugins reported")} emptyDetail={t("Select a valid native profile to inspect its inventory.")} render={(item) => { const packageName = stringValue(item, "package") || ""; const builtin = booleanValue(item, "builtin"); const removable = booleanValue(item, "removable"); return <><div><strong>{packageName}</strong><StatusPill label={builtin ? t("Built-in") : removable ? t("Removable") : t("Protected")} tone={removable ? "warn" : "neutral"}/><span>{stringValue(item, "version") || t("Unknown version")}</span></div><span className="row-meta">{removable && <ActionButton tone="danger" disabled={gate.disabled} onClick={() => void removePlugin(packageName)}>{pluginBusy ? t("Removing") : t("Remove")}</ActionButton>}</span></>; }} />
    {pluginError && <p className="form-error"><WarningCircle size={15}/>{pluginError} <button className="button subtle" onClick={() => setPluginError(null)}>{t("Dismiss")}</button></p>}
    {pluginResult && <pre className="output-block">{[stringValue(pluginResult, "stdout"), stringValue(pluginResult, "stderr")].filter(Boolean).join("\n") || t("Plugin removed. Inventory refreshed.")}</pre>}
  </Panel>;
}

function RecoveryLogTail({ snapshot }: Pick<ViewProps, "snapshot">) {
  const { t } = useI18n();
  const recovery = asObject(snapshot.recovery);
  const tails = arrayValue(recovery, "log_tail");
  return <Panel title={t("Bounded redacted log tail")} icon={<TerminalWindow size={18} />}><DataList items={tails} emptyTitle={t("No recovery log tail")} emptyDetail={t("No current Nexus-owned Harness log session is available.")} render={(item) => <div className="snapshot-file"><strong>{stringValue(item, "stream")}</strong><pre>{stringValue(item, "content")}</pre>{booleanValue(item, "truncated") && <small className="truncation-note">{t("Log truncated by the Agent response limit.")}</small>}</div>} /></Panel>;
}

function RecoveryDiagnostics({ snapshot, busyAction, runAction, refresh }: Pick<ViewProps, "snapshot" | "busyAction" | "runAction" | "refresh">) {
  const { t } = useI18n();
  const recovery = asObject(snapshot.recovery);
  const errors = arrayValue(recovery, "diagnostic_errors").map(String);
  return <><Panel title={t("Startup recovery status")} icon={<Pulse size={18} />}><dl className="detail-list"><div><dt>{t("Harness state")}</dt><dd>{localizedRuntimeState(stringValue(asObject(recovery.harness), "state"), t)}</dd></div><div><dt>{t("Startup error")}</dt><dd>{stringValue(recovery, "startup_error") || t("None reported")}</dd></div><div><dt>{t("Fatal prefix observed")}</dt><dd>{booleanValue(recovery, "fatal_prefix_observed") ? t("Yes, advisory only") : t("No")}</dd></div></dl>{errors.map((item) => <p className="form-error" key={item}>{item}</p>)}<div className="button-row"><ActionButton disabled={busyAction !== null} onClick={() => void refresh()}>{t("Refresh")}</ActionButton><ActionButton disabled={busyAction !== null || snapshot.startup?.available !== true} onClick={() => void runAction(t("Diagnostic collection"), "/v1/diagnostics", { action: "collect", note: t("Manual recovery collection") })}>{t("Collect diagnostics")}</ActionButton></div></Panel><RecoveryLogTail snapshot={snapshot} /></>;
}

function DiagnosticsView({ snapshot, busyAction, runAction, refresh }: ViewProps) {
  const { locale, t } = useI18n();
  const items = arrayValue(snapshot.diagnostics, "bundles");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  return <><PageIntro kicker={t("Observability / Diagnostics")} title={t("Diagnostics")} detail={t("Bundles are bounded, redacted, and limited to Nexus-owned metadata and text logs.")} /><Panel title={t("Diagnostic bundles")} icon={<TerminalWindow size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{t("{count} bundles", { count: items.length })}</span><ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction(t("Diagnostic collection"), "/v1/diagnostics", { action: "collect", note: t("Native launcher collection") })}><TerminalWindow size={16} />{t("Collect diagnostics")}</ActionButton></div><DataList items={items} emptyTitle={t("No diagnostic bundles")} emptyDetail={t("Collect a bounded bundle when a runtime issue needs review.")} render={(item) => <><div><strong>{stringValue(item, "id") || t("Bundle")}</strong><span>{t("{count} files", { count: arrayValue(item, "files").length })}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"), t("Not available"), locale)}</span></>} /></Panel><RecoveryDiagnostics snapshot={snapshot} busyAction={busyAction} runAction={runAction} refresh={refresh} /></>;
}

function HarnessDiscoveryPanel({
  value,
  loading,
  error,
  disabled,
  selectedId,
  suppressAutoOpen,
  onDetect,
  onSelect,
}: {
  value: JsonObject | null;
  loading: boolean;
  error: string | null;
  disabled: boolean;
  selectedId: string | undefined;
  suppressAutoOpen: boolean;
  onDetect: () => void;
  onSelect: (candidate: HarnessCandidate) => void;
}) {
  const { t } = useI18n();
  const candidates = harnessCandidates(value);
  const [pickerOpen, setPickerOpen] = useState(false);
  const pickerRef = useRef<HTMLDivElement>(null);
  const lastFocusRef = useRef<HTMLElement | null>(null);
  const autoOpenedValueRef = useRef<JsonObject | null>(null);

  useEffect(() => {
    if (candidates.length <= 1) {
      setPickerOpen(false);
      return;
    }
    // Auto-open only for a newly returned candidate set. Once the user closes
    // the picker or edits the manual form, do not interrupt that fallback path
    // by reopening it on every render.
    if (!suppressAutoOpen && !selectedId && autoOpenedValueRef.current !== value) {
      autoOpenedValueRef.current = value;
      setPickerOpen(true);
    }
  }, [candidates.length, selectedId, suppressAutoOpen, value]);

  useEffect(() => {
    if (pickerOpen) {
      lastFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      requestAnimationFrame(() => pickerRef.current?.focus());
    } else {
      lastFocusRef.current?.focus();
      lastFocusRef.current = null;
    }
  }, [pickerOpen]);

  const selectCandidate = (candidate: HarnessCandidate) => {
    onSelect(candidate);
    setPickerOpen(false);
  };

  const handlePickerKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "Escape") {
      setPickerOpen(false);
      return;
    }
    if (event.key !== "Tab") return;
    const focusable = Array.from(
      pickerRef.current?.querySelectorAll<HTMLElement>(
        "button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex=\"-1\"])",
      ) ?? [],
    );
    if (!focusable.length) return;
    const activeIndex = focusable.indexOf(document.activeElement as HTMLElement);
    if (event.shiftKey && (activeIndex <= 0 || activeIndex === -1)) {
      event.preventDefault();
      focusable[focusable.length - 1].focus();
    } else if (!event.shiftKey && (activeIndex === focusable.length - 1 || activeIndex === -1)) {
      event.preventDefault();
      focusable[0].focus();
    }
  };

  const candidateList = () => <div className="data-list" role="list">
    {candidates.map((candidate) => <div className="data-row" key={candidate.id} role="listitem">
      <div>
        <strong>{candidate.displayName}</strong>
        <span>{candidateModeLabel(candidate.mode, t)}</span>
        {candidate.version && <span>{candidate.version}</span>}
        <span>{t("Path")}: {candidate.program}</span>
        {candidate.mode === "node" && candidate.entry && <span>{t("Entry")}: {candidate.entry}</span>}
        {candidate.workingDir && <span>{t("Working directory")}: {candidate.workingDir}</span>}
        {candidate.source && <span>{t("Search source")}: {discoverySourceLabel(candidate.source, t)}</span>}
      </div>
      <button type="button" className={`button ${selectedId === candidate.id ? "primary" : "subtle"}`} onClick={() => selectCandidate(candidate)} disabled={disabled} aria-pressed={selectedId === candidate.id} aria-label={t("Use this Harness: {name}", { name: `${candidate.displayName}: ${candidate.program}` })}>
        {selectedId === candidate.id ? t("Selected") : t("Use this Harness")}
      </button>
    </div>)}
  </div>;

  let candidateContent: React.ReactNode = null;
  if (loading && !candidates.length) {
    candidateContent = <div className="state-card loading-state" role="status" aria-live="polite"><Pulse size={20} className="spin" /><div><strong>{t("Detecting Harness installations...")}</strong><span>{t("Run a scan to refresh the local candidate list.")}</span></div></div>;
  } else if (candidates.length === 1) {
    candidateContent = <><span className="field-label">{t("Detected candidates")}</span>{candidateList()}</>;
   } else if (candidates.length > 1) {
     candidateContent = <><span className="field-label">{t("Detected candidates")}</span><div className="candidate-choice-summary"><span>{t("Multiple Harness installations were found. Choose one before saving.")}</span><button type="button" className="button subtle" onClick={() => setPickerOpen(true)} disabled={disabled}>{t("Choose a Harness installation")}</button></div>{selectedId && <p className="field-help">{t("A candidate is selected. You can change it before saving.")}</p>}{pickerOpen && <div className="candidate-dialog" role="dialog" aria-modal="true" aria-labelledby="candidate-picker-title" tabIndex={-1} ref={pickerRef} onKeyDown={handlePickerKeyDown}><div className="candidate-dialog-card"><div className="panel-toolbar"><strong id="candidate-picker-title">{t("Choose a Harness installation")}</strong><button type="button" className="icon-button" onClick={() => setPickerOpen(false)} aria-label={t("Close candidate picker")}><X size={16} /></button></div>{candidateList()}</div></div>}</>;
  } else if (value !== null) {
    candidateContent = <EmptyState title={t("No Harness candidates found")} detail={t("No installation was found in the bounded local search paths. You can still specify a path or command manually.")} />;
  }

  return <div className="form-field full">
    <div className="panel-toolbar">
      <strong>{t("Automatic detection")}</strong>
      <button type="button" className="button" onClick={onDetect} disabled={disabled || loading}>
        {loading ? <Pulse size={16} className="spin" /> : <ArrowClockwise size={16} />}
        {loading ? t("Detecting Harness installations...") : t("Detect Harness")}
      </button>
    </div>
    <p className="field-help">{t("Automatic detection is preferred. Select a detected Harness or use manual configuration below.")}</p>
    {error && <div className="form-error" role="alert"><WarningCircle size={16} />{t("Harness detection failed: {message}", { message: compactError(error) })}</div>}
    {candidateContent}
  </div>;
}

function SettingsView({ snapshot, themeMode, setThemeMode, busyAction, runAction }: ViewProps) {
  const { locale, setLocale, t } = useI18n();
  const config = asObject(snapshot.config);
  const harness = nestedValue(config, "harness");
  const update = nestedValue(config, "update");
  const harnessEnvOverride = booleanValue(config, "harness_env_override");
  const updateEnvOverride = booleanValue(config, "update_env_override");
  const hasHarnessConfig = Object.keys(harness).length > 0;
  const harnessRuntime = harnessRuntimeValue(snapshot.harnessRuntime);
  const harnessState = stringValue(harnessRuntime, "state");
  const harnessInTransition = harnessState === "starting" || harnessState === "stopping";
  const configControlsDisabled = busyAction !== null || snapshot.startup?.available !== true || harnessInTransition || harnessState === "running";
  const [editingHarness, setEditingHarness] = useState(!hasHarnessConfig);
  const [draft, setDraft] = useState<HarnessConfigDraft>(() => harnessDraftFromConfig(config));
  const [draftDirty, setDraftDirty] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [discovery, setDiscovery] = useState<JsonObject | null>(null);
  const [discoveryLoading, setDiscoveryLoading] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [selectedCandidateId, setSelectedCandidateId] = useState<string | undefined>(undefined);

  const draftDirtyRef = useRef(false);

  useEffect(() => {
    setFormError(null);
  }, [locale]);

  useEffect(() => {
    draftDirtyRef.current = draftDirty;
  }, [draftDirty]);

  useEffect(() => {
    if (!draftDirty) {
      setDraft(harnessDraftFromConfig(asObject(snapshot.config)));
      setEditingHarness(Object.keys(nestedValue(asObject(snapshot.config), "harness")).length === 0);
    }
  }, [draftDirty, snapshot.config]);

  const updateDraft = (field: keyof HarnessConfigDraft, value: string | boolean) => {
    setDraft((current) => ({
      ...current,
      [field]: value,
      ...(field === "readinessUrl" && typeof value === "string" && !value.trim()
        ? { readinessTokenRequired: false }
        : {}),
      ...(field === "readinessUrl" ? { readinessUrlRedacted: false } : {}),
    }));
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setSelectedCandidateId(undefined);
    setFormError(null);
  };

  const updateLaunchMode = (mode: HarnessLaunchMode) => {
    setDraft((current) => {
      if (current.mode === mode) return current;
      const args = current.args
        .split(/\r?\n/)
        .map((value) => value.trim())
        .filter(Boolean);
      if (mode === "node") {
        return {
          ...current,
          mode,
          // A direct command's first argument is not necessarily a JavaScript
          // entry point. Preserve it as a Node argument and require the user
          // to choose the entry explicitly.
          entry: "",
          args: args.join("\n"),
        };
      }
      return {
        ...current,
        mode,
        entry: "",
        args: [current.entry, ...args].filter(Boolean).join("\n"),
      };
    });
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setSelectedCandidateId(undefined);
    setFormError(null);
  };

  const applyCandidate = useCallback((candidate: HarnessCandidate) => {
    setDraft((current) => ({
      ...current,
      mode: candidate.mode,
      program: candidate.program,
      entry: candidate.entry,
      args: candidate.args.join("\n"),
      workingDir: candidate.workingDir,
      readinessUrl: candidate.readinessUrl,
      timeout: candidate.readinessTimeout,
      readinessTokenRequired: candidate.readinessTokenRequired,
      readinessUrlRedacted: false,
      argsRedacted: false,
      replaceRedactedArgs: false,
    }));
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setSelectedCandidateId(candidate.id);
    setFormError(null);
    setEditingHarness(true);
  }, []);

  const detectHarness = useCallback(async () => {
    setDiscoveryLoading(true);
    setDiscoveryError(null);
    try {
      const result = await proxyRequest<JsonObject>("/v1/harness/discover");
      const candidates = harnessCandidates(result);
      setDiscovery(result);
      if (selectedCandidateId && !candidates.some((candidate) => candidate.id === selectedCandidateId)) {
        setSelectedCandidateId(undefined);
      }
      // A single unconfigured result is safe to pre-fill, but saving remains
      // an explicit user action. Multiple results stay visible for selection.
      if (!hasHarnessConfig && !draftDirtyRef.current && candidates.length === 1) {
        applyCandidate(candidates[0]);
      }
    } catch (cause) {
      setDiscovery({ candidates: [] });
      setDiscoveryError(errorMessage(cause));
    } finally {
      setDiscoveryLoading(false);
    }
  }, [applyCandidate, hasHarnessConfig, selectedCandidateId]);

  useEffect(() => {
    if (!editingHarness || snapshot.startup?.available !== true || discovery !== null || discoveryLoading) return;
    void detectHarness();
  }, [detectHarness, discovery, discoveryLoading, editingHarness, snapshot.startup?.available]);

  const openEditor = () => {
    setDraft(harnessDraftFromConfig(config));
    // Keep the editor open while the background poll refreshes runtime data.
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setSelectedCandidateId(undefined);
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
    const entry = draft.entry.trim();
    if (draft.mode === "node" && !entry) {
      setFormError(t("A Harness entry is required for Node mode."));
      return;
    }
    if (draft.mode === "node" && (numberValue(snapshot.health, "harness_config_wire_version") ?? 0) < 2) {
      setFormError(t("This Agent does not advertise the explicit Node Harness configuration contract. Update Agent before saving Node mode."));
      return;
    }
    if (draft.args.includes("[REDACTED]") || entry.includes("[REDACTED]")) {
      setFormError(t("Replace hidden arguments before saving."));
      return;
    }
    const readinessUrl = draft.readinessUrl.trim();
    if (readinessUrl && !isLoopbackReadinessTarget(readinessUrl)) {
      setFormError(t("Readiness target must be an HTTP or TCP loopback URL."));
      return;
    }
    const timeoutText = draft.timeout.trim();
    let timeout: number | undefined;
    if (timeoutText) {
      const parsed = Number(timeoutText);
      if (!Number.isInteger(parsed) || parsed <= 0 || parsed > 86400) {
        setFormError(t("Timeout must be a positive integer."));
        return;
      }
      timeout = parsed;
    }
    if (draft.argsRedacted && !draft.replaceRedactedArgs) {
      setFormError(t("Existing sensitive arguments are hidden. Enable replacement before saving."));
      return;
    }
    const harnessPayload = harnessConfigPayloadFromDraft(draft);
    harnessPayload.readiness_url = readinessUrl || null;
    harnessPayload.readiness_timeout_secs = timeout ?? null;
    const saved = await runAction(t("Save Harness configuration"), "/v1/config", {
      action: "set_harness",
      harness: harnessPayload,
      preserve_harness_readiness_url: draft.readinessUrlRedacted && Boolean(readinessUrl),
    });
    if (saved === true) {
      draftDirtyRef.current = false;
      setDraftDirty(false);
      setEditingHarness(false);
      setFormError(null);
      setSelectedCandidateId(undefined);
    }
  };

  const clearHarness = async () => {
    if (!window.confirm(t("Remove the Harness launch configuration? Harness must be stopped first."))) return;
    const cleared = await runAction(t("Clear Harness configuration"), "/v1/config", { action: "clear_harness" });
    if (cleared === true) {
      setDraft(emptyHarnessDraft);
      draftDirtyRef.current = false;
      setDraftDirty(false);
      setEditingHarness(true);
      setFormError(null);
      setSelectedCandidateId(undefined);
      setDiscovery(null);
    }
  };

  const configuredMode = harnessLaunchMode(harness);
  const configuredArgs = arrayValue(harness, "args").filter((item): item is string => typeof item === "string");
  const configuredEntry = configuredMode === "node"
    ? stringValue(harness, "entry") || configuredArgs[0] || t("Not configured")
    : undefined;

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
        {harnessEnvOverride && <p className="field-help" role="status">{t("Environment variables override part of this Harness configuration. Saved file values remain in place, but the override wins at launch time.")}</p>}
        {!editingHarness && hasHarnessConfig ? <>
          <dl className="detail-list"><div><dt>{t("Launch mode")}</dt><dd>{candidateModeLabel(configuredMode, t)}</dd></div><div><dt>{configuredMode === "node" ? t("Node executable") : t("Program")}</dt><dd>{stringValue(harness, "program") || t("Not configured")}</dd></div>{configuredMode === "node" && <div><dt>{t("Harness entry")}</dt><dd>{configuredEntry}</dd></div>}<div><dt>{t("Working directory")}</dt><dd>{stringValue(harness, "working_dir") || t("Default")}</dd></div><div><dt>{t("Readiness URL")}</dt><dd>{isLoopbackReadinessTarget(stringValue(harness, "readiness_url")) ? stringValue(harness, "readiness_url") : t("Not shown")}</dd></div></dl>
          <div className="form-actions"><button type="button" className="button" disabled={configControlsDisabled} onClick={openEditor}>{t("Edit configuration")}</button><button type="button" className="button danger" disabled={configControlsDisabled} onClick={() => void clearHarness()}>{t("Clear configuration")}</button></div>
        </> : <form className="config-form" onSubmit={(event) => void saveHarness(event)}>
          <HarnessDiscoveryPanel value={discovery} loading={discoveryLoading} error={discoveryError} disabled={configControlsDisabled} selectedId={selectedCandidateId} suppressAutoOpen={draftDirty} onDetect={() => void detectHarness()} onSelect={applyCandidate} />
          <div className="panel-toolbar"><strong>{t("Manual configuration")}</strong></div>
          <div className="form-grid">
            <label className="form-field full"><span className="field-label">{t("Launch mode")}</span><select className="theme-select" value={draft.mode} onChange={(event) => updateLaunchMode(event.target.value as HarnessLaunchMode)} disabled={configControlsDisabled}><option value="direct">{t("Direct executable")}</option><option value="node">{t("Node runtime")}</option></select><span className="field-help">{t("Select how the external Harness is started. Direct runs the executable or command; Node runs the selected entry through the Node runtime.")}</span></label>
            <label className="form-field full"><span className="field-label">{draft.mode === "node" ? t("Node executable") : t("Program")}</span><input className="form-input" value={draft.program} onChange={(event) => updateDraft("program", event.target.value)} placeholder={draft.mode === "node" ? t("Node executable path or command") : t("Program path or command")} disabled={configControlsDisabled} required /></label>
            {draft.mode === "node" && <label className="form-field full"><span className="field-label">{t("Harness entry")}</span><input className="form-input" value={draft.entry} onChange={(event) => updateDraft("entry", event.target.value)} placeholder={t("Harness entry script or package")} disabled={configControlsDisabled} required /></label>}
            <label className="form-field"><span className="field-label">{draft.mode === "node" ? t("Node project directory") : t("Working directory")} <em>{t("Optional")}</em></span><input className="form-input" value={draft.workingDir} onChange={(event) => updateDraft("workingDir", event.target.value)} placeholder={t("Agent default")} disabled={configControlsDisabled} /></label>
            <label className="form-field"><span className="field-label">{t("Readiness timeout (seconds)")} <em>{t("Optional")}</em></span><input className="form-input" inputMode="numeric" value={draft.timeout} onChange={(event) => updateDraft("timeout", event.target.value)} placeholder={t("Agent default")} disabled={configControlsDisabled} /></label>
            <label className="form-field full"><span className="field-label">{t("Readiness URL")} <em>{t("Optional")}</em></span><input className="form-input" type="text" value={draft.readinessUrl} onChange={(event) => updateDraft("readinessUrl", event.target.value)} placeholder={t("Readiness URL example")} disabled={configControlsDisabled} /><span className="field-help">{t("Use an HTTP loopback URL for a 2xx check, or tcp://127.0.0.1:PORT when the Harness protects its page with authentication.")}</span></label>
            <label className="form-check full"><input type="checkbox" checked={draft.readinessTokenRequired} onChange={(event) => updateDraft("readinessTokenRequired", event.target.checked)} disabled={configControlsDisabled || !draft.readinessUrl.trim()} /><span>{t("Require a fresh Harness token before accepting readiness")}</span></label>
            <p className="field-help full">{t("Enable this for token-protected Harness services. A listener alone is not enough; the Agent must observe a fresh URL in its current Harness log session.")}</p>
            <label className="form-field full"><span className="field-label">{draft.mode === "node" ? t("Node arguments") : t("Arguments")}</span><textarea className="form-textarea" value={draft.args} onChange={(event) => updateDraft("args", event.target.value)} placeholder={draft.mode === "node" ? t("Arguments passed to the Node Harness entry, one per line. Use {profile}, {release}, or {release_root} when needed.") : t("One argument per line. Use {profile}, {release}, or {release_root} when needed.")} disabled={configControlsDisabled || (draft.argsRedacted && !draft.replaceRedactedArgs)} /></label>
          </div>
          <p className="field-help">{draft.mode === "node" ? t("Arguments passed to the Node Harness entry, one per line. Use {profile}, {release}, or {release_root} when needed.") : t("One argument per line. Use {profile}, {release}, or {release_root} when needed.")}</p>
          {draft.argsRedacted && <label className="form-check"><input type="checkbox" checked={draft.replaceRedactedArgs} onChange={(event) => updateDraft("replaceRedactedArgs", event.target.checked)} disabled={configControlsDisabled} /><span>{t("Replace hidden arguments")}</span></label>}
          {formError && <div className="form-error" role="alert"><WarningCircle size={16} />{formError}</div>}
          {harnessState === "running" && <p className="field-help" role="status">{t("Stop Harness before changing its launch configuration.")}</p>}
          <div className="form-actions"><button type="submit" className="button primary" disabled={configControlsDisabled}>{t("Save configuration")}</button>{hasHarnessConfig && <button type="button" className="button" disabled={configControlsDisabled} onClick={() => { draftDirtyRef.current = false; setDraftDirty(false); setSelectedCandidateId(undefined); setFormError(null); setEditingHarness(false); }}>{t("Cancel")}</button>}{hasHarnessConfig && <button type="button" className="button danger" disabled={configControlsDisabled} onClick={() => void clearHarness()}>{t("Clear configuration")}</button>}</div>
        </form>}
        {!hasHarnessConfig && !editingHarness && <EmptyState title={t("Harness is not configured")} detail={t("The Agent remains usable as a control plane until an external Harness is configured.")} />}
      </Panel>
      <Panel title={t("Update configuration")} icon={<CloudArrowUp size={18} />}>
        {updateEnvOverride && <p className="field-help" role="status">{t("Environment variables override part of this update configuration.")}</p>}
        {Object.keys(update).length ? <dl className="detail-list"><div><dt>{t("Source")}</dt><dd>{stringValue(update, "source") || t("Not shown")}</dd></div><div><dt>{t("Ref")}</dt><dd>{stringValue(update, "ref_name") || t("Default")}</dd></div><div><dt>{t("Git program")}</dt><dd>{stringValue(update, "git_program") || t("Default")}</dd></div></dl> : <EmptyState title={t("Updates are not configured")} detail={t("Release metadata and current runtime remain available without an update source.")} />}
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
