import { useRef, useState } from "react";
import { type DesktopUpdate } from "./desktop-update";
import { invoke } from "./desktop";
import { useI18n } from "./i18n";
import { Modal, ActionButton } from "./ui-components";

export function DesktopUpdateDialog({
  state,
  needsHarnessStop,
  onClose,
}: {
  state: DesktopUpdate;
  needsHarnessStop: boolean;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const pending = useRef(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const downloading = state.phase === "downloading";
  const ready = state.phase === "ready";
  const installing = state.phase === "installing";
  const percent = Math.min(100, Math.max(0, Math.floor(state.percent ?? 0)));
  async function perform(command: "update_download" | "update_check" | "update_install") {
    if (pending.current) return;
    pending.current = true;
    setBusy(true);
    setError("");
    try {
      await invoke(command, command === "update_download" ? { version: state.version } : {});
    } catch (cause) {
      setError((cause as Error).message || String(cause));
    } finally {
      pending.current = false;
      setBusy(false);
    }
  }
  return (
    <Modal title={t("Update Nexus")} locked={installing} onClose={onClose}>
      {state.version && (
        <p className="update-version">{t("New version: {version}", { version: state.version })}</p>
      )}
      <p role="status" aria-live="polite">
        {ready
          ? t("Download verified. Choose when to restart.")
          : installing
            ? t("Installing")
            : downloading
              ? t("Downloading {percent}%", { percent })
              : state.phase === "checking"
                ? t("Checking…")
                : state.phase === "error"
                  ? t("Update failed. Check again to retry.")
                  : state.phase === "idle"
                    ? t("No update available")
                    : t(
                        "Download starts only after you confirm. Nexus will not restart automatically.",
                      )}
      </p>
      {downloading && (
        <>
          <progress
            className="desktop-update-progress"
            max={100}
            value={percent}
            aria-label={t("Update download progress")}
          />
          <p className="field-help">
            {t(
              "Closing this dialog keeps your requested download running. Reopen Update to view progress.",
            )}
          </p>
        </>
      )}
      {ready && (
        <p className="field-help">
          {t("Save your work and stop Harness before restarting Nexus.")}
        </p>
      )}
      {ready && needsHarnessStop && (
        <p className="form-error">
          {t("Stop Harness before updating; running tasks will be interrupted.")}
        </p>
      )}
      {(error || state.error) && (
        <p className="form-error" role="alert">
          {error || state.error}
        </p>
      )}
      <div className="button-row">
        {!installing && (
          <ActionButton onClick={onClose}>
            {ready ? t("Restart later") : downloading ? t("Close") : t("Not now")}
          </ActionButton>
        )}
        {state.phase === "available" && (
          <ActionButton
            tone="primary"
            disabled={busy}
            onClick={() => void perform("update_download")}
          >
            {t("Confirm and download")}
          </ActionButton>
        )}
        {state.phase === "error" && (
          <ActionButton tone="primary" disabled={busy} onClick={() => void perform("update_check")}>
            {t("Check again")}
          </ActionButton>
        )}
        {ready && (
          <ActionButton
            tone="primary"
            disabled={busy || needsHarnessStop}
            onClick={() => void perform("update_install")}
          >
            {t("Update and restart")}
          </ActionButton>
        )}
      </div>
    </Modal>
  );
}
