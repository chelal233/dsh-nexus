export type HarnessControlGate = {
  controlsDisabled: boolean;
  externallyManaged: boolean;
};

export function pluginPolicyVerified(profiles: Record<string, unknown>): boolean {
  const report = profiles.compatibility as Record<string, unknown> | undefined;
  const checked = report?.checked_disabled_plugins;
  const policy = Object.hasOwn(profiles, "disabled_plugins") ? profiles.disabled_plugins : [];
  if (
    profiles.api_version !== "v1" ||
    !Array.isArray(checked) ||
    !Array.isArray(policy) ||
    !checked.every((value) => typeof value === "string") ||
    !policy.every((value) => typeof value === "string")
  )
    return false;
  const expected = new Set(checked),
    current = new Set(policy);
  return expected.size === current.size && [...current].every((value) => expected.has(value));
}

/** The server exposes one source profile's policy, never a per-profile map. */
export function pluginIsolationChoice(
  profiles: Record<string, unknown>,
  profile: string,
  packageName: string,
  blocked: boolean,
) {
  const manifests = Array.isArray(profiles.manifests)
    ? (profiles.manifests as Record<string, unknown>[])
    : [];
  const selected = manifests.find((item) => item?.name === profiles.active_profile);
  const displayed = manifests.find((item) => item?.name === profile);
  const report =
    profiles.compatibility && typeof profiles.compatibility === "object"
      ? (profiles.compatibility as Record<string, unknown>)
      : {};
  const selectedSource = selected?.source_profile || profiles.active_profile;
  const policySource = report.source_profile || profiles.active_profile;
  // v1 omits this field when the saved policy is empty. Explicit malformed
  // values remain unknown; only an absent field receives the wire default.
  const policy = Object.hasOwn(profiles, "disabled_plugins") ? profiles.disabled_plugins : [];
  const eligible =
    !!packageName &&
    !packageName.startsWith("@deepseek-ai/") &&
    Array.isArray(displayed?.bundles) &&
    (displayed.bundles.includes(packageName) ||
      (Array.isArray(policy) && policy.includes(packageName)));
  const known =
    profiles.api_version === "v1" &&
    eligible &&
    !!selected &&
    !displayed?.source_profile &&
    profile === selectedSource &&
    profile === policySource &&
    Array.isArray(policy) &&
    policy.every((item) => typeof item === "string");
  const disabled = known ? (policy as string[]).includes(packageName) : null;
  const command =
    known && !blocked
      ? {
          action: disabled ? "plugin_enable" : "plugin_disable",
          profile,
          package: packageName,
        }
      : null;
  return { eligible, known, disabled, command };
}

export function validStartupCheck(value: unknown): boolean {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const report = value as Record<string, unknown>;
  if (
    report.api_version !== "v1" ||
    typeof report.ready !== "boolean" ||
    typeof report.paused !== "boolean" ||
    !Number.isSafeInteger(report.checked_at_unix) ||
    Number(report.checked_at_unix) < 0 ||
    !Array.isArray(report.checks) ||
    report.checks.length === 0 ||
    report.checks.length > 64
  )
    return false;
  if (
    !report.checks.every(
      (entry) =>
        entry &&
        typeof entry === "object" &&
        !Array.isArray(entry) &&
        typeof entry.id === "string" &&
        entry.id.length > 0 &&
        ["ok", "warning", "blocked"].includes(entry.status) &&
        typeof entry.reason === "string" &&
        typeof entry.next === "string",
    )
  )
    return false;
  return report.ready === !report.checks.some((entry) => entry.status === "blocked");
}

export function invalidatesHarnessCredentials(path: string, action: unknown): boolean {
  if (path === "/v1/recovery" && action === "enter") return true;
  if (path === "/v1/profiles" && action === "select") return true;
  if (path === "/v1/releases" && (action === "promote" || action === "rollback")) return true;
  if (
    path === "/v1/updates" &&
    (action === "switch" || action === "confirm" || action === "offline_import")
  )
    return true;
  if (action !== "start" && action !== "stop" && action !== "restart") return false;
  return path === "/v1/agent" || path === "/v1/harness";
}

export function isLifecycleBusyError(message: string): boolean {
  return /(?:^|: )NEXUS_LIFECYCLE_BUSY:/.test(message);
}

export function isMissingHarnessError(message: string): boolean {
  return /harness_not_configured|Harness is not configured|no verified DSH release is selected|no current release (?:is )?selected/i.test(
    message,
  );
}

export function externalHarnessRoot(config: Record<string, unknown> | null): string | undefined {
  const document = (config?.config ?? config ?? {}) as Record<string, unknown>;
  const source = (config?.external_harness ?? document.external_harness) as
    Record<string, unknown> | undefined;
  return typeof source?.root === "string" && source.root.length > 0 ? source.root : undefined;
}
export function hasHarnessSource(
  config: Record<string, unknown> | null,
  releases: Record<string, unknown> | null,
): boolean {
  return (
    !!externalHarnessRoot(config) ||
    (typeof releases?.current_release === "string" && releases.current_release.length > 0)
  );
}
export function startupRepairTarget(id: string): {
  module: "guide" | "profiles" | "settings" | "maintenance";
  section?: string;
} {
  if (id === "profile") return { module: "profiles" };
  if (["release", "source", "entry"].includes(id)) return { module: "guide" };
  if (
    [
      "recovery",
      "recovery_mode",
      "cold_operation",
      "paused",
      "pending_restore",
      "transaction",
      "installation",
    ].includes(id)
  ) {
    return { module: "maintenance" };
  }
  const runtimeSetting = [
    "runtime",
    "node",
    "npm",
    "pnpm",
    "node_program",
    "launch",
    "working_directory",
  ].includes(id);
  return { module: "settings", section: runtimeSetting ? "runtime" : "harness" };
}

