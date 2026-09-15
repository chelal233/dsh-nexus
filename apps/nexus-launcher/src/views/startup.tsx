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

export function GuideView(props: ViewProps) {
  const { t } = useI18n();
  const { snapshot, busyAction } = props;
  const [step, setStep] = useDraftState("guide.step", 0);
  const installation = nestedValue(snapshot.updates, "operation");
  const installing =
    !!stringValue(installation, "operation_id") &&
    (!coldOperationIsTerminal(stringValue(installation, "phase")) ||
      booleanValue(installation, "cleanup_pending"));
  const ready =
    hasHarnessSource(snapshot.config, snapshot.releases) &&
    !needsHarnessInstall(snapshot.config, snapshot.releases) &&
    !snapshot.lifecycleBusy &&
    !installing &&
    busyAction === null;
  const external = externalHarnessRoot(snapshot.config);
  const home = stringValue(nestedValue(snapshot.config, "harness_preferences"), "home");
  const [chooseExternal, setChooseExternal] = useState(false);
  const sourceRevision = stringValue(snapshot.config, "revision");
  const sourceState = stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state");
  const sourceDisabled =
    busyAction !== null ||
    !!snapshot.lifecycleBusy ||
    !snapshot.startup?.available ||
    !["stopped", "detached", "failed"].includes(sourceState || "");
  const installManaged = async () => {
    if (sourceDisabled) return;
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
        title={t("Install Harness step by step")}
        detail={t("Prepare your settings, install a version, then continue in Workbench.")}
      />
      <nav className="setup-journey" aria-label={t("Setup progress")}>
        {["Preparation", "Install Harness", "Finish setup"].map((label, index) => (
          <button
            key={label}
            type="button"
            className={index === step ? "step-current" : ""}
            aria-current={index === step ? "step" : undefined}
            disabled={index === 2 && !ready}
            onClick={() => setStep(index)}
          >
            <span>
              {index + 1} · {t(label)}
            </span>
          </button>
        ))}
      </nav>
      <section hidden={step !== 0}>
        <Panel title={t("Preparation")} icon={<Gear size={18} />}>
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
              disabled={sourceDisabled}
              onClick={() => void installManaged()}
            >
              {t("Choose version and install")}
            </ActionButton>
            <ActionButton disabled={sourceDisabled} onClick={() => setChooseExternal(true)}>
              {t("Use an already built directory")}
            </ActionButton>
            <ActionButton onClick={() => props.openSettings?.()}>{t("Open Settings")}</ActionButton>
          </div>
          {sourceDisabled && (
            <p className="field-help">{t("Stop Harness before changing its program source.")}</p>
          )}
        </Panel>
        {chooseExternal && (
          <>
            <HarnessSourcePanel {...props} />
            <div className="form-actions">
              <ActionButton
                tone="primary"
                disabled={!external || !ready}
                onClick={() => setStep(2)}
              >
                {t("Use this external Harness")}
              </ActionButton>
            </div>
          </>
        )}
      </section>
      <section hidden={step !== 1}>
        {external ? (
          <Panel title={t("External directory")} icon={<Package size={18} />}>
            <p>{external}</p>
            <p>
              {t(
                "The selected external program is used directly. Nexus does not install or build its files.",
              )}
            </p>
            <ActionButton disabled={sourceDisabled} onClick={() => void installManaged()}>
              {t("Choose version and install")}
            </ActionButton>
          </Panel>
        ) : (
          <UpdatesView {...props} embedded autoLoadTags={step === 1} />
        )}
        <div className="form-actions">
          <ActionButton onClick={() => setStep(0)}>{t("Previous step")}</ActionButton>
          <ActionButton
            tone="primary"
            disabled={!ready || busyAction !== null}
            onClick={() => setStep(2)}
          >
            {t("Next step")}
          </ActionButton>
        </div>
      </section>
      <section hidden={step !== 2}>
        <Panel title={t("Finish setup")} icon={<CheckCircle size={18} />}>
          <p>
            {t(
              ready
                ? "Harness is installed. Continue in Workbench to check and start it."
                : "Install or select a Harness version before continuing.",
            )}
          </p>
          <ActionButton tone="primary" disabled={!ready} onClick={() => props.openWorkbench?.()}>
            {t("Open Workbench")}
          </ActionButton>
        </Panel>
      </section>
    </>
  );
}

