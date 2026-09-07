import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { displayZoom, setDisplayZoom, ZOOM_CHANGED, ZOOM_LEVELS } from "./display-preferences";
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
  Info,
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
  FIXED_PROFILE_PLUGINS,
  isMissingHarnessError,
  needsHarnessInstall,
  pluginMoveTarget,
  failClosedSnapshot,
  coldOperationIsTerminal,
  createLatestRequest,
  createFailureNoticeTracker,
  actionNoticeKey,
  harnessControlGate,
  invalidatesHarnessCredentials,
  launcherContentMode,
  recoveryMutationGate,
  isLifecycleBusyError,
  lifecycleBusySnapshot,
  runtimeSettingsGate,
  validStartupCheck,
} from "./control-state";
import { useI18n, type Locale, type Translator } from "./i18n";
import { preferencesDraft, preferencesPayload, type HarnessPreferencesDraft } from "./harness-preferences";
import { notify, notificationsEnabledPreference, setNotificationsEnabledPreference } from "./notifications";
import { refreshEditableDraft, finishDraftSave, replacementArgumentRows, harnessFailureKeys, diagnosticExportResult, homePreferencesPayload, launchInputMatches, updateSourcePayload, offlineArchivePathValid, offlinePackageCommand } from "./settings-state";

import { operationSummaries, operationResponseNotice, operationNoticeKind, releaseCatalogIsCurrent, operationRetryCommand } from "./operation-status";

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
  harness_startup_error?: string;
};

type Snapshot = {
  startup: StartupStatus | null;
  endpointErrors: Record<string, string>;
  lifecycleBusy?: boolean;
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
  maintenance?: JsonObject | null;
  recovery: JsonObject | null;
  config: JsonObject | null;
};

type ModuleId = "workbench" | "guide" | "profiles" | "maintenance" | "settings";

type ThemeMode = "system" | "light" | "dark";
type HarnessLaunchMode = "direct" | "node";

type ModuleDefinition = {
  id: ModuleId;
  label: string;
  icon: IconComponent;
};

const modules: ModuleDefinition[] = [
  { id: "workbench", label: "Workbench", icon: House },
  { id: "guide", label: "Setup guide", icon: RocketLaunch },
  { id: "profiles", label: "Profiles and plugins", icon: SlidersHorizontal },
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

type SnapshotEndpoint = Exclude<keyof Snapshot, "startup" | "status" | "endpointErrors" | "lifecycleBusy">;

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
  // Characteristic failure signatures map to plain-language causes with the
  // recovery action that actually applies; anything else falls through to
  // the backend's own message.
  if (normalized.includes("plugin tree failed to load") || normalized.includes("loader entries failed to apply")) {
    return t("Plugins failed to load, likely a version mismatch between installed plugins and this Harness build. Open Recovery to remove the affected plugins or restore a healthy snapshot.");
  }
  if (normalized.includes("does not provide an export named")) {
    return t("A plugin expects module APIs this Harness build does not have: the installed plugin set and the Harness version are out of sync. Restore a healthy snapshot or update the plugins.");
  }
  if (normalized.includes("duplicate loader entry")) {
    return t("A profile plugin duplicates a plugin this Harness now ships built-in. Remove the older copy from the profile's plugin inventory.");
  }
  if (normalized.includes("illegal operation on a directory") || normalized.includes("os error 267") || normalized.includes("目录名称无效")) {
    return t("A path is invalid, likely a leftover from a previous version switch. Restore a healthy snapshot to rebuild the profile.");
  }
  if (normalized.includes("release slots are full")) {
    return t("All release slots are full. Remove a slot you no longer need, then try again.");
  }
  if (normalized.includes("release_slot_protected") || normalized.includes("is the current slot") || normalized.includes("is the last-known-good slot")) {
    return t("That slot is still in use (current or last-known-good). Switch to another version first.");
  }
  if (normalized.includes("snapshot_release_not_installed")) {
    return t("This snapshot's Harness version is not installed. Cold-switch to that tag first, then restore.");
  }
  if (normalized.includes("switch the dependency registry to npmmirror")) {
    return t("Upstream dependency installation failed, usually a network issue. Switch the dependency registry to npmmirror in Settings, then retry.");
  }
  if (normalized.includes("eisdir")) {
    return t("A directory was used where a file was expected, usually a leftover from a crashed run. Restore a healthy snapshot to rebuild the profile.");
  }
  const fallbacks: Array<[RegExp, string]> = [
    [/refused to connect|connection refused|econnrefused/, t("The local service is not responding. Retry, and check the Agent status on the Overview page.")],
    [/timed out|timeout/, t("The operation timed out, often a network issue. Check your connection or proxy settings, then retry.")],
    [/os error 5|access is denied/, t("Access denied: the file may be locked by another process. Close programs using it and retry.")],
  ];
  for (const [pattern, text] of fallbacks) {
    if (pattern.test(normalized)) {
      return text;
    }
  }
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

const isBrowserPreview =
  typeof window === "undefined" || !("__TAURI_INTERNALS__" in window);

/// Browser-only preview: synthesize the startup status from the proxied
/// Agent health endpoint, since the native auto-start command is unavailable.
async function commandStartupStatus(): Promise<StartupStatus> {
  if (!isBrowserPreview) {
    return invoke<StartupStatus>("startup_status");
  }
  const health = await fetch("/agent/v1/health").then(
    (response) => response.json() as Promise<JsonObject>,
  );
  const alive = health?.status === "ok";
  return {
    available: alive,
    running: alive,
    api_base: "/agent",
    data_root: health?.data_root,
    data_root_id: health?.data_root_id,
    instance_id: health?.instance_id,
  } as StartupStatus;
}

async function proxyRequest<T = JsonObject>(
  path: string,
  method = "GET",
  body?: JsonObject,
): Promise<T> {
  // Browser-only development preview: `pnpm dev` serves the same UI without
  // the Tauri bridge, so requests go through the /agent dev proxy instead.
  if (isBrowserPreview) {
    const response = await fetch(`/agent${path}`, {
      method,
      headers: body === undefined ? undefined : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return (await response.json()) as T;
  }
  return invoke<T>("proxy_request", {
    method,
    path,
    body: body ?? null,
  });
}

const runtimeToolNames = ["git", "node", "pnpm"] as const;
type RuntimeToolName = typeof runtimeToolNames[number];
type RuntimeToolSource = "system" | "nexus" | "bundled";

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
    const source =
      sourceValue === "system" || sourceValue === "nexus" || sourceValue === "bundled"
        ? sourceValue
        : undefined;
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
  return source === "system" ? t("System source") : source === "bundled" ? t("Bundled") : t("Nexus source");
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

export function MissingReleaseNotice({ releases, onReinstall }: { releases: JsonObject; onReinstall: () => void }) {
  const { t } = useI18n();
  const missing = releases.unavailable_selections;
  if (!Array.isArray(missing) || missing.length === 0) return null;
  return <div className="notice action-error" role="alert"><WarningCircle size={17} />
    <span><strong>{t("Harness installation is incomplete")}</strong> {t("Agent is available. Reinstall Harness from the setup guide; existing data and remaining files are preserved.")}
      <small>{missing.filter((id): id is string => typeof id === "string").join(", ")}</small></span>
    <button className="button subtle" onClick={onReinstall}>{t("Reinstall Harness")}</button>
  </div>;
}

function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: React.ReactNode }) {
  return <div className="modal-overlay" role="dialog" aria-modal="true" onClick={onClose}>
    <div className="modal-card" onClick={(event) => event.stopPropagation()}>
      <div className="modal-header"><strong>{title}</strong><ActionButton onClick={onClose}><X size={16} /></ActionButton></div>
      <div className="modal-body">{children}</div>
    </div>
  </div>;
}

function Metric({ label, value, detail, actions, children }: { label: string; value: string; detail?: string; actions?: React.ReactNode; children?: React.ReactNode }) {
  return <div className="metric"><span>{label}</span><strong>{value}</strong>{detail && <small>{detail}</small>}{actions && <div className="metric-actions">{actions}</div>}{children}</div>;
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
  return <button type="button" className={`button ${tone}`} onClick={onClick} disabled={disabled} title={title}>{children}</button>;
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
  const [activeModule, setActiveModule] = useState<ModuleId>("workbench");
  const [operationAnchor, setOperationAnchor] = useState<string | null>(null);
  useEffect(() => { if (operationAnchor) { document.getElementById(operationAnchor)?.scrollIntoView({ block: "start" }); setOperationAnchor(null); } }, [activeModule, operationAnchor]);
  const [themeMode, setThemeMode] = useState<ThemeMode>(storedTheme);
  const [systemThemeMode, setSystemThemeMode] = useState<"light" | "dark">(systemTheme);
  const [snapshot, setSnapshot] = useState<Snapshot>(emptySnapshot);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [bridgeError, setBridgeError] = useState<string | null>(null);
  const [agentUnavailable, setAgentUnavailable] = useState<string | null>(null);
  const [checkOpen, setCheckOpen] = useState(false);
  const [checkPending, setCheckPending] = useState(false);
  const [basicCheckResult, setBasicCheckResult] = useState<JsonObject | null>(null);
  const [basicCheckError, setBasicCheckError] = useState("");
  const startupCheckInFlight = useRef(false);
  const checkEvents = useRef(createFailureNoticeTracker());
  const [notice, setNoticeMessage] = useState<string | null>(null);
  const [noticeKind, setNoticeKind] = useState<"success" | "warning" | "info">("info");
  const setNotice = useCallback((message: string | null, kind: "success" | "warning" | "info" = "info") => { setNoticeMessage(message); setNoticeKind(kind); }, []);
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
          try {
            const startup = await commandStartupStatus();
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
            let lifecycleBusy = false;
            // These reads share the Agent lifecycle gate. Serialize them within a
            // refresh so our own reads do not look like an active mutation.
            const lifecycleReads = new Set(["state", "harnessRuntime", "harnessUi", "profiles", "releases", "recovery"]);
            let readQueue: Promise<unknown> = Promise.resolve();
            const entries = await Promise.all(Object.entries(endpointMap).map(([key, path]) => {
              const read = async () => {
              try {
                const value = await proxyRequest<JsonObject>(path);
                return [key as SnapshotEndpoint, value] as const;
              } catch (cause) {
                const message = errorMessage(cause);
                if (isLifecycleBusyError(message)) lifecycleBusy = true;
                else endpointErrors[path] = message;
                return [key as SnapshotEndpoint, null] as const;
              }
              };
              if (!lifecycleReads.has(key)) return read();
              const pending = readQueue.then(read);
              readQueue = pending;
              return pending;
            }));
            for (const [key, value] of entries) next[key] = value;
            next.endpointErrors = endpointErrors;
            const coldPhase = stringValue(asObject(asObject(next.updates).operation), "phase");
            harnessPollState.current = lifecycleBusy || (!!coldPhase && !coldOperationIsTerminal(coldPhase)) ? "busy" : stringValue(harnessRuntimeValue(next.harnessRuntime), "state");
            setSnapshot(previous => {
              if (!lifecycleBusy) return next;
              const instance = stringValue(next.health, "instance_id");
              const root = stringValue(next.health, "data_root_id");
              const sameOwner = !!instance && !!root && instance === stringValue(previous.health, "instance_id") && root === stringValue(previous.health, "data_root_id");
              return lifecycleBusySnapshot(next, previous, sameOwner);
            });
            if (
              !lifecycleBusy && credentialInvalidation.current !== null &&
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
      const interval = harnessPollState.current === "busy" ? 1000 : (harnessPollState.current === "starting" || harnessPollState.current === "failed") ? 400 : 8000;
      timer = window.setTimeout(() => void poll(), interval);
    };
    void poll();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [refresh]);

  useEffect(() => {
    const wake = () => { if (document.visibilityState !== "hidden") void refresh(); };
    window.addEventListener("focus", wake);
    document.addEventListener("visibilitychange", wake);
    return () => { window.removeEventListener("focus", wake); document.removeEventListener("visibilitychange", wake); };
  }, [refresh]);

  useEffect(() => {
    void invoke("set_native_notifications", { enabled: notificationsEnabledPreference() }).catch(() => undefined);
    const unlisten = listen<string>("nexus-native-error", event => setError(event.payload)).catch(() => () => undefined);
    setDisplayZoom(displayZoom());
    const zoomKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey) || event.altKey || !["+", "=", "-", "0"].includes(event.key)) return;
      event.preventDefault();
      const current = displayZoom();
      const index = ZOOM_LEVELS.indexOf(current);
      setDisplayZoom(event.key === "0" ? 100 : ZOOM_LEVELS[Math.max(0, Math.min(ZOOM_LEVELS.length - 1, index + (event.key === "-" ? -1 : 1)))]);
    };
    window.addEventListener("keydown", zoomKey);
    return () => { window.removeEventListener("keydown", zoomKey); void unlisten.then(stop => stop()); };
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

  const runAction = useCallback(async (label: string, path: string, body: JsonObject): Promise<boolean> => {
    if (startupCheckInFlight.current) return false;
    if (snapshot.lifecycleBusy && !(path === "/v1/updates" && body.action === "cancel")) {
      setNotice(t("Version or startup operation in progress. Please wait; update progress remains available."));
      return false;
    }
    const isNativeAgentLifecycle = path === "/v1/agent";
    if (snapshot.startup === null || (snapshot.startup.available !== true && !isNativeAgentLifecycle)) {
      setError(t("Launcher controls are disabled until the Agent identity is verified."));
      return false;
    }
    if ((path === "/v1/profiles" && body.action === "select") && !booleanValue(snapshot.recovery, "paused") && needsHarnessInstall(snapshot.config, snapshot.releases)) {
      setActiveModule("workbench"); setCheckOpen(false); setError(null);
      setNotice(t("No local Harness is installed. Select a version here to install it."));
      return false;
    }
    if (path === "/v1/harness" && ["start", "restart"].includes(String(body.action))) {
      // Preserve the current iframe/token until a real lifecycle POST is ready.
      // A failed observation must never invalidate an otherwise healthy session.
      startupCheckInFlight.current = true;
      setBusyAction(t("Basic startup checks")); setBasicCheckError(""); setBasicCheckResult(null);
      setNotice(null); setError(null);
      try {
        const report = await proxyRequest<JsonObject>("/v1/preflight");
        if (!validStartupCheck(report)) throw new Error(t("Invalid startup check response. Retry the check or export diagnostics."));
        setBasicCheckResult(report);
        if (report.ready !== true || report.paused === true) { setCheckOpen(true); return false; }
      } catch (cause) {
        setBasicCheckError(errorMessage(cause)); setCheckOpen(true); return false;
      } finally {
        startupCheckInFlight.current = false;
        setBusyAction(null);
      }
    }
    const compatibilityAction = (path === "/v1/profiles" && body.action === "select") ||
      (path === "/v1/releases" && ["promote", "rollback"].includes(String(body.action))) ||
      (path === "/v1/updates" && ["switch", "confirm", "offline_import"].includes(String(body.action))) ||
      (path === "/v1/harness" && ["start", "restart"].includes(String(body.action)));
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
    let actionError: string | null = null;
    let rawActionError: string | null = null;
    let actionSucceeded = false;
    try {
      const response = await proxyRequest<JsonObject>(path, "POST", body);
      const exported = path === "/v1/diagnostics" && body.action === "export" ? diagnosticExportResult(response) : null;
      actionSucceeded = true;
      if (path === "/v1/updates" && (response.operation || response.install_operation || body.action === "clear_finished")) {
        setSnapshot(current => ({ ...current, updates: response }));
      }
      const noticeKey = operationResponseNotice(path, response) ?? actionNoticeKey(path, body.action);
      setNotice(exported ? t(exported.manual ? "Diagnostic file exported; open its folder manually: {path}" : "Diagnostic file exported: {path}", { path: exported.path }) : noticeKey === "complete" ? `${label} ${t(noticeKey)}` : t(noticeKey), operationNoticeKind(noticeKey, exported));
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
        if (rawActionError && isMissingHarnessError(rawActionError)) {
          setActiveModule("workbench"); setCheckOpen(false); setError(null);
          setNotice(t("No local Harness is installed. Select a version here to install it."));
        } else setError(actionError);
      }
      setBusyAction(null);
      if (compatibilityAction) setCheckPending(false);
    }
    return actionSucceeded;
  }, [refresh, snapshot, t]);

  useEffect(() => {
    if (!snapshot.startup?.available || snapshot.lifecycleBusy) return;
    const report = asObject(asObject(snapshot.profiles).compatibility);
    const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
    const startupError = snapshot.startup.harness_startup_error || "";
    const runtimeError = stringValue(runtime, "error") || "";
    const failed = currentStartupFailure(snapshot);
    const keys = [
      startupError ? `bootstrap:${startupError}` : "",
      failed ? `failure:${stringValue(snapshot.harnessRuntime, "log_session_run_id")}:${numberValue(runtime, "updated_at_unix")}:${runtimeError}` : "",
      ["failed", "needs_choice"].includes(stringValue(report, "status") || "")
        ? `check:${stringValue(report, "source_profile")}:${stringValue(report, "release_id")}:${numberValue(report, "checked_at_unix")}` : "",
    ].filter(Boolean);
    // Wait for a complete initial snapshot; cached failures are history.
    if (!snapshot.profiles || !snapshot.harnessRuntime) return;
    if (!checkEvents.current.observe(keys)) return;
    if (isMissingHarnessError(startupError) || isMissingHarnessError(runtimeError)) {
      setActiveModule("workbench"); setCheckOpen(false);
      setNotice(t("No local Harness is installed. Select a version here to install it."));
    } else setCheckOpen(true);
  }, [snapshot, t]);

  const launcherStatus = asObject(snapshot.status);
  const isRunning = launcherStatus.running === true;
  const agentState = nestedValue(snapshot.state, "state");
  const connectionLabel = snapshot.status ? (isRunning ? t("Agent online") : t("Agent stopped")) : t("Bridge offline");
  const connectionTone = snapshot.status ? (isRunning ? "good" : "warn") : "bad";
  const contentMode = launcherContentMode(bridgeError, loading, snapshot.status !== null);

  const failureNotices = useRef(createFailureNoticeTracker());
  useEffect(() => {
    if (!snapshot.harnessRuntime) return;
    const fresh = failureNotices.current.observe(harnessFailureKeys(snapshot.harnessRuntime));
    if (fresh && notificationsEnabledPreference()) {
      void notify("Nexus Launcher", t("Harness failed to start or crashed. Check the Overview page for details."));
    }
  }, [snapshot.harnessRuntime, t]);

  const content = useMemo(() => {
    const common = { snapshot, busyAction: busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null), credentialInvalidationPending, runAction, refresh, themeMode, setThemeMode, openSettings: () => setActiveModule("settings") };
    switch (activeModule) {
      case "guide": return <GuideView {...common} />;
      case "profiles": return <ProfilesView {...common} />;
      case "maintenance": return <MaintenanceView {...common} />;
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
            <ActionButton onClick={() => setCheckOpen(true)}>{t("Startup compatibility check")}</ActionButton>
            <StatusPill label={connectionLabel} tone={connectionTone} />
            <button className="icon-button" onClick={() => void refresh()} aria-label={t("Refresh launcher status")} title={t("Refresh launcher status")}><ArrowsClockwise size={19} /></button>
          </div>
        </header>

        {notice && <div className="notice" role="status">{noticeKind === "success" ? <CheckCircle size={17} /> : noticeKind === "warning" ? <WarningCircle size={17} /> : <Info size={17} />}{notice}<button onClick={() => setNotice(null)} aria-label={t("Dismiss notice")}><X size={15} /></button></div>}
        {error && contentMode !== "error" && <div className="notice action-error" role="alert"><WarningCircle size={17} /><span>{error}</span><button onClick={() => setError(null)} aria-label={t("Dismiss error")}><X size={15} /></button></div>}
        {agentUnavailable && contentMode !== "error" && <AgentUnavailableNotice message={agentUnavailable} onRetry={() => void retryStartup()} />}
        {snapshot.startup?.available && <MissingReleaseNotice releases={asObject(snapshot.releases)} onReinstall={() => setActiveModule("guide")} />}
        {snapshot.startup?.available && snapshot.recovery && <RecoveryModePanel snapshot={snapshot} busyAction={busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null)} runAction={runAction} />}
        {snapshot.lifecycleBusy && <div className="notice" role="status"><ArrowsClockwise size={17}/><span>{t("Version or startup operation in progress. Showing the last confirmed catalogs; Harness access is temporarily unavailable. Update progress continues to refresh.")}</span></div>}
        <OperationStatusPanel snapshot={snapshot} onOpen={(module, anchor) => { setActiveModule(module); setOperationAnchor(anchor); }} />
        {!error && Object.keys(snapshot.endpointErrors).length > 0 && <DegradedNotice errors={snapshot.endpointErrors} />}
        {contentMode === "error"
          ? <ErrorState message={bridgeError ?? t("The native bridge is unavailable.")} onRetry={() => void retryStartup()} />
          : contentMode === "loading"
            ? <LoadingState />
            : <section className="page-content">{content}</section>}

        {checkOpen && <CompatibilityDialog snapshot={snapshot} busyAction={busyAction ?? (snapshot.lifecycleBusy ? t("Operation in progress") : null)} runAction={runAction} pending={checkPending || !!snapshot.lifecycleBusy} basicResult={basicCheckResult} basicError={basicCheckError} onClose={() => setCheckOpen(false)} />}
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
  embedded?: boolean;
};

