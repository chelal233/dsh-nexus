export type HarnessControlGate = {
  controlsDisabled: boolean;
  externallyManaged: boolean;
};

export function invalidatesHarnessCredentials(path: string, action: unknown): boolean {
  if (path === "/v1/releases" && (action === "promote" || action === "rollback")) return true;
  if (path === "/v1/updates" && (action === "switch" || action === "confirm")) return true;
  if (action !== "start" && action !== "stop" && action !== "restart") return false;
  return path === "/v1/agent" || path === "/v1/harness";
}

export function isLifecycleBusyError(message: string): boolean {
  return /(?:^|: )NEXUS_LIFECYCLE_BUSY:/.test(message);
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

export function coldOperationIsTerminal(phase: unknown): boolean {
  return phase === "succeeded" || phase === "cancelled" || phase === "failed";
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
