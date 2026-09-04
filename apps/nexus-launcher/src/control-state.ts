export type HarnessControlGate = {
  controlsDisabled: boolean;
  externallyManaged: boolean;
};

export function invalidatesHarnessCredentials(path: string, action: unknown): boolean {
  if (action !== "start" && action !== "stop" && action !== "restart") return false;
  return path === "/launcher/agent" || path === "/launcher/agent-api/v1/harness";
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
  const pidlessActive =
    (state === "starting" || state === "running") && pid === undefined;
  const externallyManaged = state === "running" && pid === undefined;
  return {
    controlsDisabled:
      busy || !bridgeAvailable || !controllableState || pidlessActive,
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
