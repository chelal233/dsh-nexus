import { type Locale, type Translator } from "./i18n";
import { apiErrorInfo, errorWithExplanation, recoverableNoop } from "./api-errors";
import { type JsonObject, type RuntimeToolSource } from "./app-types";
import { stringValue } from "./json-values";

export function formatTimestamp(value: unknown, unavailable: string, locale: Locale): string {
  if (typeof value !== "number" || value <= 0) return unavailable;
  return new Date(value * 1000).toLocaleString(locale === "zh" ? "zh-CN" : "en-US");
}

export function errorMessage(error: unknown): string {
  return apiErrorInfo(error).message;
}

export function localizeBackendError(message: string, t: Translator): string {
  return errorWithExplanation(message, explainBackendError(message, t), t("Original error"));
}

export function explainBackendError(message: string, t: Translator): string {
  const normalized = message.toLowerCase();
  if (normalized.includes("external source protection history is full (1 mib)")) {
    return t(
      "External source protection history is full. Keep the current source or choose a previously confirmed directory, then save again. Existing directory protection is retained.",
    );
  }
  if (
    normalized.includes("gyp err! find python") ||
    normalized.includes("could not find any visual studio installation")
  ) {
    return t(
      "A Harness dependency requires native build tools. Keep the original error below when seeking upstream help, or choose another Harness version.",
    );
  }
  // Characteristic failure signatures map to plain-language causes with the
  // recovery action that actually applies; anything else falls through to
  // the backend's own message.
  if (
    normalized.includes("plugin tree failed to load") ||
    normalized.includes("loader entries failed to apply")
  ) {
    return t(
      "Plugins failed to load, likely a version mismatch between installed plugins and this Harness build. Open Recovery to remove the affected plugins or restore a healthy snapshot.",
    );
  }
  if (normalized.includes("does not provide an export named")) {
    return t(
      "A plugin expects module APIs this Harness build does not have: the installed plugin set and the Harness version are out of sync. Restore a healthy snapshot or update the plugins.",
    );
  }
  if (normalized.includes("duplicate loader entry")) {
    return t(
      "A profile plugin duplicates a plugin this Harness now ships built-in. Remove the older copy from the profile's plugin inventory.",
    );
  }
  if (normalized.includes("release slots are full")) {
    return t("All release slots are full. Remove a slot you no longer need, then try again.");
  }
  if (
    normalized.includes("release_slot_protected") ||
    normalized.includes("is the current slot") ||
    normalized.includes("is the last-known-good slot")
  ) {
    return t(
      "That slot is still in use (current or last-known-good). Switch to another version first.",
    );
  }
  if (normalized.includes("snapshot_release_not_installed")) {
    return t(
      "This snapshot's Harness version is not installed. Cold-switch to that tag first, then restore.",
    );
  }
  if (normalized.includes("switch the dependency registry to npmmirror")) {
    return t(
      "Upstream dependency installation failed, usually a network issue. Switch the dependency registry to npmmirror in Settings, then retry.",
    );
  }
  const fallbacks: Array<[RegExp, string]> = [
    [
      /refused to connect|connection refused|econnrefused/,
      t(
        "The local service is not responding. Retry, and check the Agent status on the Overview page.",
      ),
    ],
    [
      /os error 5|access is denied/,
      t(
        "Access denied: the file may be locked by another process. Close programs using it and retry.",
      ),
    ],
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
    return t(
      "Harness is detached. Configure it in Settings, then start it from the control panel.",
    );
  }
  if (normalized.includes("harness is already running")) {
    return t("Harness is already running; no lifecycle change was made.");
  }
  if (normalized.includes("harness is running but is not attached")) {
    return t("Harness is running outside this Agent. Its status is read-only until it reconnects.");
  }
  if (normalized.includes("harness log session marker is not available")) {
    return t(
      "Harness log session is unavailable. Restart Harness to establish a safe token boundary.",
    );
  }
  if (normalized.includes("harness log session marker is invalid")) {
    return t("Harness log session is invalid. Restart Harness to establish a safe token boundary.");
  }
  if (normalized.includes("current harness authentication token not found")) {
    return t("No current Harness authentication token was found for this run.");
  }
  if (normalized.includes("harness changed state while its token was being observed")) {
    return t(
      "Harness changed state while its token was being observed. Refresh after it is running.",
    );
  }
  if (normalized.includes("harness node launch mode requires a node runtime program")) {
    return t("Node mode requires a Node runtime executable.");
  }
  if (
    normalized.includes("harness node launch mode requires an entry script") ||
    normalized.includes("harness node entry must be non-empty")
  ) {
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
  if (
    normalized.includes("readiness url has an invalid") ||
    normalized.includes("readiness url port must be")
  ) {
    return t("Readiness target has an invalid port or host.");
  }
  if (normalized.includes("readiness url must target localhost")) {
    return t("Readiness target must use a loopback host.");
  }
  if (normalized.includes("harness token-bound readiness requires a readiness url")) {
    return t("Token-bound readiness requires a readiness URL.");
  }
  if (
    normalized.includes("agent is not responding at") ||
    normalized.includes("agent api is not responding")
  ) {
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

export function localizedRuntimeState(value: unknown, t: Translator): string {
  const state = typeof value === "string" ? value.toLowerCase() : "";
  switch (state) {
    case "ok":
      return t("Healthy");
    case "running":
      return t("Running");
    case "starting":
      return t("Starting");
    case "stopping":
      return t("Stopping");
    case "shutting_down":
      return t("Shutting down");
    case "stopped":
      return t("Stopped");
    case "failed":
      return t("Failed");
    case "passed":
      return t("Passed");
    case "inconclusive":
      return t("Inconclusive");
    case "succeeded":
      return t("Succeeded");
    case "prepared":
      return t("Prepared; awaiting manual confirmation");
    case "completed":
      return t("Completed");
    case "interrupted":
      return t("Interrupted");
    case "deleting":
      return t("Deleting");
    case "removed":
      return t("Removed");
    case "present":
      return t("Present");
    case "missing":
      return t("Missing");
    case "omitted":
      return t("Omitted");
    case "applying":
      return t("Applying");
    case "detached":
      return t("Detached");
    case "idle":
      return t("idle");
    case "ready":
      return t("Ready");
    case "pending":
      return t("Pending");
    case "queued":
      return t("Queued");
    case "cloning":
      return t("Cloning");
    case "planning":
      return t("Planning");
    case "awaiting_confirmation":
      return t("Awaiting confirmation");
    case "supplying":
      return t("Supplying runtimes");
    case "installing":
      return t("Installing");
    case "building":
      return t("Building");
    case "verifying":
      return t("Verifying");
    case "registering":
      return t("Registering");
    case "promoting":
      return t("Promoting");
    case "cancelling":
      return t("Cancelling");
    case "cancelled":
      return t("Cancelled");
    case "manual":
      return t("Manual");
    case "healthy":
      return t("Healthy");
    case "legacy_metadata_only":
      return t("Legacy metadata only");
    case "materialization_pending":
      return t("Materialization pending");
    case "applied":
      return t("Applied");
    case "committed":
      return t("Committed");
    case "rolled_back":
      return t("Rolled back");
    case "registered":
      return t("Registered");
    case "available":
      return t("Available");
    default:
      return typeof value === "string" && value
        ? t("Unknown state: {state}", { state: value })
        : t("Unknown");
  }
}

export function preflightReasonLabel(entry: JsonObject, t: Translator): string {
  const reason = stringValue(entry, "reason") || "";
  const id = stringValue(entry, "id"),
    status = stringValue(entry, "status");
  // Preserve filesystem/OS errors verbatim. Only known successful observation
  // templates separate a user path/profile name from Nexus-owned wording.
  if (id === "home" && status === "ok") {
    for (const suffix of [
      "read/write access verified",
      "parent access verified; directories will be created at startup",
    ]) {
      if (reason.endsWith(` · ${suffix}`)) return `${reason.slice(0, -suffix.length)}${t(suffix)}`;
    }
  }
  if (id === "profile" && status !== "blocked") {
    for (const suffix of [
      ": initialized",
      ": not initialized; the selected Harness must provide its built-in profile.",
    ]) {
      if (reason.endsWith(suffix))
        return `${reason.slice(0, -suffix.length)}: ${t(suffix.slice(2))}`;
    }
  }
  if (id === "port" && status === "ok" && reason.endsWith(" is currently available")) {
    return t("{address} is currently available", {
      address: reason.slice(0, -" is currently available".length),
    });
  }
  if (["node", "npm", "pnpm"].includes(id || "")) {
    if (status === "ok") {
      const source = reason.match(/^(.* · )(system|nexus|bundled)$/);
      if (source) return source[1] + runtimeToolSourceLabel(source[2] as RuntimeToolSource, t);
    } else if (/^[a-z_]+$/.test(reason)) return runtimeToolReason(reason, t);
  }
  return t(reason);
}

export function snapshotContentNote(value: unknown, t: Translator): string {
  if (typeof value !== "string" || !value)
    return t("Content truncated by the Agent response limit.");
  const limit = value.match(
    /^content is truncated by the (\d+) byte per-file and (\d+) byte response limits$/,
  );
  return limit
    ? t("Content truncated at {file} bytes per file and {response} bytes per response.", {
        file: limit[1],
        response: limit[2],
      })
    : value;
}

export function harnessOptionLabel(value: string, t: Translator): string {
  switch (value) {
    case "true":
      return t("Enabled");
    case "false":
      return t("Disabled");
    case "read-only":
      return t("Read only (read-only)");
    case "workspace-write":
      return t("Workspace write (workspace-write)");
    case "danger-full-access":
      return t("Full access (danger-full-access)");
    case "native":
      return t("Native tools (native)");
    case "ptc":
      return t("PTC tools (ptc)");
    case "both":
      return t("Native and PTC tools (both)");
    default:
      return value;
  }
}

export function launchInputValueLabel(row: JsonObject, t: Translator): string {
  const name = stringValue(row, "name"),
    value = stringValue(row, "value");
  if (!value) return t("Not available");
  // User names and filesystem paths must never be looked up in the UI dictionary.
  if (["Program", "Harness data directory", "Profile", "Release directory"].includes(name || ""))
    return value;
  if (name === "Working directory")
    return value === "Inherited process directory" ? t("Inherited process directory") : value;
  if (name === "Verified Harness version")
    return value === "Not verified" ? t("Not verified") : value;
  if (["Open browser", "Disable telemetry", "Tools mode", "Permission mode"].includes(name || "")) {
    return value === "Resolved by Harness; not inspected"
      ? t("Resolved by Harness; not inspected")
      : harnessOptionLabel(value, t);
  }
  if (name === "Port")
    return value === "Automatic port (0)"
      ? t("Automatic port (0)")
      : value === "Resolved by Harness; not inspected"
        ? t("Resolved by Harness; not inspected")
        : value;
  return value;
}

export function updateStateLabel(update: JsonObject, t: Translator): string {
  const state = stringValue(update, "state");
  return state ? localizedRuntimeState(state, t) : t("Update queue idle");
}

export function isRecoverableNoopError(error: unknown): boolean {
  return recoverableNoop(error);
}

export function compactError(value: string): string {
  return value.length > 180 ? `${value.slice(0, 177)}...` : value;
}

export function runtimeToolSourceLabel(source: RuntimeToolSource, t: Translator): string {
  return source === "system"
    ? t("System source")
    : source === "bundled"
      ? t("Bundled")
      : t("Nexus source");
}

export function runtimeToolReason(reason: string | undefined, t: Translator): string {
  switch (reason) {
    case "not_found":
      return t("Runtime tool was not found. Install it or configure its path, then retry.");
    case "corepack_shim_unverified":
      return t("Corepack shim could not be verified; pnpm status cannot be confirmed.");
    default:
      return reason
        ? `${t("This runtime could not be verified.")} (${reason})`
        : t("This runtime could not be verified.");
  }
}
