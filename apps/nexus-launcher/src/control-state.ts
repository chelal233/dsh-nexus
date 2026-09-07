export type HarnessControlGate = {
  controlsDisabled: boolean;
  externallyManaged: boolean;
};

export function validStartupCheck(value: unknown): boolean {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const report = value as Record<string, unknown>;
  if (report.api_version !== "v1" || typeof report.ready !== "boolean" || typeof report.paused !== "boolean"
      || !Number.isSafeInteger(report.checked_at_unix) || Number(report.checked_at_unix) < 0
      || !Array.isArray(report.checks) || report.checks.length === 0 || report.checks.length > 64) return false;
  if (!report.checks.every(entry => entry && typeof entry === "object" && !Array.isArray(entry)
      && typeof entry.id === "string" && entry.id.length > 0
      && ["ok", "warning", "blocked"].includes(entry.status)
      && typeof entry.reason === "string" && typeof entry.next === "string")) return false;
  return report.ready === !report.checks.some(entry => entry.status === "blocked");
}

export function invalidatesHarnessCredentials(path: string, action: unknown): boolean {
  if (path === "/v1/recovery" && action === "enter") return true;
  if (path === "/v1/profiles" && action === "select") return true;
  if (path === "/v1/releases" && (action === "promote" || action === "rollback")) return true;
  if (path === "/v1/updates" && (action === "switch" || action === "confirm" || action === "offline_import")) return true;
  if (action !== "start" && action !== "stop" && action !== "restart") return false;
  return path === "/v1/agent" || path === "/v1/harness";
}

export function isLifecycleBusyError(message: string): boolean {
  return /(?:^|: )NEXUS_LIFECYCLE_BUSY:/.test(message);
}

export function isMissingHarnessError(message: string): boolean {
  return /harness_not_configured|Harness is not configured|no verified DSH release is selected|no current release (?:is )?selected/i.test(message);
}

export function needsHarnessInstall(config: Record<string, unknown> | null, releases: Record<string, unknown> | null): boolean {
  if (!config || !releases || !Array.isArray(releases.releases)) return false;
  const document = (config.config ?? config) as Record<string, unknown>;
  if (config.harness_env_override === true || document.harness_env_override === true) return false;
  const harness = document.harness as Record<string, unknown> | undefined;
  return !harness?.program && releases.releases.length === 0;
}

/** Keep only non-credential catalog data from the same verified Agent. */
export function lifecycleBusySnapshot<T extends {
  profiles: unknown; releases: unknown; state: unknown;
  harnessRuntime: unknown; harnessUi: unknown; recovery: unknown;
}>(next: T, previous: T, sameOwner: boolean): T & { lifecycleBusy: boolean } {
  return {
    ...next, lifecycleBusy: true,
    profiles: sameOwner ? previous.profiles : null,
    releases: sameOwner ? previous.releases : null,
    state: null, harnessRuntime: null, harnessUi: null, recovery: null,
  };
}

export function harnessControlGate(
  state: string | undefined,
  pid: number | undefined,
  busy: boolean,
  bridgeAvailable: boolean,
): HarnessControlGate {
  const controllableState =
    state === "detached" ||
    state === "stopped" ||
    state === "failed" ||
    state === "running";
  // A PID-less running Harness can be a descendant that survived its
  // bootstrap parent or an externally restarted instance. It remains
  // observable, but lifecycle operations must stay read-only until this Agent
  // has a child handle again.
  const pidlessActive = state === "starting" && pid === undefined;
  const externallyManaged = state === "running" && pid === undefined;
  return {
    controlsDisabled:
      busy || !bridgeAvailable || !controllableState || pidlessActive || externallyManaged,
    externallyManaged,
  };
}

export function failClosedSnapshot<T extends { startup: unknown }>(empty: T): T {
  return { ...empty, startup: null };
}

export function launcherContentMode(
  error: string | null,
  loading: boolean,
  hasStatus: boolean,
): "error" | "loading" | "content" {
  if (error) return "error";
  if (loading && !hasStatus) return "loading";
  return "content";
}

export type LatestRequest = {
  begin: () => number;
  isCurrent: (token: number) => boolean;
  cancel: () => void;
};

/** Prevents a completed older request from replacing newer refresh/action state. */
export function createLatestRequest(): LatestRequest {
  let generation = 0;
  return {
    begin: () => ++generation,
    isCurrent: (token) => token === generation,
    cancel: () => { generation += 1; },
  };
}

/** Seed from history, then notify once per failure even across busy/empty refreshes. */
export function createFailureNoticeTracker() {
  let initialized = false;
  const seen = new Set<string>();
  return {
    observe(keys: string[]): boolean {
      const fresh = initialized && keys.some(key => !seen.has(key));
      keys.forEach(key => seen.add(key));
      initialized = true;
      return fresh;
    },
  };
}

export function coldOperationIsTerminal(phase: unknown): boolean {
  return phase === "succeeded" || phase === "cancelled" || phase === "failed";
}

export function actionNoticeKey(path: string, action: unknown): string {
  if (path === "/v1/updates" && (action === "offline_import" || action === "offline_export")) return "Offline package request accepted. Follow the current stage to confirm completion.";
  if (path === "/v1/updates" && (action === "switch" || action === "confirm")) {
    return "Installation request accepted. Follow the current stage below to confirm completion.";
  }
  if (path === "/v1/updates" && action === "cancel") return "Cancellation requested. Waiting for cleanup to finish.";
  return "complete";
}

export function recoveryMutationGate(
  harnessStopRequired: boolean,
  harnessState: unknown,
  busy: boolean,
): { disabled: boolean; reason: "busy" | "stop_required" | "not_stopped" | null } {
  if (busy) return { disabled: true, reason: "busy" };
  if (harnessStopRequired) return { disabled: true, reason: "stop_required" };
  if (harnessState !== "stopped" && harnessState !== "detached" && harnessState !== "failed") {
    return { disabled: true, reason: "not_stopped" };
  }
  return { disabled: false, reason: null };
}

export type RuntimeSettingsGate = {
  disabled: boolean;
  reason: "busy" | "harness_not_stopped" | "update_active" | "cold_active" | "cleanup_pending" | null;
};

/** Mirrors the backend prerequisites while keeping the backend authoritative. */
export function runtimeSettingsGate(
  harnessState: unknown,
  harnessPid: number | undefined,
  updateState: unknown,
  coldPhase: unknown,
  cleanupPending: boolean,
  busy: boolean,
): RuntimeSettingsGate {
  if (busy) return { disabled: true, reason: "busy" };
  if (!["stopped", "detached", "failed"].includes(String(harnessState)) || harnessPid !== undefined) {
    return { disabled: true, reason: "harness_not_stopped" };
  }
  if (updateState === "running") return { disabled: true, reason: "update_active" };
  if (coldPhase !== undefined && coldPhase !== null && !coldOperationIsTerminal(coldPhase)) {
    return { disabled: true, reason: "cold_active" };
  }
  if (cleanupPending) return { disabled: true, reason: "cleanup_pending" };
  return { disabled: false, reason: null };
}


export const FIXED_PROFILE_PLUGINS = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"];

/** Dropping on a later row moves after it; on an earlier row moves before it. */
export function pluginMoveTarget(order: string[], source: string, destination: string): { target: string | null } | null {
  const from = order.indexOf(source), to = order.indexOf(destination);
  if (from < 0 || to < 0 || from === to || FIXED_PROFILE_PLUGINS.includes(source) || FIXED_PROFILE_PLUGINS.includes(destination)) return null;
  return { target: from < to ? order[to + 1] ?? null : destination };
}