type HarnessPanelProps = Pick<ViewProps, "snapshot" | "busyAction" | "runAction">;
type HarnessAuthPanelProps = HarnessPanelProps & Pick<ViewProps, "credentialInvalidationPending">;
type HarnessWebPanelProps = Pick<ViewProps, "snapshot" | "credentialInvalidationPending" | "busyAction" | "runAction">;

/// Guided setup: the one-stop flow as a wizard. Steps check themselves from
/// live state and advance automatically; starting Harness stays an explicit
/// button press (no implicit start, per contract).
export function GuideView(props: ViewProps) {
  const { t } = useI18n();
  const { snapshot, busyAction, runAction } = props;
  const savedPreferences = nestedValue(snapshot.config, "harness_preferences");
  const savedHome = stringValue(savedPreferences, "home") || "";
  const [homeDraft, setHomeDraft] = useState(() => ({ value: savedHome, dirty: false }));
  const [savingHome, setSavingHome] = useState(false);
  useEffect(() => { setHomeDraft(current => refreshEditableDraft(current, savedHome)); }, [savedHome, homeDraft.dirty]);
  const checkRevision = JSON.stringify([asObject(snapshot.releases).current_release, asObject(snapshot.profiles).active_profile,
    asObject(snapshot.config).harness, savedPreferences, asObject(snapshot.config).runtime,
    asObject(snapshot.recovery).paused, asObject(snapshot.recovery).pending_restore, asObject(snapshot.updates).operation]);
  const [checkedRevision, setCheckedRevision] = useState("");
  const saveHome = async () => {
    if (savingHome) return;
    setSavingHome(true);
    try {
      const saved = await runAction(t("Save Harness preferences"), "/v1/config", {
        action: "set_harness_preferences", harness_preferences: homePreferencesPayload(savedPreferences, homeDraft.value),
      });
      setHomeDraft(current => finishDraftSave(current, saved === true));
    } finally { setSavingHome(false); }
  };
  const [environment, setEnvironment] = useState<RuntimeStatusPayload | null>(null);
  const [checking, setChecking] = useState(false);
  const [environmentError, setEnvironmentError] = useState<string | null>(null);
  const request = useRef(createLatestRequest());
  const runtime = asObject(asObject(snapshot.config).runtime);
  const [source, setSource] = useState(stringValue(runtime, "source") || "official");
  useEffect(() => { setSource(stringValue(runtime, "source") || "official"); }, [runtime.source]);
  const check = useCallback(async () => {
    if (!snapshot.startup?.available) return;
    const token = request.current.begin(); setChecking(true); setEnvironmentError(null);
    try { const value = runtimeStatusFromResponse(await proxyRequest("/v1/runtime")); if (request.current.isCurrent(token)) setEnvironment(value); }
    catch (cause) { if (request.current.isCurrent(token)) { setEnvironment(null); setEnvironmentError(errorMessage(cause)); } }
    finally { if (request.current.isCurrent(token)) setChecking(false); }
  }, [snapshot.startup?.available]);

  const current = stringValue(asObject(snapshot.releases), "current_release");
  const harness = harnessRuntimeValue(snapshot.harnessRuntime);
  const phase = stringValue(harness, "state");
  const operation = asObject(asObject(snapshot.updates).operation);
  const operationPhase = stringValue(operation, "phase");
  const installing = !!operationPhase && !coldOperationIsTerminal(operationPhase);
  const startupAvailable = snapshot.startup?.available === true;
  const tools = environment?.tools || [];
  const ready = ["node", "pnpm"].every(name => tools.some(tool => tool.name === name && tool.available));
  const envReady = !installing && ready;
  useEffect(() => {
    // Re-run automatically whenever the Agent becomes available, a version
    // install finishes, or the tool set can have changed.
    if (startupAvailable) { void check(); }
    return () => request.current.cancel();
  }, [check, current, startupAvailable, operationPhase === "succeeded", envReady]);
  const locked = !startupAvailable || runtimeSettingsGate(phase, numberValue(harness, "pid"), stringValue(asObject(asObject(snapshot.updates).update), "state"), operationPhase, booleanValue(operation, "cleanup_pending"), busyAction !== null).disabled;
  const versionStepDone = !!current && !installing;
  return <><PageIntro kicker={t("Setup guide")} title={t(current ? "Harness is ready" : "Set up Harness automatically")} detail={t("Environment checks run automatically; install a version, then press Start once. Starting Harness always needs your explicit click.")} />
    <div className="setup-journey" aria-label={t("Setup progress")}>
      <div className={!homeDraft.dirty ? "step-done" : ""}><span>1 · {t("Harness data directory")}</span><strong>{t(homeDraft.dirty ? "Unsaved changes" : savedHome ? "Configured" : "Inherit upstream default")}</strong></div>
      <div className={versionStepDone ? "step-done" : ""}><span>2 · {t("Harness version")}</span><strong>{current ? t("Version installed") : t("Choose a version")}</strong></div>
      <div className={checkedRevision === checkRevision ? "step-done" : ""}><span>3 · {t("Basic startup checks")}</span><strong>{t(checkedRevision === checkRevision ? "No blocking issues found" : "Run basic checks")}</strong></div>
      <div className={phase === "running" ? "step-done" : ""}><span>4 · {t("Start Harness")}</span><strong>{localizedRuntimeState(phase, t)}</strong></div>
    </div>
    <Panel title={t("Harness data directory")} icon={<House size={18}/>}>
      <label className="form-field"><span>{t("Harness data directory")}</span><input className="form-input" value={homeDraft.value} disabled={locked || savingHome} placeholder={t("Inherit upstream default")} onChange={event => setHomeDraft({ value: event.target.value, dirty: true })}/></label>
      <p className="field-help">{t("Changing this path only changes where Harness looks for data. Existing files are not moved or deleted.")}</p>
      <ActionButton disabled={locked || savingHome || !homeDraft.dirty} onClick={() => void saveHome()}>{t("Save Harness preferences")}</ActionButton>
      {homeDraft.dirty && <ActionButton disabled={savingHome} onClick={() => setHomeDraft({ value: savedHome, dirty: false })}>{t("Cancel")}</ActionButton>}
    </Panel>
    <details className="setup-sections" open={!current || installing || operationPhase === "failed"}><summary>{t("Environment and versions")}</summary>
    <Panel title={t("Runtime environment")} icon={<Cpu size={18}/>}>
      <div className="button-row">{(["node", "pnpm", "git"] as const).map(name => { const tool = tools.find(item => item.name === name); return <span key={name}>{runtimeToolLabel(name, t)} · {name === "git" ? t(tool?.available ? "System Git with embedded fallback" : "Embedded Git available") : tool?.available ? `${tool.version}${tool.source ? ` · ${runtimeToolSourceLabel(tool.source, t)}` : ""}` : t(environment ? "Not available" : "Not checked")}</span>; })}<ActionButton disabled={checking || !snapshot.startup?.available} onClick={() => void check()}>{t("Check environment")}</ActionButton></div>
      <p className="field-help">{t("Node, npm and pnpm must form a complete combination. Explicit paths take priority, otherwise Nexus uses its bundled runtime and checks each Harness version against it.")}</p>
      {environmentError && <p className="form-error" role="alert">{environmentError}</p>}
      <details><summary>{t("Dependency registry")}</summary><div className="form-grid">
        <label className="form-field">{t("Dependency registry")}<select value={source} disabled={locked} onChange={event => setSource(event.target.value)}><option value="official">{t("Official")}</option><option value="npmmirror">npmmirror</option></select></label>
        <p className="field-help">{t("Only used when Harness dependencies are downloaded. Nexus never downloads Node, pnpm, or Git.")}</p>
        <ActionButton disabled={locked} onClick={() => void runAction(t("Save runtime settings"), "/v1/config", { action: "set_runtime", runtime: { ...runtime, source } })}>{t("Save")}</ActionButton>
      </div></details>
    </Panel>
    <UpdatesView {...props} busyAction={homeDraft.dirty || savingHome ? t("Unsaved changes") : busyAction} embedded />
    </details>
    <BasicStartupCheck key={checkRevision} disabled={!startupAvailable || busyAction !== null || homeDraft.dirty || savingHome} onResult={result => setCheckedRevision(result?.ready === true ? checkRevision : "")} />
    <HarnessControlPanel snapshot={snapshot} busyAction={homeDraft.dirty || savingHome ? t("Unsaved changes") : busyAction} runAction={runAction} openSettings={props.openSettings} />
    {phase === "running" && <><HarnessWebPanel {...props}/><details className="advanced-settings"><summary>{t("Connection details")}</summary><HarnessAuthPanel {...props}/></details></>}
    {phase === "failed" && <RecoveryLogTail snapshot={snapshot}/>}
  </>;
}

