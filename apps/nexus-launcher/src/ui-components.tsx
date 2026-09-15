import { useI18n } from "./i18n";
import { Pulse, BracketsCurly, WarningCircle, ArrowClockwise, X } from "@phosphor-icons/react";
import { localizeBackendError, compactError, errorMessage } from "./display-format";
import { type JsonObject } from "./app-types";
import { useRef, useEffect, useState } from "react";
import { isBrowserPreview } from "./agent-bridge";
import { invoke } from "@tauri-apps/api/core";

export function StatusPill({
  label,
  tone = "neutral",
}: {
  label: string;
  tone?: "good" | "warn" | "bad" | "neutral";
}) {
  return (
    <span className={`status-pill ${tone}`}>
      <span className="status-dot" />
      {label}
    </span>
  );
}

export function LoadingState() {
  const { t } = useI18n();
  return (
    <div className="state-card loading-state" role="status" aria-live="polite">
      <Pulse size={22} className="spin" aria-hidden="true" />
      <div>
        <strong>{t("Connecting to Nexus")}</strong>
        <span>{t("Waiting for the local control plane.")}</span>
      </div>
    </div>
  );
}

export function EmptyState({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="state-card empty-state">
      <BracketsCurly size={24} aria-hidden="true" />
      <div>
        <strong>{title}</strong>
        <span>{detail}</span>
      </div>
    </div>
  );
}

export function ErrorState({
  message,
  onRetry,
  title,
}: {
  message: string;
  onRetry: () => void;
  title?: string;
}) {
  const { t } = useI18n();
  return (
    <div className="state-card error-state" role="alert">
      <WarningCircle size={25} aria-hidden="true" />
      <div className="state-copy">
        <strong>{title || t("Launcher bridge unavailable")}</strong>
        <span>{localizeBackendError(message, t)}</span>
      </div>
      <button className="button subtle" onClick={onRetry}>
        <ArrowClockwise size={16} />
        {t("Retry")}
      </button>
    </div>
  );
}

export function DegradedNotice({
  errors,
  readOnlyRecovery = false,
}: {
  errors: Record<string, string>;
  readOnlyRecovery?: boolean;
}) {
  const { t } = useI18n();
  const details = Object.entries(errors)
    .filter(([, message]) => !readOnlyRecovery || !message.includes("Read-only recovery:"))
    .map(([path, message]) => `${path}: ${compactError(localizeBackendError(message, t))}`)
    .join(" | ");
  if (!details) return null;
  return (
    <div className="notice degraded" role="status" aria-live="polite">
      <WarningCircle size={17} />{" "}
      <span>
        {t("Some workspace data is unavailable.")} {details}
      </span>
    </div>
  );
}

export function AgentUnavailableNotice({
  message,
  onRetry,
}: {
  message: string;
  onRetry: () => void;
}) {
  const { t } = useI18n();
  return (
    <div className="notice action-error" role="status" aria-live="polite">
      <WarningCircle size={17} />
      <span>
        <strong>{t("Agent unavailable")}</strong> {localizeBackendError(message, t)}
      </span>
      <button className="button subtle" onClick={onRetry}>
        {t("Retry")}
      </button>
    </div>
  );
}

export function MissingReleaseNotice({
  releases,
  onReinstall,
}: {
  releases: JsonObject;
  onReinstall: () => void;
}) {
  const { t } = useI18n();
  const missing = releases.unavailable_selections;
  if (!Array.isArray(missing) || missing.length === 0) return null;
  return (
    <div className="notice action-error" role="alert">
      <WarningCircle size={17} />
      <span>
        <strong>{t("Harness installation is incomplete")}</strong>{" "}
        {t(
          "Agent is available. Reinstall Harness from the setup guide; existing data and remaining files are preserved.",
        )}
        <small>{missing.filter((id): id is string => typeof id === "string").join(", ")}</small>
      </span>
      <button className="button subtle" onClick={onReinstall}>
        {t("Reinstall Harness")}
      </button>
    </div>
  );
}