export function CompatibilitySummary({
  snapshot,
  busyAction,
  runAction,
}: Pick<ViewProps, "snapshot" | "busyAction" | "runAction">) {
  const { locale, t } = useI18n();
  const report = asObject(asObject(snapshot.profiles).compatibility);
  const policy = arrayValue(snapshot.profiles, "disabled_plugins").map(String);
  const [selected, setSelected] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
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
  const disabled = arrayValue(report, "disabled");
  const needsChoice = stringValue(report, "status") === "needs_choice";
  const failedReport = stringValue(report, "status") === "failed";
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
          !(await runAction(t("Disable plugin in isolated profiles"), "/v1/profiles", {
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
  const retry = () =>
    trigger === "manual_check"
      ? runAction(retryLabel, "/v1/profiles", { action: "compatibility_check" })
      : trigger === "profile_switch"
        ? runAction(retryLabel, "/v1/profiles", { action: "select", profile: source })
        : trigger === "startup"
          ? runAction(retryLabel, "/v1/harness", { action: "start" })
          : installed
            ? runAction(t("Retry version switch"), "/v1/releases", {
                action: "promote",
                id: target,
              })
            : runAction(t("Retry version switch"), "/v1/updates", {
                action: "switch",
                tag: retryTag,
                source: stringValue(operation, "source") || "official",
                mode: stringValue(operation, "mode") || "portable",
              });
  return (
    <Panel title={t("Startup compatibility check")} icon={<SlidersHorizontal size={18} />}>
      <p>
        {t("Source profile")}: {source}
        {hasReport && (
          <>
            {" "}
            · {t("Release")}: {target}
          </>
        )}
      </p>
      {hasReport && !needsChoice && (
        <p>
          {t("Effective isolated profile")}: {stringValue(report, "effective_profile")}
        </p>
      )}
      {hasReport && (
        <div className="status-block">
          <span>
            {t("Checked at")}:{" "}
            {formatTimestamp(numberValue(report, "checked_at_unix"), t("Not available"), locale)} ·{" "}
            {triggerLabel(stringValue(report, "trigger"))}
          </span>
          <span>
            {booleanValue(report, "cache_reused")
              ? t("Reused previous check result")
              : stringValue(report, "trigger")
                ? t("New check result")
                : t("Legacy record: trigger not recorded")}
            {numberValue(report, "last_used_at_unix")
              ? ` · ${t("Last used")}: ${formatTimestamp(numberValue(report, "last_used_at_unix"), t("Not available"), locale)} · ${triggerLabel(stringValue(report, "last_trigger"))}`
              : ""}
          </span>
          <span>
            {t(
              "Plugin errors below were recorded during this check; they are not new errors from viewing this page.",
            )}
          </span>
        </div>
      )}
      {hasReport && !policyVerified && (
        <p className="notice">
          {t(
            "Saved plugin choices have not been verified. The report below describes an earlier check.",
          )}
        </p>
      )}
      {hasReport && (policyVerified || failedReport || needsChoice) && (
        <StatusPill
          label={
            failedReport
              ? t("Startup check failed")
              : needsChoice
                ? t("Choose how to handle plugin errors")
                : disabled.length
                  ? t("Started with isolated plugins")
                  : t("Startup check passed")
          }
          tone={failedReport ? "bad" : needsChoice || disabled.length ? "warn" : "good"}
        />
      )}
      <p>
        {t(
          "Checks plugin loading and initialization, not every runtime feature. Original profile and data remain unchanged.",
        )}
      </p>
      {failedReport && (
        <p className="form-error" role="alert">
          {stringValue(report, "error")}
        </p>
      )}
      {disabled.length > 0 && (
        <ul>
          {disabled.map((item) => (
            <li key={stringValue(item, "package")}>
              <strong>{stringValue(item, "package")}</strong>:{" "}
              {t(stringValue(item, "reason") || "")}
            </li>
          ))}
        </ul>
      )}
      {policy.length > 0 && (
        <div className="status-block">
          <strong>{t("Saved plugin choices; effective on next check")}</strong>
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
        <div className="status-block">
          <p className="form-error">{stringValue(report, "error")}</p>
          <p>
            {t(
              "Choose plugins to disable, then retry. Unattributed plugins are options, not confirmed faults. Nothing is uninstalled.",
            )}
          </p>
          {candidates.map((item) => {
            const name = stringValue(item, "package") || "";
            return (
              <label key={name}>
                <input
                  type="checkbox"
                  checked={selected.includes(name)}
                  disabled={blocked}
                  onChange={(event) =>
                    setSelected((current) =>
                      event.target.checked ? [...current, name] : current.filter((p) => p !== name),
                    )
                  }
                />{" "}
                <strong>{name}</strong> · {t(stringValue(item, "reason") || "")}
              </label>
            );
          })}
          <div className="button-row">
            <ActionButton
              disabled={blocked || !candidates.length}
              onClick={() =>
                setSelected(candidates.map((item) => stringValue(item, "package") || ""))
              }
            >
              {t("Select all third-party plugins")}
            </ActionButton>
            <ActionButton disabled={blocked || !selected.length} onClick={() => void saveChoices()}>
              {t("Save disabled plugins")}
            </ActionButton>
            {(installed || retryTag || trigger === "profile_switch" || trigger === "startup") && (
              <ActionButton disabled={blocked || selected.length > 0} onClick={() => void retry()}>
                {retryLabel}
              </ActionButton>
            )}
          </div>
          <p>
            {t(
              "Saved choices apply to isolated profiles until restored. The original profile remains intact.",
            )}
          </p>
          {!installed && !retryTag && (
            <p>{t("After saving, select the upstream version again to retry.")}</p>
          )}
          {blocked && <p>{t("Stop Harness before changing plugin isolation.")}</p>}
        </div>
      )}
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
  if (!available || (!phase && !error) || phase === "idle" || phase === "submitted") return null;
  const label =
    phase === "checking"
      ? t("Checking startup inputs")
      : phase === "compatibility"
        ? t("Checking startup compatibility")
        : phase === "spawning"
          ? t("Creating Harness process; use Stop after startup")
          : phase === "cancelled"
            ? t("Startup cancelled. The previous instance is not restarted automatically.")
            : t("Startup preparation failed");
  return (
    <section className="notice" aria-live="polite">
      <span>{label}</span>
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
      <StartupOperationPanel
        available={
          snapshot.startup?.available === true && !booleanValue(snapshot.health, "degraded")
        }
        identity={`${stringValue(snapshot.health, "instance_id")}:${stringValue(snapshot.health, "data_root_id")}`}
      />
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
            "Runs plugin initialization in a temporary local process and closes it afterward. Recovery mode, the selected profile and the stopped Harness service remain unchanged. No browser is opened.",
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
      {snapshot.startup?.harness_startup_error && (
        <p className="form-error" role="alert">
          {snapshot.startup.harness_startup_error}
        </p>
      )}
      {pending ? (
        <p role="status">
          {t(
            "Checking plugin compatibility. You can close this dialog; the check continues in the background.",
          )}
        </p>
      ) : (
        <>
          {failed && (
            <div role="alert">
              <p className="form-error">
                {t(
                  "Harness startup failed. The preflight result below does not mean this startup succeeded.",
                )}
              </p>
              <RecoveryLogTail snapshot={snapshot} />
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
          <CompatibilitySummary snapshot={snapshot} busyAction={busyAction} runAction={runAction} />
          {!Object.keys(report).length && !failed && (
            <p>{t("No compatibility check result yet.")}</p>
          )}
        </>
      )}
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
          <>
            {booleanValue(result, "paused") && (
              <p className="notice">
                {t(
                  "Harness startup is paused. Checks remain available; leave recovery mode before starting.",
                )}
              </p>
            )}
          </>
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