export function MaintenanceView(props: ViewProps) {
  const { t } = useI18n();
  return <><PageIntro kicker={t("Maintenance")} title={t("Maintenance")} detail={t("Inspect errors, collect diagnostics, and recover from startup failures.")} />
    <Panel title={t("Agent lifecycle")} icon={<Cpu size={18}/>}><ActionButton tone="primary" disabled={props.busyAction !== null} onClick={() => void props.runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })}>{t("Force restart Agent")}</ActionButton></Panel>
    <DiagnosticsView {...props} embedded />
    <SpaceMaintenancePanel {...props} />
  </>;
}

function SpaceMaintenancePanel({ busyAction, snapshot }: ViewProps) {
  const { t } = useI18n();
  const [status, setStatus] = useState<JsonObject>({});
  const [days, setDays] = useState("30");
  const [selected, setSelected] = useState<string[]>([]);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const preview = asObject(status.preview), result = asObject(status.result);
  const load = useCallback(async () => {
    try {
      const value = await proxyRequest("/v1/maintenance");
      if (value.error) throw new Error(stringValue(asObject(value.error), "message") || errorMessage(value.error));
      setStatus(value);
    } catch (e) { setError(errorMessage(e)); }
  }, []);
  useEffect(() => { void load(); }, [load]);
  const execute = async (cleanup: boolean) => {
    setPending(true); setError("");
    try {
      const value = await proxyRequest("/v1/maintenance", "POST", cleanup
        ? { action: "cleanup", preview_id: preview.preview_id, item_ids: selected }
        : { action: "preview", retention_days: Number(days) });
      if (value.error) throw new Error(stringValue(asObject(value.error), "message") || errorMessage(value.error));
      setStatus(value); setSelected([]);
    } catch (e) { setError(errorMessage(e)); await load(); }
    finally { setPending(false); }
  };
  const bytes = (value: unknown) => typeof value === "number" ? `${(value / 1024 / 1024).toFixed(1)} MiB` : t("Unknown");
  const disabled = pending || busyAction !== null || snapshot.startup?.available !== true;
  return <section id="maintenance-cleanup"><Panel title={t("Data and disk space")} icon={<Package size={18} />}>
    <p>{t("Preview disk use and select old files to remove. Harness data, project files, recovery backups, and active versions are protected. No data is moved.")}</p>
    <p className="field-help">{t("Sizes are logical file sizes. Overlapping directories are shown separately and must not be added together. Unknown means inspection was incomplete.")}</p>
    <div className="field-grid"><label>{t("Keep logs and diagnostics for at least (days)")}<input type="number" min="1" max="3650" value={days} disabled={pending} onChange={e => setDays(e.target.value)} /></label></div>
    <div className="button-row"><ActionButton disabled={disabled || !Number.isInteger(Number(days)) || Number(days) < 1 || Number(days) > 3650} onClick={() => void execute(false)}>{pending ? t("Working…") : t("Preview cleanup")}</ActionButton><ActionButton disabled={pending} onClick={() => void load()}>{t("Refresh saved result")}</ActionButton></div>
    {error && <p className="form-error">{error}</p>}
    {arrayValue(preview, "areas").map(area => <details key={stringValue(area, "path")}><summary>{stringValue(area, "kind")} · {bytes(asObject(area).bytes)}</summary><p className="field-help">{stringValue(area, "path")}</p>{asObject(area).error ? <p>{stringValue(area, "error")}</p> : null}</details>)}
    {typeof preview.preview_id === "string" && <>
      <p className="field-help">{t("This preview expires after 15 minutes. Changed files are preserved. Stop Harness before cleanup. The newest logs and latest failure diagnostics are always retained.")}</p>
      <DataList items={arrayValue(preview, "items")} emptyTitle={t("No cleanup candidates")} emptyDetail={t("Unrecognized downloads and local recovery backups are preserved.")} render={item => {
        const id = stringValue(item, "id") || "", eligible = booleanValue(item, "eligible");
        return <><label><input type="checkbox" checked={selected.includes(id)} disabled={!eligible || disabled || result.preview_id === preview.preview_id} onChange={e => setSelected(current => e.target.checked ? [...current, id] : current.filter(value => value !== id))}/>{stringValue(item, "name")} · {bytes(asObject(item).bytes)}</label><span>{stringValue(item, "reason") || t("Can be removed")}</span></>;
      }}/>
      <ActionButton tone="danger" disabled={disabled || selected.length === 0 || result.preview_id === preview.preview_id} onClick={() => void execute(true)}>{t("Remove selected files")} ({selected.length})</ActionButton>
    </>}
    {result.state ? <><h3>{t("Last cleanup result")} · {String(result.state)}</h3><DataList items={arrayValue(result, "items")} emptyTitle={t("No files selected")} emptyDetail="" render={item => <><strong>{stringValue(item, "name")}</strong><span>{stringValue(item, "state")}</span>{asObject(item).error ? <p className="form-error">{stringValue(item, "error")}</p> : null}</>}/></> : null}
  </Panel></section>;
}

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
      <div className="page-heading"><div><span className="kicker">{t("Workbench")}</span><h1>{t("Workbench")}</h1><p>{t("Service status at a glance: Agent, Harness, active profile, and the Harness web UI.")}</p></div><StatusPill label={agentRunning ? t("Running") : t("Standby")} tone={agentRunning ? "good" : "warn"} /></div>
      {(() => {
        const harnessState = stringValue(harness, "state");
        const controlGate = harnessControlGate(harnessState, numberValue(harness, "pid"), busyAction !== null, snapshot.startup?.available === true);
        const startDisabled = controlGate.controlsDisabled || harnessState === "running" || harnessState === "starting" || harnessState === "stopping";
        const restartDisabled = controlGate.controlsDisabled || harnessState === "starting" || harnessState === "stopping" || harnessState === "detached";
        const stopDisabled = controlGate.controlsDisabled || !["running", "starting", "failed"].includes(harnessState || "");
        const harnessAction = (action: string) => void runAction(t(`Harness ${action}`), "/v1/harness", { action });
        return <div className="metric-grid">
        <Metric label={t("Agent lifecycle")} value={localizedRuntimeState(stringValue(state, "lifecycle"), t)} detail={localizedRuntimeState(stringValue(health, "status"), t)} actions={<ActionButton tone="primary" disabled={agentRestartDisabled} onClick={() => void runAction(t("Force restart Agent"), "/v1/agent", { action: "restart" })}><ArrowsClockwise size={16} />{t("Force restart Agent")}</ActionButton>} />
        <Metric label={t("Harness")} value={localizedRuntimeState(harnessState, t)} detail={stringValue(harness, "pid") ? t("PID {pid}", { pid: stringValue(harness, "pid") || "" }) : t("No child process")} actions={<>{harnessState !== "running" && <ActionButton tone="primary" disabled={startDisabled} onClick={() => harnessAction("start")}><CheckCircle size={16} />{t("Start")}</ActionButton>}{(harnessState === "running" || harnessState === "failed") && <ActionButton disabled={restartDisabled} onClick={() => harnessAction("restart")}><ArrowsClockwise size={16} />{t("Restart")}</ActionButton>}{(harnessState === "running" || harnessState === "starting") && <ActionButton tone="danger" disabled={stopDisabled} onClick={() => harnessAction("stop")}><StopCircle size={16} />{t("Stop")}</ActionButton>}</>}>{harnessState === "failed" && <div className="button-row"><ActionButton onClick={() => setFailLogOpen(true)}>{t("Show startup log")}</ActionButton></div>}{harnessState === "failed" && failLogOpen && <Modal title={t("Startup log tail")} onClose={() => setFailLogOpen(false)}><RecoveryLogTail snapshot={snapshot} /></Modal>}</Metric>
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
  const operation = asObject(asObject(snapshot.updates).operation);
  const installing = !!operation.phase && !coldOperationIsTerminal(operation.phase);
  const missingHarness = needsHarnessInstall(snapshot.config, snapshot.releases);
  const current = stringValue(snapshot.releases, "current_release");
  const release = arrayValue(snapshot.releases, "releases").find(item => stringValue(item, "id") === current);
  const controlsDisabled = controlGate.controlsDisabled || installing || !!snapshot.lifecycleBusy;
  const startDisabled = missingHarness || controlsDisabled || state === "running" || state === "starting" || state === "stopping";
  const restartDisabled = missingHarness || controlsDisabled || state === "starting" || state === "stopping" || state === "detached";
  const stopDisabled = controlsDisabled || !["running", "starting", "failed"].includes(state || "");
  const harnessAction = (action: string) => void runAction(t(`Harness ${action}`), "/v1/harness", { action });
  return (
    <Panel title={t("Start and use")} icon={<MonitorPlay size={18} />}>
      <p>{t("Current version")}: {stringValue(release, "version") || current || t("None selected")} · {t("Active profile")}: {stringValue(snapshot.profiles, "active_profile") || t("None selected")}</p>
      {missingHarness && <p className="field-help">{t("Install a version above before starting Harness.")}</p>}
      {installing && <p role="status">{t("Installing a version also selects it. Start Harness when installation and compatibility checks finish.")}</p>}
      <div className="button-row">
        <ActionButton tone="primary" disabled={startDisabled} onClick={() => harnessAction("start")}><CheckCircle size={16} />{t("Start")}</ActionButton>
        <ActionButton disabled={restartDisabled} onClick={() => harnessAction("restart")}><ArrowsClockwise size={16} />{t("Restart")}</ActionButton>
        <ActionButton tone="danger" disabled={stopDisabled} onClick={() => harnessAction("stop")}><StopCircle size={16} />{t("Stop")}</ActionButton>
      </div>
      {controlGate.externallyManaged && <p className="field-help" role="status">{t("Harness is running outside this Agent process. Manage it from its owning Agent; lifecycle controls are disabled here.")}</p>}
      {state === "detached" && !missingHarness && <><p className="field-help" role="status">{t("Harness is detached. Configure it in Settings, then start it from the control panel.")}</p>{openSettings && <div className="button-row"><ActionButton onClick={openSettings}><Gear size={16} />{t("Configure Harness")}</ActionButton></div>}</>}
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

export function CompatibilitySummary({ snapshot, busyAction, runAction }: Pick<ViewProps, "snapshot" | "busyAction" | "runAction">) {
  const { locale, t } = useI18n();
  const report = asObject(asObject(snapshot.profiles).compatibility);
  const policy = arrayValue(snapshot.profiles, "disabled_plugins").map(String);
  const [selected, setSelected] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const reportKey = `${stringValue(report, "source_profile")}:${stringValue(report, "release_id")}:${numberValue(report, "checked_at_unix")}`;
  useEffect(() => setSelected([]), [reportKey]);
  const hasReport = Object.keys(report).length > 0;
  if (!hasReport && !policy.length) return null;
  const triggerLabel = (value: string | undefined) => value === "version_switch" ? t("During version switch") : value === "profile_switch" ? t("During profile switch") : value === "startup" ? t("Before startup or restart") : t("Legacy record: trigger not recorded");
  const disabled = arrayValue(report, "disabled");
  const needsChoice = stringValue(report, "status") === "needs_choice";
  const failedReport = stringValue(report, "status") === "failed";
  const candidates = arrayValue(report, "candidates");
  const source = stringValue(report, "source_profile") || stringValue(snapshot.profiles, "active_profile");
  const target = stringValue(report, "release_id");
  const recovery = asObject(snapshot.recovery);
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), asObject(recovery.harness).state, busyAction !== null || saving);
  const blocked = gate.disabled;
  const saveChoices = async () => {
    setSaving(true);
    try {
      for (const packageName of selected) {
        if (!await runAction(t("Disable plugin in isolated profiles"), "/v1/profiles", { action: "plugin_disable", profile: source, package: packageName })) return;
      }
      setSelected([]);
    } finally { setSaving(false); }
  };
  const installed = arrayValue(snapshot.releases, "releases").some(item => stringValue(item, "id") === target);
  const operation = asObject(asObject(snapshot.updates).operation);
  const retryTag = stringValue(operation, "release_id") === target ? stringValue(operation, "tag") : null;
  const trigger = stringValue(report, "last_trigger") || stringValue(report, "trigger");
  const retryLabel = trigger === "profile_switch" ? t("Retry profile switch") : trigger === "startup" ? t("Retry Harness startup") : t("Retry version switch");
  const retry = () => trigger === "profile_switch"
    ? runAction(retryLabel, "/v1/profiles", { action: "select", profile: source })
    : trigger === "startup" ? runAction(retryLabel, "/v1/harness", { action: "start" })
    : installed
    ? runAction(t("Retry version switch"), "/v1/releases", { action: "promote", id: target })
    : runAction(t("Retry version switch"), "/v1/updates", { action: "switch", tag: retryTag, source: stringValue(operation, "source") || "official", mode: stringValue(operation, "mode") || "portable" });
  return <Panel title={t("Startup compatibility check")} icon={<SlidersHorizontal size={18} />}>
    <p>{t("Source profile")}: {source}{hasReport && <> · {t("Release")}: {target}</>}</p>
    {hasReport && !needsChoice && <p>{t("Effective isolated profile")}: {stringValue(report, "effective_profile")}</p>}
    {hasReport && <div className="status-block">
      <span>{t("Checked at")}: {formatTimestamp(numberValue(report, "checked_at_unix"), t("Not available"), locale)} · {triggerLabel(stringValue(report, "trigger"))}</span>
      <span>{booleanValue(report, "cache_reused") ? t("Reused previous check result") : stringValue(report, "trigger") ? t("New check result") : t("Legacy record: trigger not recorded")}{numberValue(report, "last_used_at_unix") ? ` · ${t("Last used")}: ${formatTimestamp(numberValue(report, "last_used_at_unix"), t("Not available"), locale)} · ${triggerLabel(stringValue(report, "last_trigger"))}` : ""}</span>
      <span>{t("Plugin errors below were recorded during this check; they are not new errors from viewing this page.")}</span>
    </div>}
    {hasReport && <StatusPill label={failedReport ? t("Startup check failed") : needsChoice ? t("Choose how to handle plugin errors") : disabled.length ? t("Started with isolated plugins") : t("Startup check passed")} tone={failedReport ? "bad" : needsChoice || disabled.length ? "warn" : "good"} />}
    <p>{t("Checks plugin loading and initialization, not every runtime feature. Original profile and data remain unchanged.")}</p>
    {failedReport && <p className="form-error" role="alert">{stringValue(report, "error")}</p>}
    {disabled.length > 0 && <ul>{disabled.map((item) => <li key={stringValue(item, "package")}><strong>{stringValue(item, "package")}</strong>: {t(stringValue(item, "reason") || "")}</li>)}</ul>}
    {policy.length > 0 && <div className="status-block"><strong>{t("Saved plugin choices; effective on next check")}</strong>{policy.map(name => <div key={name}>{name} <ActionButton disabled={blocked} onClick={() => void runAction(t("Restore plugin on next check"), "/v1/profiles", { action: "plugin_enable", profile: source, package: name })}>{t("Restore plugin on next check")}</ActionButton></div>)}</div>}
    {needsChoice && <div className="status-block">
      <p className="form-error">{stringValue(report, "error")}</p>
      <p>{t("Choose plugins to disable, then retry. Unattributed plugins are options, not confirmed faults. Nothing is uninstalled.")}</p>
      {candidates.map(item => { const name = stringValue(item, "package") || ""; return <label key={name}><input type="checkbox" checked={selected.includes(name)} disabled={blocked} onChange={event => setSelected(current => event.target.checked ? [...current, name] : current.filter(p => p !== name))} /> <strong>{name}</strong> · {t(stringValue(item, "reason") || "")}</label>; })}
      <div className="button-row">
        <ActionButton disabled={blocked || !candidates.length} onClick={() => setSelected(candidates.map(item => stringValue(item, "package") || ""))}>{t("Select all third-party plugins")}</ActionButton>
        <ActionButton disabled={blocked || !selected.length} onClick={() => void saveChoices()}>{t("Save disabled plugins")}</ActionButton>
        {(installed || retryTag || trigger === "profile_switch" || trigger === "startup") && <ActionButton disabled={blocked || selected.length > 0} onClick={() => void retry()}>{retryLabel}</ActionButton>}
      </div>
      <p>{t("Saved choices apply to isolated profiles until restored. The original profile remains intact.")}</p>
      {!installed && !retryTag && <p>{t("After saving, select the upstream version again to retry.")}</p>}
      {blocked && <p>{t("Stop Harness before changing plugin isolation.")}</p>}
    </div>}
  </Panel>;
}

function currentStartupFailure(snapshot: Snapshot): boolean {
  const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
  if (stringValue(runtime, "state") !== "failed") return false;
  const report = asObject(asObject(snapshot.profiles).compatibility);
  const failedAt = numberValue(runtime, "updated_at_unix");
  const checkedAt = numberValue(report, "last_used_at_unix");
  return failedAt === undefined || checkedAt === undefined || failedAt >= checkedAt;
}

export function CompatibilityDialog({ snapshot, busyAction, runAction, pending, onClose, basicResult, basicError }: Pick<ViewProps, "snapshot" | "busyAction" | "runAction"> & { pending: boolean; onClose: () => void; basicResult?: JsonObject | null; basicError?: string }) {
  const { t } = useI18n();
  const failed = currentStartupFailure(snapshot);
  const report = asObject(asObject(snapshot.profiles).compatibility);
  return <Modal title={t("Startup compatibility check")} onClose={onClose}>
    <BasicStartupCheck disabled={pending || busyAction !== null} initialResult={basicResult} initialError={basicError} />
    {snapshot.startup?.harness_startup_error && <p className="form-error" role="alert">{snapshot.startup.harness_startup_error}</p>}
    {pending ? <p role="status">{t("Checking plugin compatibility. You can close this dialog; the check continues in the background.")}</p> : <>
      {failed && <div role="alert"><p className="form-error">{t("Harness startup failed. The preflight result below does not mean this startup succeeded.")}</p><RecoveryLogTail snapshot={snapshot} /><ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Retry Harness startup"), "/v1/harness", { action: "start" })}>{t("Retry Harness startup")}</ActionButton></div>}
      <CompatibilitySummary snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
      {!Object.keys(report).length && !failed && <p>{t("No compatibility check result yet.")}</p>}
    </>}
  </Modal>;
}

export function BasicStartupCheck({ disabled, initialResult, initialError, onResult }: { disabled: boolean; initialResult?: JsonObject | null; initialError?: string; onResult?: (result: JsonObject | null) => void }) {
  const { t } = useI18n();
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<JsonObject | null>(initialResult ?? null);
  const [error, setError] = useState(initialError ?? "");
  useEffect(() => { setResult(initialResult ?? null); setError(initialError ?? ""); }, [initialResult, initialError]);
  const check = async () => {
    setChecking(true); setError(""); setResult(null); onResult?.(null);
    try {
      const response = await proxyRequest("/v1/preflight");
      if (!validStartupCheck(response)) throw new Error(t("Invalid startup check response. Retry the check or export diagnostics."));
      setResult(response); onResult?.(response);
    } catch (failure) { setError(String(failure)); }
    finally { setChecking(false); }
  };
  return <section aria-label={t("Basic startup checks")}>
    <h3>{t("Basic startup checks")}</h3>
    <p>{t("Checks files, data access, profile, runtime, port and pending recovery. Does not compile or start Harness.")}</p>
    <ActionButton disabled={disabled || checking} onClick={() => void check()}>{t(checking ? "Checking…" : "Run basic checks")}</ActionButton>
    {error && <p className="form-error" role="alert">{error}</p>}
    {result && <div className="status-block" aria-live="polite">
      <>{booleanValue(result, "paused") && <p className="notice">{t("Harness startup is paused. Checks remain available; leave recovery mode before starting.")}</p>}</><strong>{t(booleanValue(result, "ready") ? "No blocking issues found" : "Resolve the blocking issues before startup")}</strong>
      <ul>{arrayValue(result, "checks").map((entry, index) => <li key={`${stringValue(entry, "id")}-${index}`}>
        <StatusPill label={t(stringValue(entry, "status") || "Unknown")} tone={stringValue(entry, "status") === "blocked" ? "bad" : stringValue(entry, "status") === "warning" ? "warn" : "good"} />
        <strong> {t(stringValue(entry, "id") || "Check")}</strong>: {stringValue(entry, "reason")}
        {stringValue(entry, "next") && <p>{stringValue(entry, "next")}</p>}
      </li>)}</ul>
      <p>{t("Results describe this check only. Startup protection and plugin compatibility checks still apply.")}</p>
    </div>}
  </section>;
}

export function ProfilesView(props: ViewProps) {
  const { t } = useI18n();
  const { snapshot, busyAction, runAction } = props;
  const active = stringValue(snapshot.profiles, "active_profile");
  const [expandedProfiles, setExpandedProfiles] = useState<string[]>([]);
  const [newProfileName, setNewProfileName] = useState("");
  const toggleProfile = (name: string) => setExpandedProfiles((current) => current.includes(name) ? current.filter((item) => item !== name) : [...current, name]);
  const manifests = arrayValue(snapshot.profiles, "manifests");
  const recovery = asObject(snapshot.recovery);
  const harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), harness.state, busyAction !== null);
  return <><PageIntro kicker={t("Control / Profiles")} title={t("Profiles")} detail={t("Profiles own checkpoints and the plugin inventory: select a profile, manage its checkpoints, then adjust its plugins. Profile deletion is unavailable in this release.")} /><Panel title={t("Profile catalog")} icon={<SlidersHorizontal size={18} />}>
    <div className="button-row">{[
      ["settings", t("Open settings.yaml")],
      ["profile_patch", t("Edit profile patch")],
      ["plugin_manifest", t("Edit plugin manifest")],
      ["profile_dir", t("Open profile directory")],
    ].map(([target, label]) => <ActionButton key={target} disabled={busyAction !== null} onClick={() => void runAction(label, "/v1/profiles", { action: "open_path", target })}>{label}</ActionButton>)}
      <ActionButton disabled={busyAction !== null || !active} onClick={() => void runAction(t("Open DSH terminal"), "/v1/profiles", { action: "open_terminal", profile: active })}>{t("Open DSH terminal")}</ActionButton>
    </div>
    <p className="field-help">{t("The DSH terminal opens in the active profile directory with dsh and pnpm on PATH.")}</p>
    {gate.reason === "stop_required" || gate.reason === "not_stopped" ? <p className="form-error"><WarningCircle size={15} />{t("Stop Harness before switching profiles or removing plugins.")}</p> : null}
    <DataList items={manifests} emptyTitle={t("No valid native profiles")} emptyDetail={t("Only valid profile manifests are selectable.")} render={(item) => { const name = stringValue(item, "name") || t("Unnamed profile"); const expanded = expandedProfiles.includes(name); return <><div className="profile-row-toggle" onClick={() => toggleProfile(name)}><span className="profile-chevron" aria-hidden="true">{expanded ? "▾" : "▸"}</span><strong>{name}</strong>{name === active && <StatusPill label={t("Active")} tone="good" />}<span>{t("{count} bundles", { count: arrayValue(item, "bundles").length })}</span></div><span className="row-meta">{name !== active && <ActionButton disabled={gate.disabled} onClick={() => void runAction(t("Profile selection"), "/v1/profiles", { action: "select", profile: name })}>{t("Select")}</ActionButton>}</span></>; }} />
    <div className="button-row"><input className="form-input" value={newProfileName} placeholder={t("New profile name")} disabled={busyAction !== null} onChange={(event) => setNewProfileName(event.target.value)} /><ActionButton tone="primary" disabled={busyAction !== null || !newProfileName.trim()} onClick={() => void runAction(t("Create profile"), "/v1/profiles", { action: "create", profile: newProfileName.trim() }).then(() => setNewProfileName(""))}>{t("Create profile")}</ActionButton></div>
  </Panel>
  <RestoreStatusPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
  {manifests.map(asObject).map((item) => { const name = stringValue(item, "name"); if (!name || !expandedProfiles.includes(name)) return null; return <div key={name} className="profile-children"><div className="profile-children-title">{t("Belongs to profile")}: {name}</div>
  <CheckpointsView {...props} embedded profileFilter={name} />
  <ProfilePlugins {...props} profile={name} />
  </div>; })}
  </>;
}

export function OperationStatusPanel({ snapshot, onOpen }: { snapshot: Snapshot; onOpen: (module: "guide" | "profiles" | "maintenance", anchor: string) => void }) {
  const { t, locale } = useI18n();
  const records = operationSummaries(snapshot as unknown as JsonObject);
  if (!records.length) return null;
  return <Panel title={t("Operation status")} icon={<Pulse size={18} />}><p className="field-help">{t("Saved results remain visible until replaced or explicitly cleared. Open details to use the operation's recovery controls.")}</p>{records.map(item => <div className="status-block" key={item.id}>
    <strong>{t(item.title)} · {t(item.status)}</strong><span>{localizedRuntimeState(item.phase, t)}</span>
    {item.time !== undefined && <small>{formatTimestamp(item.time, t("Not available"), locale)}</small>}
    {item.progress !== undefined && <progress aria-label={t("Update progress")} max="100" value={item.progress}>{item.progress}%</progress>}
    {item.archivePath && <ArchivePath path={item.archivePath} exported={item.title === "Offline package export" && item.phase === "succeeded"} />}
    {item.error && <p className="form-error">{item.error.split("\n").map(line => t(line)).join("\n")}</p>}
    <ActionButton onClick={() => onOpen(item.module, item.anchor)}>{t("Open operation details")}</ActionButton>
  </div>)}</Panel>;
}

export function RestoreStatusPanel({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  const recovery = asObject(snapshot.recovery), harness = asObject(recovery.harness);
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), harness.state, busyAction !== null);
  const disabled = gate.disabled || snapshot.startup?.available !== true;
  const pending = asObject(asObject(snapshot.checkpoints).pending_restore ?? recovery.pending_restore);
  const healthyError = stringValue(snapshot.checkpoints, "healthy_capture_error");
  return <section id="restore-status">
    {healthyError && <div className="notice action-error"><WarningCircle size={17} /><span>{t("Healthy snapshot capture failed")}: {healthyError}</span></div>}
    {Object.keys(pending).length > 0 && <Panel title={t("Pending restore")} icon={<WarningCircle size={18} />}><dl className="detail-list compact-details"><div><dt>{t("Checkpoint")}</dt><dd>{stringValue(pending, "checkpoint_id")}</dd></div><div><dt>{t("State")}</dt><dd>{localizedRuntimeState(stringValue(pending, "state"), t)}</dd></div><div><dt>{t("Last error")}</dt><dd>{stringValue(pending, "error") || t("None reported")}</dd></div></dl><div className="button-row">{booleanValue(pending, "retryable") && <ActionButton disabled={disabled} onClick={() => void runAction(t("Retry restore"), "/v1/checkpoints", { action: "retry", id: stringValue(pending, "checkpoint_id") })}>{t("Retry")}</ActionButton>}{booleanValue(pending, "abortable") && <ActionButton tone="danger" disabled={disabled} onClick={() => void runAction(t("Abort restore"), "/v1/checkpoints", { action: "abort", id: stringValue(pending, "checkpoint_id") })}>{t("Abort")}</ActionButton>}</div></Panel>}
  </section>;
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
  const loadDetail = async (id: string, action: "detail" | "inspect") => {
    const token = latest.current.begin(); setDetailLoading(true); setDetailError(null);
    try { const value = await proxyRequest<JsonObject>("/v1/checkpoints", "POST", { action, id }); if (latest.current.isCurrent(token)) setDetail(value); }
    catch (cause) { if (latest.current.isCurrent(token)) { setDetail(null); setDetailError(errorMessage(cause)); } }
    finally { if (latest.current.isCurrent(token)) setDetailLoading(false); }
  };
  return <>{!embedded && <PageIntro kicker={t("State / Checkpoints")} title={t("Checkpoints")} detail={`${t("Checkpoint manifests contain only Harness profile/release selection. Agent lifecycle and Harness runtime are never saved or restored.")} ${t("Manual checkpoints contain a bounded redacted snapshot. Legacy entries restore selection metadata only.")}`} />}
    {!embedded && <RestoreStatusPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />}
    <Panel title={t("Saved checkpoints")} icon={<ListChecks size={18} />}><div className="panel-toolbar"><span className="toolbar-count">{t("{count} saved", { count: items.length })}</span><ActionButton tone="primary" disabled={gate.disabled || snapshot.startup?.available !== true} onClick={() => void runAction(t("Checkpoint creation"), "/v1/checkpoints", { action: "create", note: t("Native launcher checkpoint") })}><CheckCircle size={16} />{t("Create checkpoint")}</ActionButton></div><DataList items={items} emptyTitle={t("No checkpoints yet")} emptyDetail={t("Create a checkpoint after the Agent has a stable profile and release state.")} render={(item) => { const id = stringValue(item, "id") || ""; const reference = asObject(asObject(item).snapshot); const summary = asObject(reference.summary); const legacy = !Object.keys(reference).length; return <><div><strong>{id || t("Checkpoint")}</strong><StatusPill label={legacy ? t("Legacy metadata only") : localizedRuntimeState(stringValue(summary, "kind"), t)} tone={legacy ? "warn" : "good"}/><span>{stringValue(item, "profile") || t("No profile")} · {stringValue(summary, "dsh_version") || stringValue(item, "release") || t("Unknown version")}</span></div><span className="row-meta">{formatTimestamp(numberValue(item, "created_at_unix"), t("Not available"), locale)} <ActionButton disabled={detailLoading || legacy} onClick={() => void loadDetail(id, "detail")}>{t("Detail")}</ActionButton><ActionButton disabled={detailLoading || legacy} onClick={() => void loadDetail(id, "inspect")}>{t("Inspect")}</ActionButton><ActionButton disabled={gate.disabled} onClick={() => void runAction(t("Restore checkpoint"), "/v1/checkpoints", { action: "restore", id })}>{t("Restore")}</ActionButton></span></>; }} /></Panel>
    <Panel title={t("Snapshot inventory")} icon={<ClipboardText size={18} />}><p className="field-help">{t("Snapshots restore bounded profile and Harness settings files plus the pointer to an installed program version. Project files, full session data, runtimes and complete program copies are excluded. Install a missing version first. Use Retry or Abort for an interrupted restore.")}</p>{booleanValue(snapshot.checkpoints, "inventory_refresh_pending") ? <p className="field-help" role="status">{t("Snapshot inventory refreshes after capture finishes. Existing snapshots have not been removed.")}</p> : <DataList items={snapshots} emptyTitle={t("No snapshots reported")} emptyDetail={t("Healthy and manual snapshots appear here after capture.")} render={(item) => { const summary = asObject(asObject(item).summary); const id = stringValue(item, "snapshot_id") || stringValue(summary, "snapshot_id") || ""; return <><div><strong>{id}</strong><StatusPill label={localizedRuntimeState(stringValue(summary, "kind"), t)} tone={booleanValue(item, "valid") ? "good" : "bad"}/><span>{stringValue(summary, "profile_name")} · {stringValue(summary, "dsh_version")} · {numberValue(summary, "file_count") ?? 0} {t("files")}</span></div><span className="row-meta"><ActionButton disabled={detailLoading} onClick={() => void loadDetail(id, "detail")}>{t("Detail")}</ActionButton><ActionButton disabled={detailLoading} onClick={() => void loadDetail(id, "inspect")}>{t("Inspect")}</ActionButton><ActionButton disabled={gate.disabled} onClick={() => { if (window.confirm(t("Restore this snapshot? Harness must be stopped."))) void runAction(t("Restore snapshot"), "/v1/checkpoints", { action: "restore", id }); }}>{t("Restore snapshot")}</ActionButton></span></>; }} />}</Panel>
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

export function ArchivePath({ path, exported }: { path: string; exported: boolean }) {
  const { t } = useI18n();
  const [copyState, setCopyState] = useState("");
  useEffect(() => setCopyState(""), [path]);
  return <div className="status-block"><label className="form-field"><span className="field-label">{t(exported ? "Exported package path" : "Archive path")}</span><input className="form-input" readOnly value={path} onFocus={event => event.target.select()} /></label><ActionButton onClick={async () => { try { await navigator.clipboard.writeText(path); setCopyState("Path copied"); } catch { setCopyState("Select the path and copy it manually."); } }}>{t("Copy path")}</ActionButton>{copyState && <small role="status">{t(copyState)}</small>}</div>;
}

export function OfflinePackagePanel({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  const [importPath, setImportPath] = useState("");
  const [exportPath, setExportPath] = useState("");
  const current = stringValue(snapshot.releases, "current_release") || "";
  const [releaseDraft, setReleaseDraft] = useState(() => ({ value: current, dirty: false }));
  useEffect(() => setReleaseDraft(draft => refreshEditableDraft(draft, current)), [current, releaseDraft.dirty]);
  const releases = arrayValue(snapshot.releases, "releases");
  const operation = nestedValue(snapshot.updates, "operation"), install = nestedValue(snapshot.updates, "install_operation");
  const phase = stringValue(operation, "phase");
  const locked = busyAction !== null || snapshot.startup?.available !== true || !!snapshot.lifecycleBusy
    || (!!phase && !coldOperationIsTerminal(phase)) || booleanValue(operation, "cleanup_pending")
    || stringValue(install, "phase") === "installing" || booleanValue(install, "cleanup_pending");
  const exportSelected = releases.some(item => stringValue(item, "id") === releaseDraft.value);
  return <section id="offline-packages"><Panel title={t("Offline packages")} icon={<Package size={18} />}><details className="advanced-settings"><summary>{t("Import or export an offline package")}</summary>
    <p className="field-help">{t("A package contains a built Windows x64 Harness version and its complete Node/npm/pnpm runtime, without user data. Dependency installation and builds are not performed; version probes will run.")}</p>
    <p className="field-help">{t("Integrity checks detect damaged packages; they do not authenticate the publisher. Only import packages from sources you trust.")}</p>
    <label className="form-field"><span className="field-label">{t("Package to import (full .tar.gz path)")}</span><input className="form-input" value={importPath} placeholder="D:\Offline\harness.tar.gz" disabled={locked} onChange={event => setImportPath(event.target.value)} /></label>
    <p className="field-help">{t("Import selects the verified version as current. Harness stays stopped; run startup checks before starting it.")}</p>
    <ActionButton disabled={locked || !offlineArchivePathValid(importPath)} onClick={() => void runAction(t("Offline package import"), "/v1/updates", offlinePackageCommand("offline_import", importPath))}>{t("Import package")}</ActionButton>
    <hr className="panel-divider" />
    <label className="form-field"><span className="field-label">{t("Version to export")}</span><select value={releaseDraft.value} disabled={locked} onChange={event => setReleaseDraft({ value: event.target.value, dirty: true })}><option value="">{t("Select an installed version")}</option>{releases.map(item => <option key={stringValue(item, "id")} value={stringValue(item, "id")}>{stringValue(item, "version") || stringValue(item, "id")}</option>)}</select></label>
    <label className="form-field"><span className="field-label">{t("Export destination (full .tar.gz path)")}</span><input className="form-input" value={exportPath} placeholder="D:\Offline\harness-export.tar.gz" disabled={locked} onChange={event => setExportPath(event.target.value)} /></label>
    <p className="field-help">{t("Choose a new file outside Nexus-managed data. Existing files are never overwritten. Export does not change the selected version.")}</p>
    <ActionButton disabled={locked || !exportSelected || !offlineArchivePathValid(exportPath)} onClick={() => void runAction(t("Offline package export"), "/v1/updates", offlinePackageCommand("offline_export", exportPath, releaseDraft.value))}>{t("Export package")}</ActionButton>
  </details></Panel></section>;
}

export function UpdatesView({ snapshot, busyAction, runAction, refresh, embedded }: ViewProps) {
  const { t, locale } = useI18n();
  const update = nestedValue(snapshot.updates, "update");
  const operation = nestedValue(snapshot.updates, "operation");
  const release = nestedValue(snapshot.updates, "release");
  const releases = arrayValue(snapshot.releases, "releases");
  const updateState = stringValue(update, "state");
  const runtime = nestedValue(snapshot.config, "runtime");
  const persistedSource = stringValue(runtime, "source") || "official";
  const currentUpdateSource = stringValue(nestedValue(snapshot.config, "update"), "source") || "";
  const persistedMode = stringValue(runtime, "mode") || "portable";
  const [tagList, setTagList] = useState<JsonObject | null>(null);
  const [selectedTag, setSelectedTag] = useState<string>("");
  const [tagsLoading, setTagsLoading] = useState(false);
  const [tagsError, setTagsError] = useState<string | null>(null);
  const [sourceDraft, setSourceDraft] = useState(() => ({ value: currentUpdateSource, dirty: false }));
  const [sourceSaving, setSourceSaving] = useState(false);
  const latestTags = useRef(createLatestRequest());
  useEffect(() => () => latestTags.current.cancel(), []);
  useEffect(() => {
    setSourceDraft(current => refreshEditableDraft(current, currentUpdateSource));
  }, [currentUpdateSource, sourceDraft.dirty]);
  const saveSource = async () => {
    if (sourceSaving || busyAction !== null) return;
    setSourceSaving(true);
    try { const saved = await runAction(t("Save update source"), "/v1/config", { action: "set_update_source", update: updateSourcePayload(sourceDraft.value) });
      setSourceDraft(current => finishDraftSave(current, saved === true));
    } finally { setSourceSaving(false); }
  };
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
  const configurationRecovery = asObject(asObject(snapshot.updates).configuration_recovery);
  const configurationRecoveryId = stringValue(configurationRecovery, "operation_id");
  const publicationRecovery = asObject(asObject(snapshot.updates).publication_recovery);
  const publicationId = stringValue(publicationRecovery, "operation_id");
  const installOperation = asObject(asObject(snapshot.updates).install_operation);
  const installId = stringValue(installOperation, "operation_id");
  const installPhase = stringValue(installOperation, "phase");
  const installCleanup = booleanValue(installOperation, "cleanup_pending");
  const operationPhase = stringValue(operation, "phase");
  const operationKind = stringValue(operation, "kind") || "cold_switch";
  const offlineExport = operationKind === "offline_export", offlineImport = operationKind === "offline_import";
  const archivePath = stringValue(operation, "archive_path") || "";
  const retryCommand = operationRetryCommand(operation, persistedSource, persistedMode);
  const operationId = stringValue(operation, "operation_id");
  const cleanupPending = booleanValue(operation, "cleanup_pending");
  const publishedId = stringValue(operation, "release_id");
  const slotVisible = !!publishedId && releases.some(item => stringValue(item, "id") === publishedId);
  const catalogCurrent = releaseCatalogIsCurrent(snapshot as unknown as JsonObject);
  const verificationPending = operationPhase === "succeeded" && !offlineExport && !catalogCurrent;
  const unpublishedSuccess = operationPhase === "succeeded" && !offlineExport && catalogCurrent && !slotVisible;
  const finished = !!operationId && coldOperationIsTerminal(operationPhase);
  const canDismiss = finished && !cleanupPending;
  const attemptDetails = <>
    {stringValue(operation, "output_tail") && <div className="snapshot-file"><strong>{t(offlineExport || offlineImport ? "Operation output" : "Install output")}</strong><pre>{stringValue(operation, "output_tail")}</pre></div>}
    {stringValue(operation, "warning")?.includes("bundled_pnpm_major_skew") && <p className="field-help"><WarningCircle size={15}/>{t("Using the bundled pnpm: it differs from the release's exact pnpm pin, but the major version matches.")}</p>}
    {(stringValue(operation, "error") || (!operationId && stringValue(update, "error"))) && <p className="form-error" role={canDismiss ? undefined : "alert"}><WarningCircle size={15}/>{stringValue(operation, "error") || stringValue(update, "error")}</p>}
  </>;
  return <><div id="installation-status" />{!embedded && <PageIntro kicker={t("Releases / Updates")} title={t("Updates")} detail={t("Cold switches are asynchronous and never start Harness automatically.")} />}
    
    
    <OfflinePackagePanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
    {installId && <Panel title={t("Installation")} icon={<CloudArrowUp size={18} />}>
      <p>{t("Current stage")}: {localizedRuntimeState(installPhase, t)}</p>
      <p>{stringValue(installOperation, "error")}</p><p>{stringValue(installOperation, "cleanup_error")}</p>
      <div className="button-row"><ActionButton onClick={() => void refresh()}>{t("Refresh")}</ActionButton>
      {(installPhase === "installing" || installCleanup) && <ActionButton tone="danger" onClick={() => void runAction(t("Cancel"), "/v1/updates", { action: "cancel", operation_id: installId })}>{installCleanup ? t("Retry cleanup") : t("Cancel")}</ActionButton>}</div>
    </Panel>}
    {!publicationId && configurationRecoveryId && <Panel title={t("Configuration recovery")} icon={<WarningCircle size={18} />}>
      <p>{t("Retry the interrupted settings save, or keep the current valid configuration and previous backup exactly as they are. This does not repair invalid configuration files or start Harness.")}</p>
      <div className="button-row"><ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Retry recovery"), "/v1/updates", { action: "configuration_retry", operation_id: configurationRecoveryId })}>{t("Retry recovery")}</ActionButton>
      <ActionButton disabled={busyAction !== null || !booleanValue(configurationRecovery, "can_preserve")} onClick={() => void runAction(t("Keep current and end recovery"), "/v1/updates", { action: "configuration_abandon", operation_id: configurationRecoveryId })}>{t("Keep current and end recovery")}</ActionButton></div>
    </Panel>}
    {publicationId && <Panel title={t("Publication recovery")} icon={<WarningCircle size={18} />}>
      <p>{t("Retry interrupted publication, or keep current configuration and every existing version and candidate file. Keeping current ends this recovery without compiling or starting Harness. Retained candidate files are not automatically cleaned.")}</p>
      <p className="field-help">{stringValue(publicationRecovery, "reason")}</p>
      <div className="button-row"><ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Retry recovery"), "/v1/updates", { action: "publication_retry", operation_id: publicationId })}>{t("Retry recovery")}</ActionButton>
      <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Keep current and end recovery"), "/v1/updates", { action: "publication_abandon", operation_id: publicationId })}>{t("Keep current and end recovery")}</ActionButton></div>
    </Panel>}
    <Panel title={t("Upstream tags & cold switch")} icon={<CloudArrowUp size={18} />}><p className="field-help">{t("Nexus prefers system Git and falls back to embedded Git when unavailable or unsuccessful. Embedded Git does not provide a git command for plugins.")}</p><div className="status-block"><ActionButton disabled={tagsLoading} onClick={() => void loadTags()}>{tagsLoading ? t("Listing tags") : t("List upstream tags")}</ActionButton>{tagsError ? <span>{tagsError} · <button className="button subtle" onClick={() => void loadTags()}>{t("Refresh")}</button></span> : <span>{tagList ? `${t("Source")}: ${stringValue(tagList, "source")}` : t("No tags loaded")}</span>}<details><summary>{t("Advanced source settings")}</summary><div className="kv-row"><input className="form-input" value={sourceDraft.value} placeholder="https://github.com/deepseek-ai/deepseek-harness" disabled={busyAction !== null || sourceSaving} onChange={(event) => setSourceDraft({ value: event.target.value, dirty: true })} /><ActionButton disabled={busyAction !== null || sourceSaving || !sourceDraft.value.trim() || sourceDraft.value === currentUpdateSource} onClick={() => void saveSource()}>{t("Save update source")}</ActionButton>{sourceDraft.dirty && <ActionButton disabled={sourceSaving} onClick={() => setSourceDraft({ value: currentUpdateSource, dirty: false })}>{t("Cancel")}</ActionButton>}</div></details>{tags.length > 0 && <select value={selectedTag} onChange={(event) => setSelectedTag(event.target.value)} aria-label={t("Upstream tags")}><option value="">{t("Select a tag")}</option>{tags.map((tag) => <option key={tag} value={tag}>{tag}</option>)}</select>}{selectedTag && <ActionButton tone="primary" disabled={busyAction !== null || tagsLoading || cleanupPending || (!!operationId && !coldOperationIsTerminal(operationPhase))} onClick={() => void runAction(t("Switch to tag"), "/v1/updates", { action: "switch", tag: selectedTag, source: persistedSource, mode: persistedMode })}>{releases.some((item) => stringValue(item, "version") === selectedTag) ? t("Switch to tag") : t("Fetch this tag")}</ActionButton>}{selectedTag && <span>{`${t("Selected tag")}: ${selectedTag}`}</span>}</div>
    {(!!operationId || updateState === "running" || updateState === "failed") && <><hr className="panel-divider" /><div className="status-block">
      <strong>{offlineExport ? t("Offline package export") : offlineImport ? t("Offline package import") : finished ? t("Last installation") : t("Current stage")}: {verificationPending ? t("Verification pending") : unpublishedSuccess ? t("Verifying installed version") : localizedRuntimeState(operationPhase || updateState, t)}</strong>
      {finished && <span>{formatTimestamp(numberValue(operation, "updated_at_unix") ?? numberValue(operation, "started_at_unix"), t("Not available"), locale)}</span>}
      {finished && <p className="field-help">{t(offlineExport || offlineImport ? "This is the saved result of the last offline package operation." : "This is a saved installation record, not a new error from reinstalling Nexus.")}</p>}
      {archivePath && <ArchivePath path={archivePath} exported={offlineExport && operationPhase === "succeeded"} />}
      {offlineExport && operationPhase === "succeeded" && <p className="field-help">{t(cleanupPending ? "The package was exported. Temporary-file cleanup still needs attention." : "The package was exported. The selected version is unchanged.")}</p>}
      {offlineImport && operationPhase === "succeeded" && <p className="field-help">{t("Import selects the verified version as current. Harness stays stopped; run startup checks before starting it.")}</p>}
      {(stringValue(operation, "tag") || stringValue(update, "release_id")) && <span>{stringValue(operation, "tag") || stringValue(update, "release_id")}</span>}
      {unpublishedSuccess && <p className="form-error" role="alert">{t("The task reports completion, but its version slot is unavailable. Refresh to verify installation before starting Harness.")}</p>}
      {operationId && !finished && <progress aria-label={t("Update progress")} max="100" value={numberValue(operation, "progress_percent") || 0}>{numberValue(operation, "progress_percent") || 0}%</progress>}
      {canDismiss ? <details key={operationId}><summary>{t(offlineExport || offlineImport ? "Operation log and details" : "Installation log and details")}</summary>{attemptDetails}</details> : attemptDetails}
      {stringValue(operation, "cleanup_error") && <p className="form-error" role="alert"><WarningCircle size={15}/>{t("Cleanup error")}: {stringValue(operation, "cleanup_error")}</p>}
      {cleanupPending && <p className="notice degraded">{t("Cleanup is incomplete. Retry cleanup before starting another update.")}</p>}
      <div className="button-row"><ActionButton disabled={busyAction !== null} onClick={() => void refresh()}>{t("Refresh")}</ActionButton>{operationId && (!coldOperationIsTerminal(operationPhase) || cleanupPending) && <ActionButton tone="danger" disabled={(busyAction !== null && !snapshot.lifecycleBusy) || operationPhase === "cancelling"} onClick={() => void runAction(t(offlineExport || offlineImport ? "Cancel offline operation" : "Cancel cold switch"), "/v1/updates", { action: "cancel", operation_id: operationId })}>{cleanupPending ? t("Retry cleanup") : t("Cancel")}</ActionButton>}
        {canDismiss && operationPhase !== "succeeded" && retryCommand && <ActionButton tone="primary" disabled={busyAction !== null || snapshot.startup?.available !== true} onClick={() => void runAction(t(offlineExport || offlineImport ? "Retry offline operation" : "Retry installation"), "/v1/updates", retryCommand)}>{t(offlineExport || offlineImport ? "Retry offline operation" : "Retry installation")}</ActionButton>}
        {canDismiss && <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Clear finished record"), "/v1/updates", { action: "clear_finished", operation_id: operationId })}>{t("Clear finished record")}</ActionButton>}
      </div>
      {canDismiss && <span className="field-help">{t("Clearing this record keeps installed versions and Harness data.")}</span>}
    </div></>}</Panel>
    <Panel title={t("Release slots")} icon={<Package size={18} />}><DataList items={releases} emptyTitle={t("No release slots")} emptyDetail={t("A successful cold switch registers and promotes its immutable slot without starting Harness.")} render={(item) => { const slotId = stringValue(item, "id") || ""; const current = stringValue(snapshot.releases, "current_release"); const lkg = stringValue(snapshot.releases, "last_known_good"); const protectedSlot = slotId === current || slotId === lkg; return <><div><strong>{slotId || t("Release")}</strong><span>{stringValue(item, "version") || t("Unknown version")}{slotId === current ? ` · ${t("Current")}` : slotId === lkg ? ` · ${t("Last known good")}` : ""}</span></div><span className="row-meta">{slotId !== current && <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Switch to this version"), "/v1/releases", { action: "promote", id: slotId })}>{t("Switch to this version")}</ActionButton>}{!protectedSlot && <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Release slot"), "/v1/releases", { action: "remove", id: slotId })}>{t("Release slot")}</ActionButton>}</span></>; }} /></Panel>
  </>;
}




