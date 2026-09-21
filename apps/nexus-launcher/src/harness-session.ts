import { asObject, harnessRuntimeValue, numberValue, stringValue } from "./json-values";
import { type Snapshot } from "./app-types";

export function harnessUiMatchesRuntime(
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
  if (
    credentialInvalidationPending ||
    stringValue(runtime, "state") !== "running" ||
    !(attachedProcess || recoveredProcess) ||
    info.available !== true
  ) {
    return false;
  }
  const generation = numberValue(response, "generation");
  if (generation === undefined || generation !== numberValue(info, "generation")) return false;
  const runId = stringValue(response, "log_session_run_id");
  return runId !== undefined && runId === stringValue(info, "run_id");
}

export function harnessSessionKey(snapshot: Snapshot): string | undefined {
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

/** Host readiness precedes the browser audit; keep observing that transition. */
export function clientCheckPending(snapshot: Snapshot): boolean {
  if (stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state") !== "running")
    return false;
  if (!harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi)) return true;
  const health = asObject(asObject(snapshot.harnessUi).browser_health);
  return !stringValue(health, "state") || health.state === "checking";
}

/** A live host and URL are not proof that the browser client can be used. */
export function harnessBrowserReady(snapshot: Snapshot, invalidating = false): boolean {
  return (
    harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi, invalidating) &&
    asObject(asObject(snapshot.harnessUi).browser_health).state === "active"
  );
}
