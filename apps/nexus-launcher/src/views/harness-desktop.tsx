import { useEffect, useRef, useState } from "react";
import { ArrowRight, ArrowsClockwise, StopCircle } from "@phosphor-icons/react";
import { invoke } from "../desktop";
import { useI18n } from "../i18n";
import { ActionButton } from "../ui-components";
import type { Snapshot } from "../app-types";

type DesktopState = {
  phase: "idle" | "preparing" | "launched" | "stopping" | "stopped" | "failed";
  stage?: string;
  version?: string;
  error?: string;
  detail?: string;
  startedAt?: number;
  audit?: { state: "checking" | "ready" | "failed" | "unverified"; error?: string };
};

export function useHarnessDesktop(snapshot: Snapshot) {
  const release =
    typeof snapshot.releases?.current_release === "string" ? snapshot.releases.current_release : "";
  const external = !!snapshot.config?.external_harness;
  const available = snapshot.startup?.available === true;
  const probeKey = `${release}:${external}:${available}`;
  const [capability, setCapability] = useState<{
    key: string;
    supported: boolean;
    failed?: boolean;
  }>();
  useEffect(() => {
    let disposed = false;
    let pending = false;
    const probe = async () => {
      if (!available || pending) return;
      pending = true;
      try {
        const result = await invoke<{ supported: boolean; release?: string }>(
          "harness_desktop_capability",
        );
        if (
          !result ||
          typeof result.supported !== "boolean" ||
          (result.supported && result.release !== release)
        )
          throw new Error("Desktop support check unavailable");
        if (!disposed) setCapability({ key: probeKey, supported: result.supported });
      } catch {
        if (!disposed) setCapability({ key: probeKey, supported: false, failed: true });
      } finally {
        pending = false;
      }
    };
    void probe();
    const timer = setInterval(() => void probe(), 10000);
    return () => {
      disposed = true;
      clearInterval(timer);
    };
  }, [probeKey, release, available]);
  const [state, setState] = useState<DesktopState>({ phase: "idle" });
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState("");
  const [pollError, setPollError] = useState("");
  const [stopping, setStopping] = useState(false);
  const operation = useRef(0);
  useEffect(() => {
    let disposed = false;
    let polling = false;
    const refresh = async () => {
      if (polling) return;
      polling = true;
      try {
        const next = await invoke<DesktopState>("harness_desktop_status");
        if (
          !next ||
          !["idle", "preparing", "launched", "stopping", "stopped", "failed"].includes(next.phase)
        ) {
          throw new Error("Desktop status unavailable");
        }
        if (!disposed) {
          setState(next);
          setPollError("");
        }
      } catch (failure) {
        if (!disposed) setPollError((failure as Error).message);
      } finally {
        polling = false;
      }
    };
    void refresh();
    const timer = setInterval(() => void refresh(), 2000);
    return () => {
      disposed = true;
      clearInterval(timer);
    };
  }, []);
  const active = ["preparing", "launched", "stopping"].includes(state.phase);
  const launch = async () => {
    setStarting(true);
    setError("");
    try {
      setState(await invoke<DesktopState>("harness_desktop_start"));
    } catch (failure) {
      setError((failure as Error).message);
    } finally {
      setStarting(false);
    }
  };
  const stop = async () => {
    operation.current++;
    setStopping(true);
    setError("");
    try {
      setState(await invoke<DesktopState>("harness_desktop_stop"));
    } catch (failure) {
      setError((failure as Error).message);
    } finally {
      setStopping(false);
    }
  };
  const restart = async () => {
    const current = ++operation.current;
    setStarting(true);
    setError("");
    try {
      // Do not launch another instance unless the owned process tree stopped.
      setState(await invoke<DesktopState>("harness_desktop_stop"));
      if (operation.current !== current) return;
      setState(await invoke<DesktopState>("harness_desktop_start"));
    } catch (failure) {
      setError((failure as Error).message);
    } finally {
      setStarting(false);
    }
  };
  return {
    state,
    starting,
    error: error || pollError,
    stopping,
    stop,
    restart,
    active,
    launch,
    supported: capability?.key === probeKey && capability.supported,
    probeFailed: capability?.key === probeKey && capability.failed,
  };
}