export function ProfilePlugins({ snapshot, busyAction, runAction, refresh, profile }: ViewProps & { profile?: string }) {
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
  const activeManifest = manifests.map(asObject).find((item) => stringValue(item, "name") === active) || {};
  const plugins = arrayValue(activeManifest, "plugins");
  const order = arrayValue(activeManifest, "bundles").map(String);
  const sourceProfile = stringValue(activeManifest, "source_profile");
  const orderUndoId = stringValue(activeManifest, "order_undo_id");
  const gate = recoveryMutationGate(booleanValue(recovery, "harness_stop_required"), harness.state, busyAction !== null || pluginBusy);
  const movePlugin = async (packageName: string, destination: string) => {
    if (gate.disabled || sourceProfile) return;
    const move = pluginMoveTarget(order, packageName, destination);
    if (!move) return;
    setPluginBusy(true); setPluginError(null); setOrderNotice(null); setPluginResult(null);
    try {
      const result = await runAction(t("Plugin load order"), "/v1/profiles", { action: "plugin_move", profile: active, package: packageName, target: move.target });
      if (result !== false) setOrderNotice(t("Load order saved. It takes effect on the next Harness startup."));
    } catch (cause) { setPluginError(errorMessage(cause)); }
    finally { setPluginBusy(false); }
  };
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
    <p className="field-help">{t("Undo restores only the last saved plugin order. Removing a plugin requires reinstalling it; configuration snapshots do not restore deleted dependencies.")}</p>
    {orderUndoId && <ActionButton disabled={gate.disabled || !!sourceProfile} onClick={() => void runAction(t("Undo plugin order"), "/v1/profiles", { action: "plugin_undo_move", profile: active, target: orderUndoId })}>{t("Undo plugin order")}</ActionButton>}
    {(booleanValue(recovery, "harness_stop_required") || gate.reason === "not_stopped") && <div className="notice degraded"><WarningCircle size={17}/><span>{t("Harness must be stopped before profile, plugin, or rollback changes. Diagnostics remain available.")}</span><ActionButton disabled={busyAction !== null} onClick={() => void runAction(t("Harness stop"), "/v1/harness", { action: "stop" })}>{t("Stop Harness")}</ActionButton></div>}
    <p className="panel-description">{t("Built-in plugins come from the profile template. Installed plugins are dependency-managed even when included in the load list.")}</p>
    <p className="field-help">{t("Drag plugins to change loading order, or use the arrow buttons. dsh-base and dsh-web-app stay in positions 1 and 2.")}</p>
    {sourceProfile && <p className="notice">{t("This is a generated isolation profile. Edit plugin order in source profile {profile}.", { profile: sourceProfile })}</p>}
    {!plugins.length ? <EmptyState title={t("No plugins reported")} detail={t("Select a valid native profile to inspect its inventory.")} /> : <div className="data-list">{plugins.map(item => {
      const packageName = stringValue(item, "package") || "";
      const builtin = booleanValue(item, "builtin"), removable = booleanValue(item, "removable");
      const index = order.indexOf(packageName), fixed = FIXED_PROFILE_PLUGINS.includes(packageName);
      const movable = index >= 0 && !fixed && !gate.disabled && !sourceProfile;
      return <div key={packageName} className={`data-row plugin-row${dropTarget === packageName ? " plugin-drop-target" : ""}`} data-plugin={packageName}
        onDragOver={event => { if (movable && draggedPlugin.current && draggedPlugin.current !== packageName) { event.preventDefault(); event.dataTransfer.dropEffect = "move"; setDropTarget(packageName); } }}
        onDragLeave={() => setDropTarget(current => current === packageName ? null : current)}
        onDrop={event => { event.preventDefault(); const source = draggedPlugin.current; draggedPlugin.current = null; setDropTarget(null); if (source && movable) void movePlugin(source, packageName); }}>
        <div><span className="plugin-drag-handle" draggable={movable} title={movable ? t("Drag to reorder") : fixed ? t("Fixed load position") : t("Loading order unavailable")}
          onDragStart={event => { if (!movable) { event.preventDefault(); return; } draggedPlugin.current = packageName; event.dataTransfer.effectAllowed = "move"; event.dataTransfer.setData("text/plain", packageName); }}
          onDragEnd={() => { draggedPlugin.current = null; setDropTarget(null); }} aria-hidden="true">{fixed ? "●" : index >= 0 ? "⠿" : "·"}</span>
          {index >= 0 && <span className="plugin-position">{index + 1}</span>}<strong>{packageName}</strong>
          <StatusPill label={builtin ? t("Built-in") : removable ? t("Removable") : t("Protected")} tone={removable ? "warn" : "neutral"}/>
          {fixed && <StatusPill label={t("Fixed load position")} tone="neutral"/>}
          {index < 0 && <span>{t("Dependency only; not in the load list")}</span>}
          <span>{stringValue(item, "version") || t("Unknown version")}</span></div>
        <span className="row-meta button-row">
          {index >= 0 && !fixed && <><ActionButton title={t("Move up")} disabled={!movable || index === 0 || FIXED_PROFILE_PLUGINS.includes(order[index - 1])} onClick={() => void movePlugin(packageName, order[index - 1])}>↑</ActionButton><ActionButton title={t("Move down")} disabled={!movable || index === order.length - 1 || FIXED_PROFILE_PLUGINS.includes(order[index + 1])} onClick={() => void movePlugin(packageName, order[index + 1])}>↓</ActionButton></>}
          {removable && <ActionButton tone="danger" disabled={gate.disabled || !!sourceProfile} onClick={() => void removePlugin(packageName)}>{pluginBusy ? t("Working") : t("Remove")}</ActionButton>}
        </span>
      </div>;
    })}</div>}
    {orderNotice && <p role="status">{orderNotice}</p>}
    {pluginError && <p className="form-error"><WarningCircle size={15}/>{pluginError} <button className="button subtle" onClick={() => setPluginError(null)}>{t("Dismiss")}</button></p>}
    {pluginResult && <pre className="output-block">{[stringValue(pluginResult, "stdout"), stringValue(pluginResult, "stderr")].filter(Boolean).join("\n") || t("Plugin removed. Inventory refreshed.")}</pre>}
  </Panel>;
}