export function needsHarnessInstall(
  config: Record<string, unknown> | null,
  releases: Record<string, unknown> | null,
): boolean {
  if (externalHarnessRoot(config)) return false;
  if (!config || !releases || !Array.isArray(releases.releases)) return false;
  const document = (config.config ?? config) as Record<string, unknown>;
  if (config.harness_env_override === true || document.harness_env_override === true) return false;
  const harness = document.harness as Record<string, unknown> | undefined;
  return !harness?.program && releases.releases.length === 0;
}

/** Keep only non-credential catalog data from the same verified Agent. */
export function lifecycleBusySnapshot<
  T extends {
    profiles: unknown;
    releases: unknown;
    state: unknown;
    harnessRuntime: unknown;
    harnessUi: unknown;
    recovery: unknown;
  },
>(next: T, previous: T, sameOwner: boolean): T & { lifecycleBusy: boolean } {
  return {
    ...next,
    lifecycleBusy: true,
    profiles: sameOwner ? previous.profiles : null,
    releases: sameOwner ? previous.releases : null,
    state: null,
    harnessRuntime: null,
    harnessUi: null,
    recovery: null,
  };
}

export function harnessControlGate(
  state: string | undefined,
  pid: number | undefined,
  busy: boolean,
  bridgeAvailable: boolean,
): HarnessControlGate {
  const controllableState =
    state === "detached" || state === "stopped" || state === "failed" || state === "running";
  // A PID-less running Harness can be a descendant that survived its
  // bootstrap parent or an externally restarted instance. It remains
  // observable, but lifecycle operations must stay read-only until this Agent
  // has a child handle again.
  const externallyManaged = state === "running" && pid === undefined;
  return {
    controlsDisabled: busy || !bridgeAvailable || !controllableState || externallyManaged,
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
    cancel: () => {
      generation += 1;
    },
  };
}

/** Seed from history, then notify once per failure even across busy/empty refreshes. */
export function createFailureNoticeTracker() {
  let initialized = false;
  const seen = new Set<string>();
  return {
    observe(keys: string[]): boolean {
      const fresh = initialized && keys.some((key) => !seen.has(key));
      keys.forEach((key) => seen.add(key));
      initialized = true;
      return fresh;
    },
  };
}

export function coldOperationIsTerminal(phase: unknown): boolean {
  return (
    phase === "prepared" || phase === "succeeded" || phase === "cancelled" || phase === "failed"
  );
}

export function actionNoticeKey(path: string, action: unknown): string {
  if (path === "/v1/harness" && (action === "start" || action === "restart"))
    return "Harness startup requested. Check its status and Web entry to confirm readiness.";
  if (path === "/v1/updates" && (action === "offline_import" || action === "offline_export"))
    return "Offline package request accepted. Follow the current stage to confirm completion.";
  if (path === "/v1/updates" && (action === "switch" || action === "confirm")) {
    return "Installation request accepted. Follow the current stage below to confirm completion.";
  }
  if (path === "/v1/updates" && action === "cancel")
    return "Cancellation requested. Waiting for cleanup to finish.";
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
  reason:
    "busy" | "harness_not_stopped" | "update_active" | "cold_active" | "cleanup_pending" | null;
};

/** Mirrors the backend prerequisites while keeping the backend authoritative. */
export function runtimeSettingsGate(
  harnessState: unknown,
  harnessPid: number | undefined,
  updateState: unknown,
  coldPhase: unknown,
  cleanupPending: boolean,
  busy: boolean,
  allowRunning = false,
): RuntimeSettingsGate {
  if (busy) return { disabled: true, reason: "busy" };
  if (
    !(allowRunning && harnessState === "running") &&
    (!["stopped", "detached", "failed"].includes(String(harnessState)) || harnessPid !== undefined)
  ) {
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

// Hidden windows retain a modest heartbeat so native tray state stays fresh.
export function launcherPollDelay(
  state: string | undefined,
  unchangedFailures: number,
  hidden: boolean,
): number {
  if (hidden) return 8000;
  if (state === "busy") return 1000;
  if (state === "starting") return 400;
  if (state === "failed") return Math.min(8000, 400 * 2 ** Math.min(unchangedFailures, 5));
  return 8000;
}

export function patchPreviewExpired(expiresAt: unknown, nowMillis: number): boolean {
  return (
    typeof expiresAt !== "number" || !Number.isFinite(expiresAt) || nowMillis >= expiresAt * 1000
  );
}

/** Dropping on a later row moves after it; on an earlier row moves before it. */
export function pluginMoveTarget(
  order: string[],
  source: string,
  destination: string,
): { target: string | null } | null {
  const from = order.indexOf(source),
    to = order.indexOf(destination);
  if (
    from < 0 ||
    to < 0 ||
    from === to ||
    FIXED_PROFILE_PLUGINS.includes(source) ||
    FIXED_PROFILE_PLUGINS.includes(destination)
  )
    return null;
  return { target: from < to ? (order[to + 1] ?? null) : destination };
}