export function HarnessDesktopPanel({
  snapshot,
  busy,
  controller,
}: {
  snapshot: Snapshot;
  busy: boolean;
  controller: ReturnType<typeof useHarnessDesktop>;
}) {
  const { t } = useI18n();
  const { state, starting, stopping, error, active, launch, stop, restart } = controller;
  const preparing = starting || state.phase === "preparing";
  const startupPending =
    preparing || (state.phase === "launched" && (!state.audit || state.audit.state === "checking"));
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!startupPending) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [startupPending]);
  const webActive = ["running", "starting", "stopping"].includes(
    snapshot.harnessRuntime?.harness?.state ?? "",
  );
  const labels = {
    idle: "Open your workspace in the official desktop app.",
    stopped: "Open your workspace in the official desktop app.",
    stopping: "Stopping Desktop and its background processes.",
    preparing: "Preparing local files. This may take a moment.",
    launched: "Continue in the desktop window. Close it to switch modes.",
    failed: "Desktop did not start. Check the error and try again.",
  };
  const failure = error || state.error;
  const audit = state.phase === "launched" ? (state.audit?.state ?? "checking") : undefined;
  const stageLabel =
    state.stage === "cleanup"
      ? t("Removing unused runtime copies")
      : state.stage === "runtime"
        ? t("Preparing offline dependencies")
        : state.stage === "project"
          ? t("Preparing the workspace")
          : state.stage === "check"
            ? t("Checking offline dependencies")
            : state.stage === "launch"
              ? t("Opening the official desktop app")
              : t("Checking local files");
  return (
    <div className="harness-mode-content harness-desktop-content">
      <div className="harness-status-copy">
        <strong className="desktop-phase" role="status">
          {state.phase === "stopping"
            ? t("Stopping")
            : starting || state.phase === "preparing"
              ? t("Preparing")
              : state.phase === "launched"
                ? t(
                    audit === "ready"
                      ? "Ready"
                      : audit === "failed"
                        ? "Client startup failed"
                        : audit === "unverified"
                          ? "Startup not yet verified"
                          : "Checking client",
                  )
                : state.phase === "failed"
                  ? t("Failed")
                  : t("Not running")}
        </strong>
        <p>
          {preparing
            ? stageLabel
            : audit === "failed"
              ? t(
                  "Repair the desktop profile using the official recovery window, then restart Desktop. Web profile changes do not repair this profile.",
                )
              : audit === "checking"
                ? t("Checking official Desktop startup and client activation.")
                : audit === "unverified"
                  ? t(
                      "Desktop is running, but startup verification did not finish. Review the startup details before retrying.",
                    )
                  : t(labels[state.phase])}
        </p>
        {startupPending && typeof state.startedAt === "number" && (
          <small>
            {t("Elapsed time: {seconds}s", {
              seconds: Math.max(0, Math.floor((now - state.startedAt) / 1000)),
            })}
          </small>
        )}
      </div>
      {failure && (
        <p className="error-text" role="alert">
          {t(failure)}
        </p>
      )}
      <div className="button-row">
        {state.phase === "launched" && !starting && (
          <>
            <ActionButton disabled={busy || stopping} onClick={() => void restart()}>
              <ArrowsClockwise size={16} />
              {t("Restart")}
            </ActionButton>
            <ActionButton tone="danger" disabled={stopping} onClick={() => void stop()}>
              <StopCircle size={16} />
              {t(stopping ? "Stopping" : "Close")}
            </ActionButton>
          </>
        )}
        {preparing || state.phase === "stopping" ? (
          <ActionButton
            onClick={() => void stop()}
            disabled={stopping || state.phase === "stopping"}
          >
            {stopping || state.phase === "stopping" ? t("Stopping") : t("Cancel startup")}
          </ActionButton>
        ) : (
          state.phase !== "launched" && (
            <ActionButton
              tone="primary"
              onClick={() => void launch()}
              disabled={busy || starting || active || webActive || !snapshot.startup?.available}
            >
              {t(state.phase === "failed" ? "Retry Desktop" : "Start Harness")}
              {!active && !starting && <ArrowRight size={16} />}
            </ActionButton>
          )
        )}
        {webActive && <span>{t("Stop Harness Web before launching Desktop.")}</span>}
      </div>
      {((state.phase === "failed" && state.detail) || state.audit?.error) && (
        <details>
          <summary>{t("Error details")}</summary>
          <pre>{state.audit?.error || state.detail}</pre>
        </details>
      )}
    </div>
  );
}