export function RecoveryModePanel({ snapshot, busyAction, runAction }: Pick<ViewProps, "snapshot" | "busyAction" | "runAction">) {
  const { t } = useI18n();
  const paused = booleanValue(snapshot.recovery, "paused");
  const pauseError = stringValue(snapshot.recovery, "pause_error");
  const action = pauseError || !paused ? "enter" : "leave";
  const label = pauseError ? "Repair and enter recovery mode" : paused ? "Leave recovery mode" : "Enter recovery mode";
  return <section className="notice" aria-label={t("Harness recovery mode")}>
    <WarningCircle size={17} />
    <div><strong>{t(paused ? "Harness startup is paused" : "Harness recovery mode")}</strong>
      <p>{t(paused ? "Agent stays available. Repair profiles, plugins or configuration, run checks, then leave recovery mode. Leaving does not start Harness." : "Pause Harness startup and stop it to repair profiles, plugins or configuration. This pause survives restarting Nexus.")}</p></div>
    {pauseError && <div role="alert"><p>{pauseError}</p><p>{t("The invalid pause record will be preserved before repair. Unsafe files cannot be repaired automatically.")}</p></div>}
    <ActionButton disabled={busyAction !== null} onClick={() => void runAction(t(label), "/v1/recovery", { action })}>{t(label)}</ActionButton>
  </section>;
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
  return <><Panel title={t("Startup recovery status")} icon={<Pulse size={18} />}><dl className="detail-list"><div><dt>{t("Harness state")}</dt><dd>{localizedRuntimeState(stringValue(asObject(recovery.harness), "state"), t)}</dd></div><div><dt>{t("Startup error")}</dt><dd>{stringValue(recovery, "startup_error") || t("None reported")}</dd></div><div><dt>{t("Fatal prefix observed")}</dt><dd>{booleanValue(recovery, "fatal_prefix_observed") ? t("Yes, advisory only") : t("No")}</dd></div></dl>{errors.map((item) => <p className="form-error" key={item}>{item}</p>)}<div className="button-row"><ActionButton disabled={busyAction !== null} onClick={() => void refresh()}>{t("Refresh")}</ActionButton><ActionButton disabled={busyAction !== null || snapshot.startup?.available !== true} onClick={() => void runAction(t("Export diagnostics"), "/v1/diagnostics", { action: "export", note: t("Manual recovery collection") })}>{t("Export diagnostics")}</ActionButton></div></Panel><RecoveryLogTail snapshot={snapshot} /></>;
}

