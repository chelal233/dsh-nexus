import { confirmAction } from "../confirmation";
import { releasePromotionCommand } from "../settings-state";
import { type ViewProps, type Snapshot, type JsonObject } from "../app-types";
import {
  nestedValue,
  stringValue,
  booleanValue,
  harnessRuntimeValue,
  asObject,
  arrayValue,
  numberValue,
} from "../json-values";
import { PageIntro, Panel, ActionButton, StatusPill, Modal } from "../ui-components";
import { activeProgramSource } from "./workbench";
import { HarnessSourcePanel } from "./settings";
import { UpdatesView } from "./updates";
import { useI18n } from "../i18n";
import { useDraftState } from "../draft-memory";
import {
  coldOperationIsTerminal,
  hasHarnessSource,
  needsHarnessInstall,
  externalHarnessRoot,
  pluginPolicyVerified,
  recoveryMutationGate,
  validStartupCheck,
} from "../control-state";
import { useState, useEffect, useRef } from "react";
import { Gear, Package, CheckCircle, SlidersHorizontal } from "@phosphor-icons/react";
import { formatTimestamp, errorMessage, preflightReasonLabel } from "../display-format";
import { proxyRequest } from "../agent-bridge";
import { RecoveryLogTail } from "./recovery";
import { BrowserHealth } from "./browser-health";
import { StartupRepair, startupRepairPlan } from "./startup-repair";

