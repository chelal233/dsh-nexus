import { useRef, useState } from "react";
import { type Snapshot, type HarnessPanelProps } from "../app-types";
import {
  asObject,
  arrayValue,
  stringValue,
  numberValue,
  harnessRuntimeValue,
} from "../json-values";
import { harnessControlGate, pluginPolicyVerified } from "../control-state";
import { confirmAction } from "../confirmation";
import { useI18n } from "../i18n";
import { ActionButton } from "../ui-components";

export function startupRepairPlan(snapshot: Snapshot) {
  const profiles = asObject(snapshot.profiles),
    report = asObject(profiles.compatibility);
  const profile = stringValue(profiles, "active_profile");
  if (
    !profile ||
    !["failed", "needs_choice"].includes(String(report.status)) ||
    (!!stringValue(snapshot.releases, "current_release") &&
      report.release_id !== stringValue(snapshot.releases, "current_release")) ||
    stringValue(report, "source_profile") !== profile ||
    !pluginPolicyVerified(profiles)
  )
    return [];
  const manifest = arrayValue(profiles, "manifests")
    .map(asObject)
    .find((row) => row.name === profile);
  const bundles = arrayValue(manifest, "bundles");
  const seen = new Set<string>();
  return arrayValue(asObject(report.diagnosis), "repair_candidates")
    .map(asObject)
    .filter(
      (row) =>
        typeof row.package === "string" &&
        !row.package.startsWith("@deepseek-ai/") &&
        bundles.includes(row.package) &&
        ["activation_failure", "replaces_official_entry"].includes(String(row.evidence)),
    )
    .filter((row) => {
      const name = String(row.package);
      if (seen.has(name)) return false;
      seen.add(name);
      return true;
    });
}

export async function executeStartupRepair(
  snapshot: Snapshot,
  runAction: HarnessPanelProps["runAction"],
  label: string,
  checkOnly = false,
) {
  const plan = startupRepairPlan(snapshot);
  if (!checkOnly && !plan.length) return false;
  const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
  if (
    harnessControlGate(
      stringValue(runtime, "state"),
      numberValue(runtime, "pid"),
      false,
      snapshot.startup?.available === true,
    ).controlsDisabled ||
    snapshot.lifecycleBusy
  )
    return false;
  if (
    runtime.state === "running" &&
    (await runAction(label, "/v1/harness", { action: "stop" })) !== true
  )
    return false;
  if (checkOnly)
    return (await runAction(label, "/v1/profiles", { action: "compatibility_check" })) === true;
  for (const row of plan) {
    if (
      (await runAction(label, "/v1/profiles", {
        action: "plugin_disable",
        profile: stringValue(snapshot.profiles, "active_profile"),
        package: row.package,
      })) !== true
    )
      return false;
  }
  // Start already owns the compatibility gate. Never bypass it or equate an
  // accepted start with client readiness; the normal audit supplies the result.
  return (await runAction(label, "/v1/harness", { action: "start" })) === true;
}

export function StartupRepair({ snapshot, busyAction, runAction }: HarnessPanelProps) {
  const { t } = useI18n();
  const [working, setWorking] = useState(false),
    active = useRef(false);
  const plan = startupRepairPlan(snapshot);
  const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
  const report = asObject(asObject(snapshot.profiles).compatibility);
  const hostEntries = arrayValue(asObject(asObject(report.diagnosis).activation), "entries").map(
    asObject,
  );
  const browser = asObject(asObject(snapshot.harnessUi).browser_health);
  if (
    !plan.length &&
    !(hostEntries.length && ["failed", "needs_choice"].includes(String(report.status))) &&
    !["blocked", "limited"].includes(String(browser.state))
  )
    return null;
  const disabled =
    working ||
    !!snapshot.lifecycleBusy ||
    harnessControlGate(
      stringValue(runtime, "state"),
      numberValue(runtime, "pid"),
      busyAction != null,
      snapshot.startup?.available === true,
    ).controlsDisabled;
  const repair = async () => {
    if (active.current || disabled) return;
    active.current = true;
    setWorking(true);
    try {
      const message = plan.length
        ? t(
            "Stop Harness, temporarily disable {plugins}, then check and start again? Running tasks will stop. Packages and data are retained; plugins can be re-enabled in Profiles.",
            { plugins: plan.map((row) => row.package).join(", ") },
          )
        : t(
            "Stop Harness and run a fresh startup check to identify repair candidates? Running tasks will stop; no plugins will be changed.",
          );
      if (await confirmAction(message))
        await executeStartupRepair(snapshot, runAction, t("Apply startup repair"), !plan.length);
    } finally {
      active.current = false;
      setWorking(false);
    }
  };
  return (
    <section className="startup-repair">
      <strong>{t("Recommended recovery")}</strong>
      {plan.length ? (
        <>
          <p>
            {t(
              "Temporarily disable the following plugins, then check and restart. Packages and data are retained.",
            )}
          </p>
          <ul>
            {plan.map((row) => (
              <li key={String(row.package)}>
                <code>{String(row.package)}</code>
                {row.evidence === "activation_failure"
                  ? t("This plugin reported its own failure")
                  : t("This plugin replaces a built-in entry while services are unavailable")}
              </li>
            ))}
          </ul>
          <p>
            {t(
              "The disabled plugins' features will be unavailable. Re-enable them in Profiles after installing compatible versions.",
            )}
          </p>
        </>
      ) : (
        <p>{t("Run a fresh check to prepare a repair plan.")}</p>
      )}
      <ActionButton tone="primary" disabled={disabled} onClick={() => void repair()}>
        {working
          ? t("Working")
          : plan.length
            ? t("Disable recommended plugins and retry")
            : t("Recheck and prepare repair")}
      </ActionButton>
    </section>
  );
}