export function DiagnosticsView({ snapshot, busyAction, runAction, refresh, embedded }: ViewProps) {
  const { locale, t } = useI18n();
  const items = arrayValue(snapshot.diagnostics, "bundles");
  const controlsDisabled = busyAction !== null || snapshot.startup?.available !== true;
  const open = (bundle: string, file?: string) => void runAction(t(file ? "Open file" : "Open file location"), "/v1/diagnostics", { action: "open_path", bundle, ...(file ? { file } : {}) });
  return <>{!embedded && <PageIntro kicker={t("Observability / Diagnostics")} title={t("Diagnostics")} detail={t("Bundles are bounded, redacted, and limited to Nexus-owned metadata and text logs.")} />}
    <Panel title={t("Diagnostic bundles")} icon={<TerminalWindow size={18} />}><p>{t("The exported JSON is one portable file containing the redacted diagnostic context and logs.")}</p>
      <div className="panel-toolbar"><span className="toolbar-count">{t("{count} bundles", { count: items.length })}</span><ActionButton tone="primary" disabled={controlsDisabled} onClick={() => void runAction(t("Export diagnostics"), "/v1/diagnostics", { action: "export", note: t("Native launcher collection") })}><TerminalWindow size={16} />{t("Export diagnostics")}</ActionButton></div>
      {!items.length ? <EmptyState title={t("No diagnostic bundles")} detail={t("Collect a bounded bundle when a runtime issue needs review.")} /> : items.map(item => {
        const bundle = asObject(item), id = stringValue(bundle, "id") || "";
        return <details key={id} className="diagnostic-bundle">
          <summary>{id} · {t("{count} files", { count: arrayValue(bundle, "files").length })} · {formatTimestamp(numberValue(bundle, "created_at_unix"), t("Not available"), locale)}</summary>
          <p className="field-help">{stringValue(bundle, "directory")}</p>
          <div className="button-row"><ActionButton disabled={controlsDisabled || !id} onClick={() => open(id)}>{t("Open file location")}</ActionButton><ActionButton disabled={controlsDisabled || !id} onClick={() => open(id, "diagnostics.json")}>{t("Open bundle manifest")}</ActionButton></div>
          <DataList items={arrayValue(bundle, "files")} emptyTitle={t("No files collected")} emptyDetail={t("Open bundle manifest")} render={file => {
            const name = stringValue(file, "name") || "";
            return <><div><strong>{name}</strong><span>{numberValue(file, "bytes")} B</span></div><ActionButton disabled={controlsDisabled || !id || !name} onClick={() => open(id, name)}>{t("Open file")}</ActionButton></>;
          }} />
        </details>;
      })}
    </Panel><RecoveryDiagnostics snapshot={snapshot} busyAction={busyAction} runAction={runAction} refresh={refresh} /></>;
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

export function LaunchInputsPanel({ snapshot }: { snapshot: Snapshot }) {
  const { t } = useI18n();
  const explanation = nestedValue(snapshot.config, "launch_inputs");
  const recorded = asObject(explanation.running_launch);
  const current = launchInputMatches(recorded, asObject(snapshot.harnessRuntime)) ? recorded : {};
  let observedPort: string | null = null;
  if (Object.keys(current).length && harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi)) {
    try { const url = new URL(stringValue(snapshot.harnessUi, "url") || ""); observedPort = url.port || "80"; } catch { /* No verified URL yet. */ }
  }
  const show = (value: JsonObject) => Object.keys(value).length === 0 ? <p>{t("Launch input record unavailable")}</p> : <dl className="detail-list">{arrayValue(value, "fields").map((item, index) => {
    const row = asObject(item); return <div key={index}><dt>{t(stringValue(row, "name") || "Unknown")}</dt><dd>{t(stringValue(row, "value") || "Not available")}<small> · {t(stringValue(row, "source") || "Unknown")}</small></dd></div>;
  })}</dl>;
  return <Panel title={t("Launch configuration explained")} icon={<Info size={18}/>}>
    <p className="field-help">{t("These are launch inputs, not the final configuration after Harness applies patches. Inherited values have not been inspected.")}</p>
    <h3>{t("Next launch inputs")}</h3><p>{t("Changes apply on the next explicit launch.")}</p>{show(asObject(explanation.next_launch))}
    <h3>{t("Current instance launch inputs")}</h3>{show(current)}
    {observedPort && <p>{t("Observed current port")}: {observedPort}</p>}
  </Panel>;
}