export function GuideView(props: ViewProps) {
  const { t } = useI18n();
  const { snapshot, busyAction } = props;
  const [step, setStep] = useDraftState("guide.step", 0);
  const installation = nestedValue(snapshot.updates, "operation");
  const installing =
    !!stringValue(installation, "operation_id") &&
    (!coldOperationIsTerminal(stringValue(installation, "phase")) ||
      booleanValue(installation, "cleanup_pending"));
  const sourceReady =
    hasHarnessSource(snapshot.config, snapshot.releases) &&
    !needsHarnessInstall(snapshot.config, snapshot.releases);
  const ready = sourceReady && !snapshot.lifecycleBusy && !installing && busyAction === null;
  const external = externalHarnessRoot(snapshot.config);
  const home = stringValue(nestedValue(snapshot.config, "harness_preferences"), "home");
  const [chooseExternal, setChooseExternal] = useState(false);
  const [checkResult, setCheckResult] = useState<JsonObject | null>(null);
  const sourceRevision = stringValue(snapshot.config, "revision");
  const sourceState = stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state");
  const started = ["starting", "running"].includes(sourceState || "");
  const sourceDisabled =
    busyAction !== null ||
    !!snapshot.lifecycleBusy ||
    !snapshot.startup?.available ||
    !["stopped", "detached", "failed"].includes(sourceState || "");
  const prepareDisabled = sourceDisabled || installing;
  const startupAvailable =
    snapshot.startup?.available === true && !booleanValue(snapshot.health, "degraded");
  const checksReady = !!checkResult && booleanValue(checkResult, "ready");
  const steps = [
    { label: "Prepare", detail: "Choose the install method", done: sourceReady },
    {
      label: "Install Harness",
      detail: "Install a version or select a directory",
      done: sourceReady && !installing,
    },
    {
      label: "Check and start",
      detail: "Run the startup check and start Harness",
      done: started,
    },
  ];
  const installManaged = async () => {
    if (prepareDisabled) return;
    if (
      external &&
      !(await props.runAction(t("Select Harness source"), "/v1/config", {
        action: "clear_external_harness",
        expected_revision: sourceRevision,
      }))
    )
      return;
    setChooseExternal(false);
    setStep(1);
  };
  return (
    <>
      <PageIntro
        kicker={t("Setup guide")}
        title={t("Set up Harness in three steps")}
        detail={t(
          "Choose an install method, install or select a version, then run the startup check and start Harness.",
        )}
      />
      <nav className="setup-journey" aria-label={t("Setup progress")}>
        {steps.map((item, index) => (
          <div
            key={item.label}
            className={item.done ? "step-done" : index === step ? "step-current" : ""}
            aria-current={index === step ? "step" : undefined}
          >
            <span>{`${t("Step")} ${index + 1} · ${t(item.label)}`}</span>
            <strong>{t(item.detail)}</strong>
            <em className="step-state">
              {item.done ? t("Completed") : index === step ? t("In progress") : t("Not started")}
            </em>
          </div>
        ))}
      </nav>
      {sourceReady && !installing && !started && step !== 2 && (
        <section className="notice" aria-live="polite">
          <span>
            {t("Harness is ready. Continue to the final step, or open the Workbench directly.")}
          </span>
          <ActionButton onClick={() => setStep(2)}>{t("Go to the final step")}</ActionButton>
          <ActionButton tone="primary" onClick={() => props.openWorkbench?.()}>
            {t("Open Workbench")}
          </ActionButton>
        </section>
      )}
      {step === 0 && (
        <section>
          <Panel title={t("Prepare")} icon={<Gear size={18} />}>
            <dl className="detail-list">
              <dt>{t("Harness data directory")}</dt>
              <dd>{home || t("Inherit upstream default")}</dd>
              <dt>{t("Active program source")}</dt>
              <dd>{activeProgramSource(snapshot, t)}</dd>
            </dl>
            <p>
              {t(
                "Choose a version and install it with the bundled runtime, or select an already built local directory.",
              )}
            </p>
            <div className="button-row">
              <ActionButton
                tone="primary"
                disabled={prepareDisabled}
                onClick={() => void installManaged()}
              >
                {t("Choose version and install")}
              </ActionButton>
              <ActionButton
                disabled={prepareDisabled}
                onClick={() => {
                  setChooseExternal(true);
                  setStep(1);
                }}
              >
                {t("Use an already built directory")}
              </ActionButton>
              <ActionButton onClick={() => props.openSettings?.()}>
                {t("Open Settings")}
              </ActionButton>
            </div>
            <p className="field-help">
              {t(
                "Recommended for most setups: choose a version and install it with the bundled runtime.",
              )}
            </p>
            {installing && (
              <p className="field-help">
                {t("An installation is already in progress; wait for it to finish.")}
              </p>
            )}
            {sourceDisabled && !installing && (
              <p className="field-help">{t("Stop Harness before changing its program source.")}</p>
            )}
          </Panel>
        </section>
      )}
      {step === 1 && (
        <section>
          {external || chooseExternal ? (
            <>
              <Panel title={t("External directory")} icon={<Package size={18} />}>
                {external ? (
                  <>
                    <p>{external}</p>
                    <p>
                      {t(
                        "The selected external program is used directly. Nexus does not install or build its files.",
                      )}
                    </p>
                  </>
                ) : (
                  <p>
                    {t(
                      "Select an already built Harness directory below; Nexus uses it directly and does not install or build its files.",
                    )}
                  </p>
                )}
              </Panel>
              <HarnessSourcePanel {...props} />
              <div className="form-actions">
                <ActionButton disabled={!external || !ready} onClick={() => setStep(2)}>
                  {t("Use this external Harness")}
                </ActionButton>
              </div>
            </>
          ) : (
            <UpdatesView {...props} embedded autoLoadTags={step === 1} />
          )}
          <div className="form-actions">
            <ActionButton onClick={() => setStep(0)}>{t("Previous step")}</ActionButton>
            {!external && !chooseExternal && (
              <ActionButton
                tone="primary"
                disabled={!ready || busyAction !== null}
                onClick={() => setStep(2)}
              >
                {t("Next step")}
              </ActionButton>
            )}
          </div>
        </section>
      )}
      {step === 2 && (
        <section>
          <Panel title={t("Check and start")} icon={<CheckCircle size={18} />}>
            {started ? (
              <>
                <StatusPill label={t("Harness is starting or running")} tone="good" />
                <p>{t("Harness is starting or running. Open the Workbench to use it.")}</p>
                <ActionButton tone="primary" onClick={() => props.openWorkbench?.()}>
                  {t("Open Workbench")}
                </ActionButton>
              </>
            ) : (
              <>
                <BasicStartupCheck
                  disabled={busyAction !== null || !!snapshot.lifecycleBusy}
                  recheckEpoch={1}
                  onResult={setCheckResult}
                />
                <StartupOperationPanel
                  available={startupAvailable}
                  identity={`${stringValue(snapshot.health, "instance_id")}:${stringValue(snapshot.health, "data_root_id")}`}
                />
                <div className="form-actions">
                  <ActionButton onClick={() => setStep(1)}>{t("Previous step")}</ActionButton>
                  <ActionButton
                    tone="primary"
                    disabled={busyAction !== null || !checksReady || !startupAvailable || !ready}
                    onClick={() =>
                      void props.runAction(t("Start Harness"), "/v1/harness", { action: "start" })
                    }
                  >
                    {t("Start Harness")}
                  </ActionButton>
                </div>
                {checksReady && (
                  <p className="field-help">
                    {t("All checks passed. Start Harness when you are ready.")}
                  </p>
                )}
                {checkResult && !booleanValue(checkResult, "ready") && (
                  <p className="field-help">
                    {t("Resolve the blocked checks above, then start Harness.")}
                  </p>
                )}
              </>
            )}
          </Panel>
        </section>
      )}
    </>
  );
}

