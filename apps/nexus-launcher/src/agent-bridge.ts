import {
  type StartupStatus,
  type JsonObject,
  type AgentHealthResponse,
  type AgentRequestMethod,
} from "./app-types";
import { invoke } from "./desktop";

export const isBrowserPreview = typeof window === "undefined" || !window.nexusDesktop;

/// Browser-only preview: synthesize the startup status from the proxied
/// Agent health endpoint, since the native auto-start command is unavailable.
export async function commandStartupStatus(): Promise<StartupStatus> {
  if (!isBrowserPreview) {
    let level: string | null = null;
    try {
      level = window.localStorage.getItem("nexus.launcher.agent-log-level");
    } catch {
      // Storage may be unavailable; retain the Agent default.
    }
    if (level && ["error", "warn", "info", "debug", "trace"].includes(level)) {
      // Apply before startup_status can spawn an Agent, including after the
      // desktop adapter restarts. Opening Settings must not be required.
      await invoke("agent_log_set", { level });
    }
    return invoke<StartupStatus>("startup_status");
  }
  const health = await fetch("/agent/v1/health").then(
    (response) => response.json() as Promise<AgentHealthResponse>,
  );
  const alive = health?.status === "ok";
  return {
    available: alive,
    running: alive,
    api_base: "/agent",
    data_root: health?.data_root,
    data_root_id: health?.data_root_id,
    instance_id: health?.instance_id,
  };
}

export async function proxyRequest<T = JsonObject>(
  path: string,
  method: AgentRequestMethod = "GET",
  body?: JsonObject,
): Promise<T> {
  // Browser-only development preview: `pnpm dev` serves the same UI without
  // the Electron bridge, so requests go through the /agent dev proxy instead.
  if (isBrowserPreview) {
    const response = await fetch(`/agent${path}`, {
      method,
      headers: body === undefined ? undefined : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const value = await response.json();
    if (!response.ok) throw value;
    return value as T;
  }
  return invoke<T>("proxy_request", {
    method,
    path,
    body: body ?? null,
  });
}