function HarnessPreferencesPanel({ snapshot, busyAction, runAction }: Pick<ViewProps, "snapshot" | "busyAction" | "runAction">) {
  const { t } = useI18n();
  const saved = nestedValue(asObject(snapshot.config), "harness_preferences");
  const [draft, setDraft] = useState(() => preferencesDraft(saved));
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [profile, setProfile] = useState("");
  useEffect(() => { if (!dirty) setDraft(preferencesDraft(saved)); }, [dirty, snapshot.config]);
  const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
  const updates = asObject(snapshot.updates);
  const operation = asObject(updates.operation);
  const gate = runtimeSettingsGate(runtime.state, numberValue(runtime, "pid"), asObject(updates.update).state, operation.phase, booleanValue(operation, "cleanup_pending"), busyAction !== null);
  const disabled = gate.disabled || snapshot.startup?.available !== true;
  const change = (key: keyof HarnessPreferencesDraft, value: string) => {
    setDraft(current => ({ ...current, [key]: value })); setDirty(true); setError(null);
  };
  const field = (key: keyof HarnessPreferencesDraft, label: string, help?: string) => <label className="form-field" key={key}>
    <span className="field-label">{t(label)}</span>
    <input className="form-input" value={draft[key]} disabled={disabled} placeholder={t("Inherit upstream default")} onChange={event => change(key, event.target.value)} />
    {help && <span className="field-help">{t(help)}</span>}
  </label>;
  const choice = (key: keyof HarnessPreferencesDraft, label: string, options: string[]) => <label className="form-field" key={key}>
    <span className="field-label">{t(label)}</span><select className="form-input" value={draft[key]} disabled={disabled} onChange={event => change(key, event.target.value)}>
      <option value="">{t("Inherit upstream default")}</option>
      {options.map(value => <option key={value} value={value}>{value === "true" ? t("Enabled") : value === "false" ? t("Disabled") : value}</option>)}
    </select>
  </label>;
  const save = async () => {
    if (disabled) return;
    const result = preferencesPayload(draft);
    if (result.error) { setError(result.error); return; }
    if (await runAction(t("Save Harness preferences"), "/v1/config", { action: "set_harness_preferences", harness_preferences: result.value })) setDirty(false);
  };
  const profiles = arrayValue(snapshot.profiles, "manifests").map(item => stringValue(asObject(item), "name")).filter((name): name is string => !!name);
  return <Panel title={t("Harness preferences")} icon={<SlidersHorizontal size={18} />}>
    <p className="field-help">{t("Blank fields inherit upstream behavior. Changes apply on the next launch.")}</p>
    <div className="form-grid">
      {field("home", "Harness data directory", "Changing this path only changes where Harness looks for data. Existing files are not moved or deleted.")}
      {field("port", "Web port", "Web profiles only. Default 3080; 0 selects an available port.")}
      {choice("open_browser", "Open browser after launch", ["true", "false"])}
      {choice("telemetry_disabled", "Disable session telemetry", ["true", "false"])}
    </div>
    <p className="field-help">{t("Disabling telemetry stops session sharing. Inherited upstream behavior shares session records when feedback is submitted.")}</p>
    <details><summary>{t("Advanced Harness preferences")}</summary>
      <div className="form-grid">
        {field("deepseek_base_url", "DeepSeek model API address")}
        {field("search_base_url", "DeepSeek search API address")}
        {field("search_provider", "Search provider ID", "The named provider must already be installed and available.")}
        {field("fetch_provider", "Web fetch provider ID", "The named provider must already be installed and available.")}
        {field("agents_home", "Shared agent skills directory")}
        {field("bundled_skill_dir", "Bundled skills directory")}
        {choice("permission_mode", "Permission mode", ["read-only", "workspace-write", "danger-full-access"])}
        {choice("tools_mode", "Tool mode (temporary upstream option)", ["native", "ptc", "both"])}
      </div>
      <p className="field-help">{t("Danger full access removes the default sandbox restrictions and automatic approval prompts. Tool mode applies to web and headless profiles.")}</p>
      <label className="form-field"><span className="field-label">{t("Additional patch files, one absolute path per line")}</span><textarea className="form-input" rows={3} value={draft.patches} disabled={disabled} onChange={event => change("patches", event.target.value)} /></label>
      <p className="field-help">{t("Patch files customize plugins and are applied in the listed order. Select only files you trust.")}</p>
      <div className="form-grid">
        {field("context_window", "Context window (sdk-minimal only)")}
        {choice("max_tokens_as_success", "Treat token limit as success (sdk only)", ["true", "false"])}
      </div>
      <label className="form-field"><span className="field-label">{t("System prompt (sdk-minimal only)")}</span><textarea className="form-input" rows={3} value={draft.system_prompt} disabled={disabled} placeholder={t("Inherit upstream default")} onChange={event => change("system_prompt", event.target.value)} /></label>
    </details>
    {error && <p className="form-error" role="alert">{t(error)}</p>}
    {disabled && <p className="field-help" role="status">{t("Stop Harness and wait for updates and cleanup to finish before changing preferences.")}</p>}
    <div className="button-row"><ActionButton disabled={disabled || !dirty} onClick={() => void save()}>{t("Save Harness preferences")}</ActionButton><ActionButton disabled={!dirty || busyAction !== null} onClick={() => { setDraft(preferencesDraft(saved)); setDirty(false); setError(null); }}>{t("Discard changes")}</ActionButton></div>
    <hr className="panel-divider" />
    <p>{t("Active profile")}: {stringValue(snapshot.profiles, "active_profile") || t("None selected")}</p>
    <div className="button-row"><label className="form-field"><span className="field-label">{t("Switch profile separately")}</span><select className="form-input" value={profile} disabled={disabled} onChange={event => setProfile(event.target.value)}><option value="">{t("Keep current profile")}</option>{profiles.map(name => <option key={name} value={name}>{name}</option>)}</select></label><ActionButton disabled={disabled || !profile || dirty} onClick={() => void runAction(t("Profile selection"), "/v1/profiles", { action: "select", profile }).then(ok => { if (ok) setProfile(""); })}>{t("Switch profile")}</ActionButton></div>
    <p className="field-help">{t("Save or discard preference edits before switching profiles. Profiles are managed separately from these preferences.")}</p>
  </Panel>;
}

function SettingsView({ snapshot, themeMode, setThemeMode, busyAction, runAction }: ViewProps) {
  const [zoom, setZoom] = useState(displayZoom);
  useEffect(() => {
    const sync = (event: Event) => setZoom((event as CustomEvent<number>).detail);
    window.addEventListener(ZOOM_CHANGED, sync);
    return () => window.removeEventListener(ZOOM_CHANGED, sync);
  }, []);
  const { locale, setLocale, t } = useI18n();
  const [notificationsEnabled, setNotificationsEnabled] = useState(notificationsEnabledPreference());
  const [autostartEnabled, setAutostartEnabled] = useState<boolean | null>(null);
  const [buildIdentity, setBuildIdentity] = useState<Record<string, unknown>>({});
  useEffect(() => { void invoke<Record<string, unknown>>("build_identity").then(setBuildIdentity).catch(() => undefined); }, []);
  const [armedReset, setArmedReset] = useState<string | null>(null);
  const [logLevel, setLogLevel] = useState<string>(() => window.localStorage.getItem("nexus.launcher.agent-log-level") || "info");
  useEffect(() => {
    // Re-apply the persisted level whenever settings open; best-effort in
    // browser-only previews.
    void invoke("agent_log_set", { level: logLevel }).catch(() => undefined);
  }, [logLevel]);
  useEffect(() => {
    // Best-effort: the launcher desktop bundle answers; browser-only
    // development previews stay with an unavailable checkbox.
    invoke<boolean>("autostart_status")
      .then((value) => setAutostartEnabled(value === true))
      .catch(() => setAutostartEnabled(null));
  }, []);
  const toggleAutostart = async (enabled: boolean) => {
    try {
      await invoke("autostart_set", { enabled });
      setAutostartEnabled(enabled);
    } catch {
      setAutostartEnabled(null);
    }
  };
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
  const [argRows, setArgRows] = useState<Array<{ key: string; value: string }>>([]);

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
    setFormError(null);
  };


  const openEditor = () => {
    const nextDraft = harnessDraftFromConfig(config);
    setDraft(nextDraft);
    setArgRows(argsToRows(nextDraft.args));
    // Keep the editor open while the background poll refreshes runtime data.
    draftDirtyRef.current = true;
    setDraftDirty(true);
    setFormError(null);
    setEditingHarness(true);
  };

  const saveHarness = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setFormError(null);
    const runtimePinPath = stringValue(nestedValue(config.runtime, "node"), "path") || "";
    const program = draft.program.trim() || runtimePinPath || "node";
    const entry = draft.entry.trim() || "{release_root}/apps/cli/lib/bin.js";
    if (draft.mode === "node" && (numberValue(snapshot.health, "harness_config_wire_version") ?? 0) < 2) {
      setFormError(t("This Agent does not advertise the explicit Node Harness configuration contract. Update Agent before saving Node mode."));
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
      setFormError(t("Replace hidden arguments before saving."));
      return;
    }
    const incompleteRow = argRows.find(
      (row) => !row.key.trim() || (row.key.trim() === "--" && !row.value.trim()),
    );
    if (incompleteRow !== undefined) {
      setFormError(t("Finish or remove the empty argument row before saving."));
      return;
    }
    let argsText = rowsToArgsText(argRows);
    if (!argRows.some((row) => row.key.trim() === "--profile")) {
      argsText = argsText ? `--profile\n{profile}\n${argsText}` : "--profile\n{profile}";
    }
    if (argsText.includes("[REDACTED]")) {
      setFormError(t("Replace hidden arguments before saving."));
      return;
    }
    const harnessPayload = harnessConfigPayloadFromDraft({
      ...draft,
      mode: "node",
      program,
      entry,
      args: argsText,
    });
    harnessPayload.readiness_url = readinessUrl || null;
    harnessPayload.readiness_timeout_secs = timeout ?? null;
    const saved = await runAction(t("Save Harness configuration"), "/v1/config", {
      action: "set_harness",
      harness: harnessPayload,
      preserve_harness_readiness_url: draft.readinessUrlRedacted,
    });
    if (saved === true) {
      draftDirtyRef.current = false;
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
      setArgRows([]);
      draftDirtyRef.current = false;
      setDraftDirty(false);
      setEditingHarness(true);
      setFormError(null);
    }
  };

  const configuredMode = harnessLaunchMode(harness);
  const configuredArgs = arrayValue(harness, "args").filter((item): item is string => typeof item === "string");
  const configuredEntry = configuredMode === "node"
    ? stringValue(harness, "entry") || configuredArgs[0] || t("Not configured")
    : undefined;

  const [runtimeSetupOpen, setRuntimeSetupOpen] = useState(false);
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
  const updatesSnapshot = asObject(snapshot.updates);
  const runtimeUpdateState = stringValue(asObject(updatesSnapshot.update), "state");
  const runtimeOperationPhase = stringValue(asObject(updatesSnapshot.operation), "phase");
  const runtimeCleanupPending = booleanValue(asObject(updatesSnapshot.operation), "cleanup_pending");
  const runtimeGate = runtimeSettingsGate(harnessState, numberValue(harnessRuntime, "pid"), runtimeUpdateState, runtimeOperationPhase, runtimeCleanupPending, busyAction !== null);
  const runtimeGateReason = runtimeGate.reason === "harness_not_stopped" ? t("Harness must be positively stopped before saving runtime settings.") : runtimeGate.reason === "update_active" ? t("Wait for the update to become idle before saving runtime settings.") : runtimeGate.reason === "cold_active" ? t("Wait for the cold switch to finish before saving runtime settings.") : runtimeGate.reason === "cleanup_pending" ? t("Retry cold cleanup before saving runtime settings.") : null;
  const runtime = nestedValue(config, "runtime");
  const savedRuntimeDraft = {
    node: stringValue(nestedValue(runtime, "node"), "path") || "",
    pnpm: stringValue(nestedValue(runtime, "pnpm"), "path") || "",
    git: stringValue(nestedValue(runtime, "git"), "path") || "",
    source: stringValue(runtime, "source") || "official",
  };
  const [runtimeDraft, setRuntimeDraft] = useState(() => ({ value: savedRuntimeDraft, dirty: false }));
  const [runtimeSaving, setRuntimeSaving] = useState(false);
  const pins = runtimeDraft.value;
  const runtimeSource = runtimeDraft.value.source;
  useEffect(() => {
    setRuntimeDraft(current => refreshEditableDraft(current, savedRuntimeDraft));
  }, [savedRuntimeDraft.node, savedRuntimeDraft.pnpm, savedRuntimeDraft.git, savedRuntimeDraft.source, runtimeDraft.dirty]);
  const changeRuntime = (name: keyof typeof savedRuntimeDraft, value: string) => {
    setRuntimeDraft(current => ({ value: { ...current.value, [name]: value }, dirty: true }));
  };
  const saveRuntime = async () => {
    if (runtimeGate.disabled || runtimeSaving) return;
    setRuntimeSaving(true);
    try {
      const saved = await runAction(t("Save runtime settings"), "/v1/config", {
        action: "set_runtime", runtime: {
          node: pins.node.trim() ? { path: pins.node.trim(), ownership: "system" } : null,
          pnpm: pins.pnpm.trim() ? { path: pins.pnpm.trim(), ownership: "system" } : null,
          git: pins.git.trim() ? { path: pins.git.trim(), ownership: "system" } : null,
          source: runtimeSource, mode: stringValue(runtime, "mode") || "portable",
        },
      });
      setRuntimeDraft(current => finishDraftSave(current, saved === true));
    } finally { setRuntimeSaving(false); }
  };
  return <>
    <PageIntro kicker={t("System / Settings")} title={t("Settings")} detail={t("Configuration remains Agent-owned. This view intentionally exposes metadata, not credentials or raw environment values.")} />
    <Panel title={t("Release identity")} icon={<Info size={18} />}><dl className="detail-list">
      <div><dt>{t("Version")}</dt><dd>{stringValue(buildIdentity, "version") || t("Not available")}</dd></div>
      <div><dt>{t("Build")}</dt><dd><code>{stringValue(buildIdentity, "buildId") || t("Not available")}</code></dd></div>
      <div><dt>{t("Bundled runtime")}</dt><dd>Node {stringValue(buildIdentity, "node") || "—"} / npm {stringValue(buildIdentity, "npm") || "—"} / pnpm {stringValue(buildIdentity, "pnpm") || "—"}</dd></div>
    </dl></Panel>
    <LaunchInputsPanel snapshot={snapshot} />
    <HarnessPreferencesPanel snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
    <div className="grid-two">