export function CompatibilitySummary({
  snapshot,
  busyAction,
  runAction,
  onRepair,
}: Pick<ViewProps, "snapshot" | "busyAction" | "runAction" | "onRepair">) {
  const { locale, t } = useI18n();
  const report = asObject(asObject(snapshot.profiles).compatibility);
  const policy = arrayValue(snapshot.profiles, "disabled_plugins").map(String);
  const [selected, setSelected] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [retryError, setRetryError] = useState("");
  const reportKey = `${stringValue(report, "source_profile")}:${stringValue(report, "release_id")}:${numberValue(report, "checked_at_unix")}`;
  useEffect(() => setSelected([]), [reportKey]);
  const hasReport = Object.keys(report).length > 0;
  const policyVerified = pluginPolicyVerified(asObject(snapshot.profiles));
  if (!hasReport && !policy.length) return null;
  const triggerLabel = (value: string | undefined) =>
    value === "manual_check"
      ? t("Manual plugin verification")
      : value === "version_switch"
        ? t("During version switch")
        : value === "profile_switch"
          ? t("During profile switch")
          : value === "startup"
            ? t("Before startup or restart")
            : t("Legacy record: trigger not recorded");
  const needsChoice = stringValue(report, "status") === "needs_choice";
  const failedReport = stringValue(report, "status") === "failed";
  const rawError = stringValue(report, "error") || "";
  const diagnosis = asObject(report.diagnosis);
  const duplicateId = /duplicate loader entry id: ([\w.-]+)/i.exec(rawError)?.[1];
  const blockingError = duplicateId
    ? t(
        "Duplicate plugin entry ID: {id}. Disable or adjust one of the conflicting plugins before retrying.",
        { id: duplicateId },
      )
    : rawError
        .replace(
          /^Startup check needs an explicit plugin decision; original profile preserved\. Original error:\s*/i,
          "",
        )
        .split("\n")
        .find((line) => line.trim() && !line.startsWith("file:///")) || t("Startup check failed");
  const relatedOrigins =
    (failedReport || needsChoice) && !duplicateId
      ? arrayValue(report, "dependency_origins")
          .map(asObject)
          .filter((origin) => {
            const name = stringValue(origin, "package");
            return (
              !!name &&
              (rawError.includes(`'${name}'`) ||
                rawError.includes(`"${name}"`) ||
                rawError.replaceAll("\\", "/").includes(`/node_modules/${name}`))
            );
          })
      : [];

  const repairPlan = startupRepairPlan(snapshot);
  const candidates = arrayValue(report, "candidates");
  const source =
    stringValue(report, "source_profile") || stringValue(snapshot.profiles, "active_profile");
  const target = stringValue(report, "release_id");
  const recovery = asObject(snapshot.recovery);
  const gate = recoveryMutationGate(
    booleanValue(recovery, "harness_stop_required"),
    asObject(recovery.harness).state,
    busyAction !== null || saving,
  );
  const blocked = gate.disabled;
  const saveChoices = async () => {
    setSaving(true);
    try {
      for (const packageName of selected) {
        if (
          !(await runAction(t("Disable plugin in this profile"), "/v1/profiles", {
            action: "plugin_disable",
            profile: source,
            package: packageName,
          }))
        )
          return;
      }
      setSelected([]);
    } finally {
      setSaving(false);
    }
  };
  const installed = arrayValue(snapshot.releases, "releases").some(
    (item) => stringValue(item, "id") === target,
  );
  const operation = asObject(asObject(snapshot.updates).operation);
  const retryTag =
    stringValue(operation, "release_id") === target ? stringValue(operation, "tag") : null;
  const trigger = stringValue(report, "last_trigger") || stringValue(report, "trigger");
  const retryLabel =
    trigger === "manual_check"
      ? t("Verify plugins")
      : trigger === "profile_switch"
        ? t("Retry profile switch")
        : trigger === "startup"
          ? t("Retry Harness startup")
          : t("Retry version switch");
  const retry = async () => {
    setRetryError("");
    setSaving(true);
    try {
      if (trigger === "manual_check")
        return await runAction(retryLabel, "/v1/profiles", { action: "compatibility_check" });
      if (trigger === "profile_switch")
        return await runAction(retryLabel, "/v1/profiles", { action: "select", profile: source });
      if (trigger === "startup")
        return await runAction(retryLabel, "/v1/harness", { action: "start" });
      if (installed && target) {
        const preview = await proxyRequest<JsonObject>("/v1/releases", "POST", {
          action: "promote",
          id: target,
          inspect_only: true,
        });
        const confirmation = stringValue(preview, "rollback_confirmation") || null;
        const command = releasePromotionCommand(
          target,
          confirmation,
          !confirmation ||
            (await confirmAction(
              t(
                "There is no verified rollback version. Switch manually to {version} anyway? If it fails, automatic rollback will be unavailable. Harness will stay stopped.",
                { version: target },
              ),
            )),
        );
        if (command) return await runAction(retryLabel, "/v1/releases", command);
        return;
      }
      return await runAction(retryLabel, "/v1/updates", {
        action: "switch",
        tag: retryTag,
        source: stringValue(operation, "source") || "official",
        mode: stringValue(operation, "mode") || "portable",
      });
    } catch (cause) {
      setRetryError(errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };
  const confirmedActivationFailures = new Set(
    arrayValue(diagnosis, "repair_candidates")
      .filter((item) => stringValue(item, "evidence") === "activation_failure")
      .map((item) => stringValue(item, "package")),
  );
  const faulty = candidates.filter(
    (item) =>
      confirmedActivationFailures.has(stringValue(item, "package")) ||
      [
        "DSH reported a loader error for this plugin",
        "Declares the duplicate loader entry ID",
      ].includes(stringValue(item, "reason") || ""),
  );
  const others = candidates.filter((item) => !faulty.includes(item));
  const renderCandidate = (item: unknown, fault: boolean) => {
    const name = stringValue(item, "package") || "";
    const saved = policy.includes(name);
    return (
      <label key={name} className={fault ? "notice action-error plugin-fault" : "form-check"}>
        <input
          type="checkbox"
          checked={saved || selected.includes(name)}
          disabled={blocked || saved}
          onChange={(event) =>
            setSelected((current) =>
              event.target.checked ? [...current, name] : current.filter((p) => p !== name),
            )
          }
        />
        <strong>{name}</strong>
        {fault && <StatusPill tone="bad" label={t("Loader error")} />}
        <span className="plugin-fault-reason">
          {saved ? t("Disabled") : t(stringValue(item, "reason") || "")}
        </span>
      </label>
    );
  };
  const activation = asObject(diagnosis.activation);
  const activationEntries = arrayValue(activation, "entries").map(asObject);
  const reportedFailures = activationEntries.filter(
    (entry) => stringValue(entry, "state") === "failed",
  );
  const activationWaiting = new Map<string, string[]>();
  for (const entry of activationEntries)
    for (const service of arrayValue(entry, "missing")) {
      if (typeof service !== "string") continue;
      const names = activationWaiting.get(service) || [];
      names.push(stringValue(entry, "package") || "unknown");
      activationWaiting.set(service, names);
    }
  // One audit, two readings: a reported failure is evidence, a pending entry is
  // only its consequence. Lead with the former so a remedy has a target.
  const activationReport = activationEntries.length > 0 && (
    <div className="status-block">
      <strong>{t("Plugins that did not activate")}</strong>
      {reportedFailures.length > 0 ? (
        <>
          <p>
            {t("Start from these reported failures; the plugins listed below only wait on them.")}
          </p>
          {reportedFailures.map((entry, index) => (
            <p key={index}>
              <strong>{stringValue(entry, "package")}</strong>: {stringValue(entry, "reason")}
            </p>
          ))}
        </>
      ) : (
        <p>
          {t(
            "No plugin reported a failure of its own. A required service has no active provider; repair the provider instead of the plugins waiting for it.",
          )}
        </p>
      )}
      {[...activationWaiting]
        .sort((a, b) => b[1].length - a[1].length)
        .map(([service, names]) => (
          <details key={service}>
            <summary>
              {service} · {t("Waiting plugins")}: {names.length}
            </summary>
            <ul>
              {names.map((name, index) => (
                <li key={index}>
                  <code>{name}</code>
                </li>
              ))}
            </ul>
          </details>
        ))}
      {booleanValue(activation, "truncated") && (
        <p>
          {t(
            "Diagnostic evidence was truncated; inspect the Harness browser error for the complete list.",
          )}
        </p>
      )}
    </div>
  );
  return (
    <Panel title={t("Plugin check result")} icon={<SlidersHorizontal size={18} />}>
      <div className="startup-overview">
        <StatusPill
          tone={
            failedReport
              ? "bad"
              : needsChoice || stringValue(diagnosis, "level") === "limited"
                ? "warn"
                : policyVerified
                  ? "good"
                  : "warn"
          }
          label={
            failedReport
              ? t("Startup check failed")
              : needsChoice
                ? t("Choose how to handle plugin errors")
                : policyVerified
                  ? stringValue(diagnosis, "level") === "limited"
                    ? t("Optional plugin issue")
                    : t("Startup check passed")
                  : t("Startup not yet verified")
          }
        />
        <p>
          {source} · {t("Checked at")}:{" "}
          {formatTimestamp(numberValue(report, "checked_at_unix"), t("Not available"), locale)}
        </p>
      </div>
      {(failedReport || needsChoice) && (
        <StartupRepair snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
      )}
      <details className="startup-diagnostics">
        <summary>{t("Diagnostics and manual recovery")}</summary>
        {!(failedReport || needsChoice) && Object.keys(report).length > 0 && (
          <p className="field-help">
            {t(
              "Checks plugin loading and initialization, not every runtime feature. Original profile and data remain unchanged.",
            )}
          </p>
        )}
        {stringValue(diagnosis, "level") === "limited" && !(failedReport || needsChoice) && (
          <div className="notice" role="status">
            <strong>
              {t("Limited functionality")}: {t(stringValue(diagnosis, "summary") || "")}
            </strong>
            <p>{t(stringValue(diagnosis, "remedy") || "")}</p>
            {activationReport}
          </div>
        )}
        {(failedReport || needsChoice) && (
          <div className="action-error" role="alert">
            <strong>{t("Blocking startup error")}</strong>
            <p style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
              {stringValue(diagnosis, "summary")
                ? t(stringValue(diagnosis, "summary") || "")
                : blockingError}
            </p>
            {stringValue(diagnosis, "remedy") && <p>{t(stringValue(diagnosis, "remedy") || "")}</p>}
            {activationReport}
            {stringValue(diagnosis, "certainty") === "unconfirmed" && (
              <p>{t("Cause not confirmed. No plugin is identified as responsible.")}</p>
            )}
            {!repairPlan.length && (
              <div className="button-row">
                {diagnosis.code === "missing_module" && !!onRepair && (
                  <ActionButton disabled={blocked} onClick={() => onRepair("installation")}>
                    {t("Inspect local dependencies")}
                  </ActionButton>
                )}
                {stringValue(diagnosis, "help") === "settings" &&
                  ["configuration", "patch_target"].includes(String(diagnosis.code)) && (
                    <>
                      <ActionButton
                        disabled={blocked}
                        onClick={() =>
                          void runAction(t("Open the profile patch file"), "/v1/profiles", {
                            action: "open_path",
                            target: "profile_patch",
                          })
                        }
                      >
                        {t("Open the profile patch file")}
                      </ActionButton>
                      <ActionButton
                        disabled={blocked}
                        onClick={() =>
                          void runAction(t("Open Harness settings"), "/v1/profiles", {
                            action: "open_path",
                            target: "settings",
                          })
                        }
                      >
                        {t("Open Harness settings")}
                      </ActionButton>
                    </>
                  )}
                {stringValue(diagnosis, "help") === "settings" &&
                  !["configuration", "patch_target"].includes(String(diagnosis.code)) &&
                  !!onRepair && (
                    <ActionButton
                      disabled={blocked}
                      onClick={() =>
                        onRepair(diagnosis.code === "runtime_arguments" ? "launch" : "port")
                      }
                    >
                      {t("Open Harness settings")}
                    </ActionButton>
                  )}
                {stringValue(diagnosis, "help") === "profiles" && !!onRepair && (
                  <ActionButton disabled={blocked} onClick={() => onRepair("profile")}>
                    {t("Go to profile management")}
                  </ActionButton>
                )}
                {stringValue(diagnosis, "help") === "plugins" && !needsChoice && !!onRepair && (
                  <ActionButton disabled={blocked} onClick={() => onRepair("profile")}>
                    {t("Repair profile dependencies")}
                  </ActionButton>
                )}
                {stringValue(diagnosis, "help") === "logs" && !!onRepair && (
                  <ActionButton disabled={blocked} onClick={() => onRepair("recovery")}>
                    {t("Open the startup log")}
                  </ActionButton>
                )}
                <ActionButton disabled={blocked || saving} onClick={() => void retry()}>
                  {retryLabel}
                </ActionButton>
              </div>
            )}
            {!!faulty.length && (
              <p>
                {t("Related plugins")}:{" "}
                {faulty.map((item) => stringValue(item, "package")).join(", ")}
              </p>
            )}
          </div>
        )}

        {relatedOrigins.length > 0 && (
          <details>
            <summary>{t("Additional dependency evidence")}</summary>
            {relatedOrigins.map((origin) => (
              <div className="notice" key={stringValue(origin, "package")}>
                <strong>
                  {t("Dependency source")}: {stringValue(origin, "package")}
                </strong>
                <p>{t("Declared dependency chains; these do not prove a plugin is faulty.")}</p>
                {arrayValue(origin, "chains").map((chain, index) => (
                  <p key={index}>{Array.isArray(chain) ? chain.map(String).join(" → ") : ""}</p>
                ))}
                {!arrayValue(origin, "chains").length && (
                  <p>{t("Dependency source not confirmed")}</p>
                )}
                {booleanValue(origin, "incomplete") && (
                  <p>{t("Local dependency evidence is incomplete.")}</p>
                )}
                {!!arrayValue(origin, "loader_failures").length && (
                  <p>
                    {t("Loader error")}:{" "}
                    {arrayValue(origin, "loader_failures").map(String).join(", ")}
                  </p>
                )}
              </div>
            ))}
          </details>
        )}
        <details>
          <summary>{t("Plugin version declarations")}</summary>
          <PluginDeclarations report={report} />
        </details>
        <p>
          {t("Source profile")}: {source} · {triggerLabel(stringValue(report, "trigger"))}
          {hasReport && (
            <>
              {" "}
              · {t("Release")}: {target}
            </>
          )}
        </p>
        {hasReport && (
          <p className="field-help">
            {booleanValue(report, "cache_reused")
              ? t("Reused previous check result")
              : stringValue(report, "trigger")
                ? t("New check result")
                : t("Legacy record: trigger not recorded")}
            {!!numberValue(report, "last_used_at_unix") && (
              <>
                {" "}
                · {t("Last used")}:{" "}
                {formatTimestamp(
                  numberValue(report, "last_used_at_unix"),
                  t("Not available"),
                  locale,
                )}{" "}
                · {triggerLabel(stringValue(report, "last_trigger"))}
              </>
            )}
          </p>
        )}
        {hasReport && !policyVerified && (
          <p className="notice">
            {t(
              "Saved plugin choices have not been verified. The report below describes an earlier check.",
            )}
          </p>
        )}
        {failedReport && (
          <details>
            <summary>{t("Error details")}</summary>
            <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{rawError}</pre>
          </details>
        )}
        {stringValue(report, "effective_profile") &&
          stringValue(report, "effective_profile") !== source && (
            <p>
              {t("Verified profile")}: {stringValue(report, "effective_profile")}
            </p>
          )}
        {!Object.hasOwn(asObject(snapshot.profiles), "disabled_plugins") &&
          arrayValue(report, "disabled").length > 0 && (
            <ul>
              {arrayValue(report, "disabled").map((item) => (
                <li key={stringValue(item, "package")}>
                  <code>{stringValue(item, "package")}</code>:{" "}
                  {t(stringValue(item, "reason") || "")}
                </li>
              ))}
            </ul>
          )}
        {policy.length > 0 && (
          <div className="status-block">
            <strong>{t("Disabled plugins")}</strong>
            {policy.map((name) => (
              <div key={name}>
                {name}{" "}
                <ActionButton
                  disabled={blocked}
                  onClick={() =>
                    void runAction(t("Restore plugin on next check"), "/v1/profiles", {
                      action: "plugin_enable",
                      profile: source,
                      package: name,
                    })
                  }
                >
                  {t("Restore plugin on next check")}
                </ActionButton>
              </div>
            ))}
          </div>
        )}
        {needsChoice && (
          <div className="status-block" id="compatibility-plugin-choices">
            <strong>{t("Plugins with loader errors")}</strong>
            {faulty.length ? (
              faulty.map((item) => renderCandidate(item, true))
            ) : (
              <p>
                {t(
                  "No individual plugin was identified. Review error details before isolating plugins.",
                )}
              </p>
            )}
            <p>
              {t(
                "Choose plugins to disable, then retry. Unattributed plugins are options, not confirmed faults. Nothing is uninstalled.",
              )}
            </p>
            {others.length > 0 && (
              <details>
                <summary>
                  {t("Other plugins for troubleshooting")} ({others.length})
                </summary>
                {others.map((item) => renderCandidate(item, false))}
              </details>
            )}
            <details>
              <summary>{t("Error details")}</summary>
              <pre style={{ maxHeight: "16rem", overflow: "auto", whiteSpace: "pre-wrap" }}>
                {stringValue(report, "error")}
              </pre>
            </details>
            {retryError && (
              <p className="form-error" role="alert">
                {retryError}
              </p>
            )}
            <div className="button-row">
              <ActionButton
                disabled={
                  blocked ||
                  !faulty.some((item) => !policy.includes(stringValue(item, "package") || ""))
                }
                onClick={() =>
                  setSelected(
                    faulty
                      .map((item) => stringValue(item, "package") || "")
                      .filter((name) => !policy.includes(name)),
                  )
                }
              >
                {t("Select failing plugins")}
              </ActionButton>
              <ActionButton
                disabled={blocked || !selected.length}
                onClick={() => void saveChoices()}
              >
                {t("Save disabled plugins")}
              </ActionButton>
            </div>
            <p>
              {t(
                "Saved choices belong to this profile. Disabled packages stay installed and can be enabled again in their previous order.",
              )}
            </p>
            {!installed && !retryTag && (
              <p>{t("After saving, select the upstream version again to retry.")}</p>
            )}
            {blocked && <p>{t("Stop Harness before changing plugin isolation.")}</p>}
          </div>
        )}
      </details>
    </Panel>
  );
}

export function currentStartupFailure(snapshot: Snapshot): boolean {
  const runtime = harnessRuntimeValue(snapshot.harnessRuntime);
  if (stringValue(runtime, "state") !== "failed") return false;
  const report = asObject(asObject(snapshot.profiles).compatibility);
  const failedAt = numberValue(runtime, "updated_at_unix");
  const checkedAt = numberValue(report, "last_used_at_unix");
  return failedAt === undefined || checkedAt === undefined || failedAt >= checkedAt;
}

export function StartupOperationPanel({
  available,
  identity,
}: {
  available: boolean;
  identity: string;
}) {
  const { t } = useI18n();
  const [operation, setOperation] = useState<JsonObject | null>(null);
  const [error, setError] = useState("");
  const [cancelling, setCancelling] = useState(false);
  const sequence = useRef(0);
  const [epoch, setEpoch] = useState(0);
  useEffect(() => {
    const token = ++sequence.current;
    let timer: number | undefined;
    let active = false;
    setOperation(null);
    setCancelling(false);
    setError("");
    if (!available) return;
    const poll = async () => {
      try {
        const value = await proxyRequest<JsonObject>("/v1/harness/startup");
        if (sequence.current === token) {
          setOperation(value);
          active = ["checking", "compatibility", "spawning"].includes(String(value.phase));
          setError("");
        }
      } catch (cause) {
        if (sequence.current === token) {
          setOperation(null);
          setError(errorMessage(cause));
        }
      }
      if (sequence.current === token)
        timer = window.setTimeout(
          () => void poll(),
          document.hidden ? 15000 : active ? 1500 : 8000,
        );
    };
    void poll();
    return () => {
      ++sequence.current;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [available, identity, epoch]);
  const cancel = async () => {
    const id = stringValue(operation, "operation_id");
    if (!id) return;
    const token = ++sequence.current;
    setCancelling(true);
    setError("");
    try {
      await proxyRequest("/v1/harness/startup", "POST", { action: "cancel", operation_id: id });
    } catch (cause) {
      if (sequence.current === token) setError(errorMessage(cause));
    } finally {
      if (sequence.current === token) {
        setCancelling(false);
        setEpoch((value) => value + 1);
      }
    }
  };
  const phase = stringValue(operation, "phase");
  // This panel describes live work. Historical failures belong to the failure
  // details, not every later operation that happens to mount this panel.
  if (!available || !["checking", "compatibility", "spawning"].includes(phase || "")) return null;
  const label =
    phase === "checking"
      ? t("Checking startup inputs")
      : phase === "compatibility"
        ? t("Checking startup compatibility")
        : t("Creating Harness process; use Stop after startup");
  return (
    <section className={`notice${error ? " action-error" : ""}`} aria-live="polite">
      <span>{label}</span>
      <details>
        <summary>{t("Preparation timings")}</summary>
        <p>{t("These timings cover launch preparation, not client readiness.")}</p>
        <dl>
          {[
            ["checking", t("Checking startup inputs")],
            ["compatibility", t("Checking startup compatibility")],
            ["spawning", t("Creating Harness process; use Stop after startup")],
          ].map(([key, title]) => {
            const ms = numberValue(asObject(operation?.stage_durations_ms), key);
            return ms !== undefined && Number.isFinite(ms) && ms >= 0 ? (
              <div key={key}>
                <dt>{title}</dt>
                <dd>{t("{seconds} seconds", { seconds: (ms / 1000).toFixed(1) })}</dd>
              </div>
            ) : null;
          })}
        </dl>
      </details>
      {error && <span role="alert">{error}</span>}
      {operation?.cancel_requested === true && (
        <span>{t("Cancellation requested; waiting for checks to stop safely")}</span>
      )}
      {operation?.cancellable === true && (
        <ActionButton
          disabled={cancelling || operation.cancel_requested === true}
          onClick={() => void cancel()}
        >
          {t("Cancel startup")}
        </ActionButton>
      )}
    </section>
  );
}

export function CompatibilityDialog({
  snapshot,
  busyAction,
  runAction,
  pending,
  onClose,
  basicResult,
  basicError,
  onRepair,
  recheckEpoch,
}: Pick<ViewProps, "snapshot" | "busyAction" | "runAction" | "onRepair" | "recheckEpoch"> & {
  pending: boolean;
  onClose: () => void;
  basicResult?: JsonObject | null;
  basicError?: string;
}) {
  const { t } = useI18n();
  const failed = currentStartupFailure(snapshot);
  const recovery = asObject(snapshot.recovery);
  const gate = recoveryMutationGate(
    booleanValue(recovery, "harness_stop_required"),
    asObject(recovery.harness).state,
    busyAction !== null || pending,
  );
  const report = asObject(asObject(snapshot.profiles).compatibility);
  return (
    <Modal title={t("Startup compatibility check")} onClose={onClose}>
      <BrowserHealth
        snapshot={snapshot}
        busyAction={busyAction}
        runAction={
          ["failed", "needs_choice"].includes(String(report.status)) ? undefined : runAction
        }
        onRepair={["failed", "needs_choice"].includes(String(report.status)) ? undefined : onRepair}
      />
      <StartupOperationPanel
        available={
          snapshot.startup?.available === true && !booleanValue(snapshot.health, "degraded")
        }
        identity={`${stringValue(snapshot.health, "instance_id")}:${stringValue(snapshot.health, "data_root_id")}`}
      />
      {!Object.keys(report).length && snapshot.startup?.harness_startup_error && (
        <details>
          <summary>{t("Previous startup error")}</summary>
          <pre style={{ maxHeight: "12rem", overflow: "auto", whiteSpace: "pre-wrap" }}>
            {snapshot.startup.harness_startup_error}
          </pre>
        </details>
      )}
      {pending ? (
        <p role="status">
          {t(
            "Checking plugin compatibility. You can close this dialog; the check continues in the background.",
          )}
        </p>
      ) : (
        <>
          {failed && !["failed", "needs_choice"].includes(String(report.status)) && (
            <div role="alert">
              <p className="form-error">
                {t(
                  "Harness startup failed. The preflight result below does not mean this startup succeeded.",
                )}
              </p>
              <RecoveryLogTail snapshot={snapshot} />
              <ActionButton
                disabled={gate.disabled || snapshot.startup?.available !== true}
                onClick={() =>
                  void runAction(t("Diagnose plugin startup"), "/v1/profiles", {
                    action: "compatibility_check",
                  })
                }
              >
                {t("Diagnose plugin startup")}
              </ActionButton>
              <ActionButton
                disabled={busyAction !== null}
                onClick={() =>
                  void runAction(t("Retry Harness startup"), "/v1/harness", { action: "start" })
                }
              >
                {t("Retry Harness startup")}
              </ActionButton>
            </div>
          )}
          <CompatibilitySummary
            snapshot={snapshot}
            busyAction={busyAction}
            runAction={runAction}
            onRepair={onRepair}
          />
          {!Object.keys(report).length && !failed && (
            <p>{t("No compatibility check result yet.")}</p>
          )}
        </>
      )}
      <details className="startup-diagnostics">
        <summary>{t("Run additional checks")}</summary>
        <BasicStartupCheck
          onRepair={onRepair}
          recheckEpoch={recheckEpoch}
          disabled={pending || busyAction !== null}
          initialResult={basicResult}
          initialError={basicError}
        />
        <section aria-label={t("Verify plugins")}>
          <h3>{t("Verify plugins")}</h3>
          <p>
            {t(
              "This checks plugin loading only. Browser commands, panels and interactions have not been verified.",
            )}
          </p>
          <p>
            {t(
              "Runs plugin initialization in a temporary local process and closes it afterward. The selected profile and the stopped Harness service remain unchanged. No browser is opened.",
            )}
          </p>
          <ActionButton
            disabled={gate.disabled || snapshot.startup?.available !== true}
            onClick={() =>
              void runAction(t("Verify plugins"), "/v1/profiles", { action: "compatibility_check" })
            }
          >
            {t("Verify plugins")}
          </ActionButton>
        </section>
      </details>
    </Modal>
  );
}

export function BasicStartupCheck({
  disabled,
  initialResult,
  initialError,
  onResult,
  onRepair,
  recheckEpoch = 0,
}: {
  onRepair?: (id: string) => void;
  recheckEpoch?: number;
  disabled: boolean;
  initialResult?: JsonObject | null;
  initialError?: string;
  onResult?: (result: JsonObject | null) => void;
}) {
  const { t } = useI18n();
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<JsonObject | null>(initialResult ?? null);
  const [error, setError] = useState(initialError ?? "");
  useEffect(() => {
    setResult(initialResult ?? null);
    setError(initialError ?? "");
  }, [initialResult, initialError]);
  const check = async () => {
    setChecking(true);
    setError("");
    setResult(null);
    onResult?.(null);
    try {
      const response = await proxyRequest("/v1/preflight");
      if (!validStartupCheck(response))
        throw new Error(
          t("Invalid startup check response. Retry the check or export diagnostics."),
        );
      setResult(response);
      onResult?.(response);
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setChecking(false);
    }
  };
  const checkedEpoch = useRef(0);
  useEffect(() => {
    if (recheckEpoch > checkedEpoch.current && !disabled && !checking) {
      checkedEpoch.current = recheckEpoch;
      void check();
    }
  }, [recheckEpoch, disabled, checking]);
  return (
    <section aria-label={t("Basic startup checks")}>
      <h3>{t("Basic startup checks")}</h3>
      <p>
        {t(
          "Checks files, data access, profile, runtime, port and pending recovery. Does not compile or start Harness.",
        )}
      </p>
      <ActionButton disabled={disabled || checking} onClick={() => void check()}>
        {t(checking ? "Checking…" : "Run basic checks")}
      </ActionButton>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      {result && (
        <div className="status-block" aria-live="polite">
          <strong>
            {t(
              booleanValue(result, "ready")
                ? "No blocking issues found"
                : "Resolve the blocking issues before startup",
            )}
          </strong>
          <ul>
            {arrayValue(result, "checks").map((entry, index) => (
              <li key={`${stringValue(entry, "id")}-${index}`}>
                <StatusPill
                  label={t(stringValue(entry, "status") || "Unknown")}
                  tone={
                    stringValue(entry, "status") === "blocked"
                      ? "bad"
                      : stringValue(entry, "status") === "warning"
                        ? "warn"
                        : "good"
                  }
                />
                <strong> {t(stringValue(entry, "id") || "Check")}</strong>:{" "}
                {preflightReasonLabel(asObject(entry), t)}
                {onRepair && stringValue(entry, "status") !== "ok" && (
                  <ActionButton
                    disabled={checking}
                    onClick={() => onRepair(stringValue(entry, "id") || "configuration")}
                  >
                    {t("Open the relevant repair page")}
                  </ActionButton>
                )}
                {stringValue(entry, "next") && <p>{t(stringValue(entry, "next") || "")}</p>}
              </li>
            ))}
          </ul>
          <p>
            {t(
              "Results describe this check only. Startup protection and plugin compatibility checks still apply.",
            )}
          </p>
        </div>
      )}
    </section>
  );
}

function PluginDeclarations({ report }: { report: JsonObject }) {
  const { t } = useI18n();
  const rows = arrayValue(report, "declarations");
  const omitted = numberValue(report, "declarations_omitted") ?? 0;
  if (!rows.length && !omitted) return null;
  return (
    <details>
      <summary>{t("Plugin version declarations")}</summary>
      <p>
        {t("Local manifest declarations only. A match does not guarantee runtime compatibility.")}
      </p>
      {omitted > 0 && (
        <p role="status">
          {t("Declaration details omitted: {count}. This does not affect the startup check.", {
            count: omitted,
          })}
        </p>
      )}
      {rows.map((raw, i) => {
        const row = asObject(raw);
        const status = stringValue(row, "status");
        return (
          <div className="status-block" key={i}>
            <strong>
              {stringValue(row, "package")} {stringValue(row, "version")}
            </strong>
            <StatusPill
              tone={status === "mismatch" ? "warn" : status === "match" ? "good" : "neutral"}
              label={
                status === "mismatch"
                  ? t("Declared mismatch")
                  : status === "match"
                    ? t("Declared match")
                    : t("Declaration unknown")
              }
            />
            {arrayValue(row, "declarations").map((raw, j) => {
              const d = asObject(raw);
              return (
                <span key={j}>
                  {stringValue(d, "dependency")}: {stringValue(d, "required") || "?"} ·{" "}
                  {stringValue(d, "actual") || "?"}
                </span>
              );
            })}
          </div>
        );
      })}
    </details>
  );
}