export function Modal({
  title,
  onClose,
  children,
  locked = false,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
  locked?: boolean;
}) {
  const dialog = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    dialog.current?.focus();
    return () => previous?.focus();
  }, []);
  return (
    <div
      className="modal-overlay"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      onClick={() => {
        if (!locked) onClose();
      }}
    >
      <div
        className="modal-card"
        ref={dialog}
        tabIndex={-1}
        onClick={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            if (!locked) onClose();
          }
          if (event.key === "Tab") {
            const items = [
              ...(dialog.current?.querySelectorAll<HTMLElement>(
                'button:not(:disabled), input:not(:disabled), select:not(:disabled), [tabindex="0"]',
              ) || []),
            ];
            const first = items[0],
              last = items.at(-1);
            if (!first) {
              event.preventDefault();
              return;
            }
            if (
              event.shiftKey &&
              (document.activeElement === first || document.activeElement === dialog.current)
            ) {
              event.preventDefault();
              last?.focus();
            } else if (
              !event.shiftKey &&
              (document.activeElement === last || document.activeElement === dialog.current)
            ) {
              event.preventDefault();
              first.focus();
            }
          }
        }}
      >
        <div className="modal-header">
          <strong>{title}</strong>
          {!locked && (
            <ActionButton onClick={onClose}>
              <X size={16} />
            </ActionButton>
          )}
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}

export function Metric({
  label,
  value,
  detail,
  actions,
  children,
}: {
  label: React.ReactNode;
  value: string;
  detail?: string;
  actions?: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <div className="metric">
      <span>{label}</span>
      <strong>{value}</strong>
      {detail && <small>{detail}</small>}
      {actions && <div className="metric-actions">{actions}</div>}
      {children}
    </div>
  );
}

export function ActionButton({
  children,
  onClick,
  disabled = false,
  tone = "default",
  title,
}: {
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
  tone?: "default" | "primary" | "danger";
  title?: string;
}) {
  return (
    <button
      type="button"
      className={`button ${tone}`}
      onClick={onClick}
      disabled={disabled}
      title={title}
    >
      {children}
    </button>
  );
}

export function DataList({
  items,
  emptyTitle,
  emptyDetail,
  render,
}: {
  items: unknown[];
  emptyTitle: string;
  emptyDetail: string;
  render: (item: unknown, index: number) => React.ReactNode;
}) {
  if (!items.length) return <EmptyState title={emptyTitle} detail={emptyDetail} />;
  return (
    <div className="data-list">
      {items.map((item, index) => (
        <div className="data-row" key={index}>
          {render(item, index)}
        </div>
      ))}
    </div>
  );
}

export function PathInput({
  value,
  onChange,
  disabled,
  directory = false,
  save = false,
  archive = false,
  placeholder,
}: {
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
  directory?: boolean;
  save?: boolean;
  archive?: boolean;
  placeholder?: string;
}) {
  const { t } = useI18n();
  const [choosing, setChoosing] = useState(false);
  const [error, setError] = useState("");

  const choose = async () => {
    setChoosing(true);
    setError("");
    try {
      const path = await invoke<string | null>("choose_local_path", { directory, save, archive });
      if (path) onChange(path);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setChoosing(false);
    }
  };

  const inputDisabled = disabled || choosing;
  const label = save ? "Choose save location" : directory ? "Browse folder" : "Browse file";
  return (
    <>
      <div className="path-input">
        <input
          className="form-input"
          value={value}
          disabled={inputDisabled}
          placeholder={placeholder}
          onChange={(event) => onChange(event.target.value)}
        />
        <ActionButton disabled={inputDisabled || isBrowserPreview} onClick={() => void choose()}>
          {t(label)}
        </ActionButton>
      </div>
      {error && (
        <span className="form-error" role="alert">
          {error}
        </span>
      )}
    </>
  );
}

export function PageIntro({
  kicker,
  title,
  detail,
}: {
  kicker: string;
  title: string;
  detail: string;
}) {
  return (
    <div className="page-heading">
      <div>
        <span className="kicker">{kicker}</span>
        <h1>{title}</h1>
        <p>{detail}</p>
      </div>
    </div>
  );
}

export function Panel({
  title,
  icon,
  children,
}: {
  title: string;
  icon: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section className="panel">
      <div className="panel-header">
        <div className="panel-title">
          {icon}
          <h2>{title}</h2>
        </div>
      </div>
      {children}
    </section>
  );
}