<Panel title={t("Runtime settings")} icon={<Cpu size={18} />}><div className="status-block"><div className="button-row"><ActionButton onClick={() => setRuntimeSetupOpen((open) => !open)}>{runtimeSetupOpen ? t("Hide dependency registry") : t("Change dependency registry")}</ActionButton></div>{runtimeSetupOpen && <div className="form-grid"><label className="form-field"><span className="field-label">{t("Dependency registry")}</span><select className="form-input" value={runtimeSource} disabled={runtimeSaving} onChange={(e) => changeRuntime("source", e.target.value)}><option value="official">{t("Official")}</option><option value="npmmirror">npmmirror</option></select></label></div>}{runtimeSetupOpen && <p className="field-help">{t("Only used when Harness dependencies are downloaded. Nexus never downloads Node, pnpm, or Git.")}</p>}<div className="form-grid">{(["node", "pnpm", "git"] as const).map((name) => <label key={name} className="form-field"><span className="field-label">{name} {t("pin")}</span><input className="form-input" value={pins[name]} disabled={runtimeSaving} placeholder={stringValue(nestedValue(runtime, name), "path") || t("Leave blank for automatic discovery")} onChange={(event) => changeRuntime(name, event.target.value)} /></label>)}</div><p className="field-help">{t("Explicit paths take priority. Leave blank to use the complete bundled Node/npm/pnpm combination; system discovery is used only when no bundle is present.")}</p><ActionButton disabled={runtimeGate.disabled || runtimeSaving} onClick={() => void saveRuntime()}>{t("Save runtime settings")}</ActionButton>{runtimeDraft.dirty && <ActionButton disabled={runtimeSaving} onClick={() => setRuntimeDraft({ value: savedRuntimeDraft, dirty: false })}>{t("Cancel")}</ActionButton>}{runtimeGateReason && <p className="field-help" role="status">{runtimeGateReason}</p>}</div><hr className="panel-divider" /><RuntimeStatusPanel agentAvailable={snapshot.startup?.available === true} state={runtimeStatus} onCheck={() => void checkRuntime()} /></Panel>
      <Panel title={t("Repair & reset")} icon={<Gear size={18} />}><p className="field-help">{t("Reset repairs broken Nexus state. Harness data under .dsh is never touched; installed version slots stay on disk.")}</p><div className="button-row">
          <ActionButton disabled={runtimeGate.disabled || snapshot.startup?.available !== true || draftDirty || runtimeDraft.dirty} onClick={() => { if (window.confirm(t("Restore the previous valid Nexus configuration? Harness will stay stopped."))) void runAction(t("Restore previous configuration"), "/v1/maintenance", { action: "restore_previous", scope: "config" }); }}>{t("Restore previous configuration")}</ActionButton>
          <ActionButton tone={armedReset === "config" ? "danger" : undefined} disabled={busyAction !== null} onClick={() => { const scope = "config"; if (armedReset === scope) { setArmedReset(null); void runAction(t("Reset Nexus configuration"), "/v1/maintenance", { action: "reset", scope }); } else { setArmedReset(scope); } }}>{armedReset === "config" ? t("Click again to confirm") : t("Reset Nexus configuration")}</ActionButton>
          <ActionButton tone={armedReset === "slots" ? "danger" : undefined} disabled={busyAction !== null} onClick={() => { const scope = "slots"; if (armedReset === scope) { setArmedReset(null); void runAction(t("Reset configuration and slot registry"), "/v1/maintenance", { action: "reset", scope }); } else { setArmedReset(scope); } }}>{armedReset === "slots" ? t("Click again to confirm") : t("Reset configuration and slot registry")}</ActionButton>
        </div><p className="field-help">{t("Restores the previous valid Nexus settings without starting Harness or moving data. It cannot repair an unreadable configuration whose data paths cannot be verified.")}</p><p className="field-help">{t("Reset backups contain original private configuration. Keep them local; use diagnostic export for a redacted bundle to share.")}</p>{armedReset && <p className="form-error" role="alert">{t("Click the same button again to run the reset. Harness must be stopped.")}</p>}</Panel>
      <Panel title={t("Help")} icon={<TerminalWindow size={18} />}><div className="integration-list">
          <div><CheckCircle size={18} /><span>{t("Upstream documentation")}</span><a href="https://github.com/deepseek-ai/deepseek-harness" target="_blank" rel="noreferrer">github.com/deepseek-ai/deepseek-harness</a></div>
          <div><CheckCircle size={18} /><span>{t("Diagnostics and logs")}</span><span>{t("Runtime logs and diagnostic bundles are collected on the Diagnostics page.")}</span></div>
          <div><Gear size={18} /><span>{t("Agent log level")}</span><select className="form-input" value={logLevel} onChange={(event) => setLogLevel(event.target.value)}><option value="error">error</option><option value="warn">warn</option><option value="info">info</option><option value="debug">debug</option><option value="trace">trace</option></select></div>
        </div><p className="field-help">{t("The log level applies the next time the Agent starts.")}</p>
        <details><summary>{t("Harness fails to start")}</summary><p className="field-help">{t("Open the startup log from the Overview or Diagnostics page. Plugin mismatches are expected across versions; use Recovery to remove the affected plugin or restore a healthy snapshot.")}</p></details>
        <details><summary>{t("Dependency download is slow or times out")}</summary><p className="field-help">{t("Switch the dependency registry to npmmirror in Settings, then retry the install.")}</p></details>
        <details><summary>{t("Node, pnpm, or Git is missing")}</summary><p className="field-help">{t("Nexus defaults to its complete bundled runtime. Explicit paths in Runtime settings take priority; system discovery is only used without a bundle.")}</p></details>
      </Panel>
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
          <dl className="detail-list"><div><dt>{t("Node executable")}</dt><dd>{stringValue(harness, "program") || t("Not configured")}</dd></div><div><dt>{t("Harness entry")}</dt><dd>{configuredEntry}</dd></div><div><dt>{t("Readiness URL")}</dt><dd>{isLoopbackReadinessTarget(stringValue(harness, "readiness_url")) ? stringValue(harness, "readiness_url") : t("Not shown")}</dd></div></dl>
          <div className="form-actions"><button type="button" className="button" disabled={configControlsDisabled} onClick={openEditor}>{t("Edit configuration")}</button><button type="button" className="button danger" disabled={configControlsDisabled} onClick={() => void clearHarness()}>{t("Clear configuration")}</button></div>
        </> : <form className="config-form" onSubmit={(event) => void saveHarness(event)}>
          <p className="field-help">{t("Harness always runs in Node mode from the active release slot. The executable, entry, and profile wiring are managed by Nexus.")}</p>
          <div className="form-grid">
            <label className="form-field"><span className="field-label">{t("Readiness timeout (seconds)")} <em>{t("Optional")}</em></span><input className="form-input" inputMode="numeric" value={draft.timeout} onChange={(event) => updateDraft("timeout", event.target.value)} placeholder={t("Agent default")} disabled={configControlsDisabled} /></label>
            <label className="form-field full"><span className="field-label">{t("Readiness URL")} <em>{t("Optional")}</em></span><input className="form-input" type="text" value={draft.readinessUrl} onChange={(event) => updateDraft("readinessUrl", event.target.value)} placeholder={t("Readiness URL example")} disabled={configControlsDisabled} /><span className="field-help">{t("Use an HTTP loopback URL for a 2xx check, or tcp://127.0.0.1:PORT when the Harness protects its page with authentication.")}</span></label>
            <label className="form-check full"><input type="checkbox" checked={draft.readinessTokenRequired} onChange={(event) => updateDraft("readinessTokenRequired", event.target.checked)} disabled={configControlsDisabled || !draft.readinessUrl.trim()} /><span>{t("Require a fresh Harness token before accepting readiness")}</span></label>
          </div>
          <div className="form-field full"><span className="field-label">{t("Additional arguments")}</span>
            <p className="field-help">{t("The profile argument always follows the active profile and is added automatically.")}</p>
            {(!draft.argsRedacted || draft.replaceRedactedArgs) && <>{argRows.map((row, index) => <div className="kv-row" key={index}><input className="form-input" value={row.key} placeholder="--flag" disabled={configControlsDisabled} onChange={(event) => setArgRows((current) => current.map((item, i) => i === index ? { ...item, key: event.target.value } : item))} /><input className="form-input" value={row.value} placeholder={t("Value (optional)")} disabled={configControlsDisabled} onChange={(event) => setArgRows((current) => current.map((item, i) => i === index ? { ...item, value: event.target.value } : item))} /><ActionButton tone="danger" disabled={configControlsDisabled} onClick={() => setArgRows((current) => current.filter((_, i) => i !== index))}>{t("Remove")}</ActionButton></div>)}</>}
            {draft.argsRedacted && !draft.replaceRedactedArgs && <p className="form-error" role="alert"><WarningCircle size={15}/>{t("Existing sensitive arguments are hidden. Enable replacement before saving.")}</p>}
            {draft.argsRedacted && draft.replaceRedactedArgs && <p className="field-help">{t("Re-enter the complete argument list. Previous arguments that are not entered again will be removed.")}</p>}
            {draft.argsRedacted && <label className="form-check"><input type="checkbox" checked={draft.replaceRedactedArgs} onChange={(event) => { updateDraft("replaceRedactedArgs", event.target.checked); setArgRows(current => replacementArgumentRows(current, event.target.checked)); }} disabled={configControlsDisabled} /><span>{t("Replace hidden arguments")}</span></label>}
            {(!draft.argsRedacted || draft.replaceRedactedArgs) && <div className="button-row"><ActionButton disabled={configControlsDisabled} onClick={() => setArgRows((current) => [...current, { key: "--", value: "" }])}>{t("Add argument")}</ActionButton></div>}
          </div>
          {formError && <div className="form-error" role="alert"><WarningCircle size={16} />{formError}</div>}
          {harnessState === "running" && <p className="field-help" role="status">{t("Stop Harness before changing its launch configuration.")}</p>}
          <div className="form-actions"><button type="submit" className="button primary" disabled={configControlsDisabled}>{t("Save configuration")}</button>{hasHarnessConfig && <button type="button" className="button" disabled={configControlsDisabled} onClick={() => { draftDirtyRef.current = false; setDraftDirty(false); setFormError(null); setEditingHarness(false); }}>{t("Cancel")}</button>}{hasHarnessConfig && <button type="button" className="button danger" disabled={configControlsDisabled} onClick={() => void clearHarness()}>{t("Clear configuration")}</button>}</div>
        </form>}
        {!hasHarnessConfig && !editingHarness && <EmptyState title={t("Harness is not configured")} detail={t("The Agent remains usable as a control plane until an external Harness is configured.")} />}
      </Panel>
     </div>
     <Panel title={t("Display and window behavior")} icon={<MonitorPlay size={18} />}><label className="form-field">{t("Page zoom")}<select value={zoom} onChange={event => setDisplayZoom(Number(event.target.value))}>{ZOOM_LEVELS.map(value => <option key={value} value={value}>{value}%</option>)}</select></label><p>{t("Use Ctrl + / Ctrl - to zoom and Ctrl 0 to reset. Returning to this window refreshes service status.")}</p><p>{t("Closing the window keeps Nexus in the tray. The tray menu lets you exit the launcher while keeping services running, or stop services and exit.")}</p></Panel><Panel title={t("Native integration")} icon={<Bell size={18} />}><div className="integration-list"><div><CheckCircle size={18} /><span>{t("Single instance guard")}</span><strong>{t("Enabled")}</strong></div><div><Bell size={18} /><span>{t("Desktop notifications")}</span><label className="form-check"><input type="checkbox" checked={notificationsEnabled} onChange={(event) => { setNotificationsEnabledPreference(event.target.checked); setNotificationsEnabled(event.target.checked); }} /><span>{t("Enabled")}</span></label></div><div><Key size={18} /><span>{t("API transport")}</span><strong>{t("Rust loopback proxy")}</strong></div><div><CheckCircle size={18} /><span>{t("Launch on system startup")}</span><label className="form-check"><input type="checkbox" checked={autostartEnabled === true} disabled={autostartEnabled === null} onChange={(event) => void toggleAutostart(event.target.checked)} /><span>{autostartEnabled === null ? t("Unavailable") : autostartEnabled ? t("Enabled") : t("Disabled")}</span></label></div></div></Panel>
  </>;
}

function argsToRows(argsText: string): Array<{ key: string; value: string }> {
  const tokens = argsText.split(/\r?\n/).map((value) => value.trim()).filter(Boolean);
  const rows: Array<{ key: string; value: string }> = [];
  for (let index = 0; index < tokens.length; index += 1) {
    if (tokens[index].startsWith("--") && index + 1 < tokens.length && !tokens[index + 1].startsWith("--")) {
      rows.push({ key: tokens[index], value: tokens[index + 1] });
      index += 1;
    } else {
      rows.push({ key: tokens[index], value: "" });
    }
  }
  return rows;
}

function rowsToArgsText(rows: Array<{ key: string; value: string }>): string {
  const out: string[] = [];
  for (const row of rows) {
    const key = row.key.trim();
    if (!key) continue;
    out.push(key);
    const value = row.value.trim();
    if (value) out.push(value);
  }
  return out.join("\n");
}

function PageIntro({ kicker, title, detail }: { kicker: string; title: string; detail: string }) {
  return <div className="page-heading"><div><span className="kicker">{kicker}</span><h1>{title}</h1><p>{detail}</p></div></div>;
}

function Panel({ title, icon, children }: { title: string; icon: React.ReactNode; children: React.ReactNode }) {
  return <section className="panel"><div className="panel-header"><div className="panel-title">{icon}<h2>{title}</h2></div></div>{children}</section>;
}

export default App;
