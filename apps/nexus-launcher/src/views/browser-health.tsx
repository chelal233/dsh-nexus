import { type Snapshot, type HarnessPanelProps } from "../app-types";
import {
  asObject,
  arrayValue,
  stringValue,
  numberValue,
  harnessRuntimeValue,
} from "../json-values";
import { useI18n, type Translator } from "../i18n";
import { harnessUiMatchesRuntime } from "../harness-session";
import { ActionButton } from "../ui-components";
import { harnessControlGate, pluginPolicyVerified } from "../control-state";
import { StartupRepair } from "./startup-repair";

function currentHealth(snapshot: Snapshot) {
  return harnessUiMatchesRuntime(snapshot.harnessRuntime, snapshot.harnessUi)
    ? asObject(asObject(snapshot.harnessUi).browser_health)
    : {};
}

export function clientStartupLabel(snapshot: Snapshot, t: Translator, compact = false) {
  const profiles = asObject(snapshot.profiles);
  const report = asObject(profiles.compatibility);
  switch (stringValue(currentHealth(snapshot), "state")) {
    case "active":
      if (
        stringValue(asObject(report.diagnosis), "level") === "limited" &&
        pluginPolicyVerified(profiles) &&
        (!stringValue(snapshot.releases, "current_release") ||
          report.release_id === stringValue(snapshot.releases, "current_release")) &&
        stringValue(report, "effective_profile") === stringValue(profiles, "active_profile")
      )
        return compact ? t("Ready with warnings") : t("Startup checks found limited functionality");
      return compact ? t("Ready") : t("Startup checks passed");
    case "limited":
      return compact ? t("Services unavailable") : t("Startup checks found limited functionality");
    case "blocked":
      return compact ? t("Client unavailable") : t("Client startup failed");
    case "checking":
      return compact ? t("Checking client") : t("Checking client plugins");
    default:
      return compact ? t("Awaiting verification") : t("Startup not yet verified");
  }
}

export function StartupWarning({
  snapshot,
  onDetails,
}: {
  snapshot: Snapshot;
  onDetails: () => void;
}) {
  const { t } = useI18n();
  if (clientStartupLabel(snapshot, t, true) !== t("Ready with warnings")) return null;
  const diagnosis = asObject(asObject(asObject(snapshot.profiles).compatibility).diagnosis);
  const entries = arrayValue(asObject(diagnosis.activation), "entries").map(asObject);
  return (
    <div className="startup-warning-summary" role="status">
      <p>{t("Harness is ready; some optional plugins did not activate")}</p>
      {entries.slice(0, 3).map((entry, index) => (
        <p key={index}>
          <strong>{stringValue(entry, "package") || stringValue(entry, "id")}</strong>:{" "}
          {stringValue(entry, "reason")}
        </p>
      ))}
      <ActionButton onClick={onDetails}>{t("Service and plugin details")}</ActionButton>
    </div>
  );
}

export function BrowserHealth({
  snapshot,
  busyAction,
  runAction,
  openProfiles,
  onRepair,
  showLifecycleControls = true,
}: {
  snapshot: Snapshot;
  showLifecycleControls?: boolean;
  busyAction?: HarnessPanelProps["busyAction"];
  runAction?: HarnessPanelProps["runAction"];
  openProfiles?: () => void;
  onRepair?: (id: string) => void;
}) {
  const { t } = useI18n();
  if (stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state") !== "running") return null;
  const health = currentHealth(snapshot);
  const controlsDisabled =
    !!snapshot.lifecycleBusy ||
    harnessControlGate(
      "running",
      numberValue(harnessRuntimeValue(snapshot.harnessRuntime), "pid"),
      busyAction != null,
      snapshot.startup?.available === true,
    ).controlsDisabled;
  const state = stringValue(health, "state") || "unverified";
  const entries = arrayValue(health, "entries").map(asObject);
  const waiting = new Map<string, string[]>();
  for (const entry of entries)
    for (const service of arrayValue(entry, "missing")) {
      if (typeof service !== "string") continue;
      const names = waiting.get(service) || [];
      names.push(stringValue(entry, "name") || "unknown");
      waiting.set(service, names);
    }
  const failures = entries.filter(
    (entry) => !["pending", "loading"].includes(stringValue(entry, "state") || ""),
  );
  return (
    <section
      className={state === "blocked" ? "notice form-error browser-health" : "notice browser-health"}
      role={state === "blocked" ? "alert" : "status"}
    >
      <strong>{clientStartupLabel(snapshot, t)}</strong>
      {runAction && ["blocked", "limited"].includes(state) && (
        <StartupRepair snapshot={snapshot} busyAction={busyAction ?? null} runAction={runAction} />
      )}
      <p>
        {state === "active"
          ? t(
              "Client activation and core services were observed. This does not verify every conversation or tool operation.",
            )
          : state === "limited"
            ? t(
                "Client plugins activated, but required core services are missing. Review the listed services before use.",
              )
            : state === "blocked"
              ? t(
                  "Harness is running, but its browser plugins are not ready. Stop, profile selection and dependency repair remain available.",
                )
              : state === "checking"
                ? t(
                    "The host is ready. Nexus is checking client plugins in the same Harness instance before reporting startup success.",
                  )
                : t(
                    "Client verification did not complete. Review the startup log and retry startup; an accessible web address alone does not prove readiness.",
                  )}
      </p>
      {stringValue(health, "reason") === "client_audit_load_failed" && (
        <p>{t("The client check page could not load or stopped responding.")}</p>
      )}
      {stringValue(health, "reason") === "client_audit_timeout" && (
        <p>{t("No conclusive client report arrived within 45 seconds.")}</p>
      )}
      {(entries.length > 0 || arrayValue(health, "missing_core").length > 0) && (
        <details className="startup-diagnostics">
          <summary>{t("Service and plugin details")}</summary>
          {arrayValue(health, "missing_core").length > 0 && (
            <p>
              <strong>{t("Unavailable core services")}: </strong>
              {arrayValue(health, "missing_core").join(", ")}
            </p>
          )}
          {failures.map((entry, i) => (
            <p key={i}>
              <strong>{stringValue(entry, "name")}</strong>: {stringValue(entry, "state")}
            </p>
          ))}
          {failures.length > 0 && (
            <p>
              {t(
                "Inspect the reported package and its original cause before choosing a compatible version or disabling it.",
              )}
            </p>
          )}
          {waiting.size > 0 && (
            <>
              <p>
                {t(
                  "Waiting consumers are not confirmed faulty. Inspect the missing service provider first; no provider is inferred from its name.",
                )}
              </p>
              {[...waiting]
                .sort((a, b) => b[1].length - a[1].length)
                .map(([service, names]) => (
                  <details key={service}>
                    <summary>
                      {service} · {t("Waiting plugins")}: {names.length}
                    </summary>
                    <ul>
                      {names.map((name, i) => (
                        <li key={i}>
                          <code>{name}</code>
                        </li>
                      ))}
                    </ul>
                  </details>
                ))}
            </>
          )}
          {health.truncated === true && (
            <p>
              {t(
                "Diagnostic evidence was truncated; inspect the Harness browser error for the complete list.",
              )}
            </p>
          )}
        </details>
      )}
      {state !== "active" && state !== "checking" && (
        <div className="button-row">
          {openProfiles && (
            <ActionButton onClick={openProfiles}>{t("Go to profile management")}</ActionButton>
          )}
          {onRepair && (
            <ActionButton onClick={() => onRepair("recovery")}>
              {t("Open the startup log")}
            </ActionButton>
          )}
          {runAction && showLifecycleControls && (
            <>
              <ActionButton
                disabled={controlsDisabled}
                onClick={() => void runAction(t("Stop Harness"), "/v1/harness", { action: "stop" })}
              >
                {t("Stop Harness")}
              </ActionButton>
              <ActionButton
                disabled={controlsDisabled}
                onClick={() =>
                  void runAction(t("Retry Harness startup"), "/v1/harness", { action: "restart" })
                }
              >
                {t("Retry Harness startup")}
              </ActionButton>
            </>
          )}
        </div>
      )}
    </section>
  );
}
