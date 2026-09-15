import { type RuntimeToolSource, type JsonObject } from "./app-types";
import { asObject, arrayValue, isObject } from "./json-values";
import {
  errorMessage,
  runtimeToolSourceLabel,
  runtimeToolReason,
  localizeBackendError,
  compactError,
} from "./display-format";
import { type Translator, useI18n } from "./i18n";
import { StatusPill, EmptyState, ActionButton, Panel } from "./ui-components";
import { Pulse, WarningCircle, ArrowClockwise, Cpu } from "@phosphor-icons/react";

const runtimeToolNames = ["git", "node", "pnpm"] as const;

type RuntimeToolName = (typeof runtimeToolNames)[number];

export type RuntimeToolStatus = {
  name: RuntimeToolName;
  available: boolean;
  version?: string;
  source?: RuntimeToolSource;
  path?: string;
  reason?: string;
};

export type RuntimeStatusPayload = {
  api_version: string | number;
  tools: RuntimeToolStatus[];
};

export type RuntimeStatusViewState = {
  phase: "idle" | "loading" | "success" | "error";
  status: RuntimeStatusPayload | null;
  error: string | null;
};

export type RuntimeStatusPanelProps = {
  agentAvailable: boolean;
  state: RuntimeStatusViewState;
  onCheck: () => void;
};

export type RuntimeStatusTransport = (path: string, method: "GET") => Promise<unknown>;

export type RuntimeStatusController = {
  getState: () => RuntimeStatusViewState;
  check: (agentAvailable: boolean) => Promise<RuntimeStatusViewState>;
};

function isRuntimeToolName(value: string | undefined): value is RuntimeToolName {
  return value !== undefined && runtimeToolNames.includes(value as RuntimeToolName);
}

function isAbsoluteRuntimePath(value: string): boolean {
  return (
    value.startsWith("/") || /^\\\\[^\\/]+[\\/][^\\/]+/.test(value) || /^[A-Za-z]:[\\/]/.test(value)
  );
}

function optionalRuntimeString(item: JsonObject, key: string): string | undefined {
  const value = item[key];
  if (value === undefined) return undefined;
  if (typeof value !== "string" || !value.trim()) {
    throw new Error("Runtime status response is invalid.");
  }
  return value;
}

export function runtimeStatusFromResponse(value: unknown): RuntimeStatusPayload {
  const response = asObject(value);
  const apiVersion = response.api_version;
  if (
    (typeof apiVersion !== "string" || !apiVersion.trim()) &&
    (typeof apiVersion !== "number" || !Number.isFinite(apiVersion))
  ) {
    throw new Error("Runtime status response is invalid.");
  }

  const rawTools = arrayValue(response, "tools");
  if (rawTools.length !== runtimeToolNames.length) {
    throw new Error("Runtime status response is invalid.");
  }
  const tools = new Map<RuntimeToolName, RuntimeToolStatus>();
  for (const item of rawTools) {
    if (!isObject(item)) throw new Error("Runtime status response is invalid.");
    const name = item.name;
    if (
      typeof name !== "string" ||
      !isRuntimeToolName(name) ||
      tools.has(name) ||
      typeof item.available !== "boolean"
    ) {
      throw new Error("Runtime status response is invalid.");
    }
    const version = optionalRuntimeString(item, "version");
    const sourceValue = optionalRuntimeString(item, "source");
    const path = optionalRuntimeString(item, "path");
    const reason = optionalRuntimeString(item, "reason");
    const source =
      sourceValue === "system" || sourceValue === "nexus" || sourceValue === "bundled"
        ? sourceValue
        : undefined;
    if (sourceValue !== undefined && source === undefined) {
      throw new Error("Runtime status response is invalid.");
    }
    if (path !== undefined && !isAbsoluteRuntimePath(path)) {
      throw new Error("Runtime status response is invalid.");
    }
    if (item.available && (version === undefined || source === undefined || path === undefined)) {
      throw new Error("Runtime status response is invalid.");
    }
    tools.set(name, { name, available: item.available, version, source, path, reason });
  }
  if (tools.size !== runtimeToolNames.length) {
    throw new Error("Runtime status response is invalid.");
  }
  return {
    api_version: apiVersion,
    tools: runtimeToolNames.map((name) => tools.get(name) as RuntimeToolStatus),
  };
}

export function createRuntimeStatusController(
  transport: RuntimeStatusTransport,
  publish: (state: RuntimeStatusViewState) => void = () => undefined,
): RuntimeStatusController {
  let state: RuntimeStatusViewState = { phase: "idle", status: null, error: null };
  const update = (next: RuntimeStatusViewState): RuntimeStatusViewState => {
    state = next;
    publish(state);
    return state;
  };
  return {
    getState: () => state,
    check: async (agentAvailable) => {
      if (!agentAvailable) return state;
      update({ phase: "loading", status: null, error: null });
      try {
        const response = await transport("/v1/runtime", "GET");
        return update({
          phase: "success",
          status: runtimeStatusFromResponse(response),
          error: null,
        });
      } catch (cause) {
        return update({ phase: "error", status: null, error: errorMessage(cause) });
      }
    },
  };
}

function runtimeToolLabel(name: RuntimeToolName, t: Translator): string {
  return t(name === "git" ? "Git" : name === "node" ? "Node" : "pnpm");
}

function RuntimeToolRow({ tool }: { tool: RuntimeToolStatus }) {
  const { t } = useI18n();
  return (
    <div className="data-row">
      <div>
        <strong>{runtimeToolLabel(tool.name, t)}</strong>
        <StatusPill
          label={tool.available ? t("Available") : t("Unavailable")}
          tone={tool.available ? "good" : "warn"}
        />
      </div>
      <div>
        {tool.version && (
          <span>
            {t("Version")}: <code>{tool.version}</code>
          </span>
        )}
        {tool.source && (
          <span>
            {t("Source")}: <code>{runtimeToolSourceLabel(tool.source, t)}</code>
          </span>
        )}
        {tool.path && (
          <span>
            {t("Path")}: <code>{tool.path}</code>
          </span>
        )}
        {!tool.available && <span>{runtimeToolReason(tool.reason, t)}</span>}
      </div>
    </div>
  );
}

export function RuntimeStatusPanel({ agentAvailable, state, onCheck }: RuntimeStatusPanelProps) {
  const { t } = useI18n();
  const checkDisabled = !agentAvailable || state.phase === "loading";
  let content: React.ReactNode;
  if (!agentAvailable) {
    content = (
      <EmptyState
        title={t("Agent unavailable")}
        detail={t("The Agent is unavailable. Reconnect the Agent before checking runtime status.")}
      />
    );
  } else if (state.phase === "idle") {
    content = (
      <EmptyState
        title={t("Runtime status not checked")}
        detail={t("Click Check runtime to inspect Git, Node, and pnpm.")}
      />
    );
  } else if (state.phase === "loading") {
    content = (
      <div className="state-card loading-state" role="status" aria-live="polite">
        <Pulse size={22} className="spin" aria-hidden="true" />
        <div>
          <strong>{t("Checking runtime...")}</strong>
          <span>{t("Reading the Agent runtime status.")}</span>
        </div>
      </div>
    );
  } else if (state.phase === "error") {
    const message = state.error ? localizeBackendError(state.error, t) : t("Unknown");
    content = (
      <div className="state-card error-state" role="alert">
        <WarningCircle size={25} aria-hidden="true" />
        <div className="state-copy">
          <strong>{t("Runtime status unavailable")}</strong>
          <span>
            {t("Runtime status request failed: {message}", { message: compactError(message) })}
          </span>
        </div>
        <ActionButton onClick={onCheck}>
          <ArrowClockwise size={16} />
          {t("Retry")}
        </ActionButton>
      </div>
    );
  } else if (state.status) {
    content = state.status.tools.length ? (
      <div className="data-list" aria-label={t("Runtime tools")}>
        {state.status.tools.map((tool) => (
          <RuntimeToolRow key={tool.name} tool={tool} />
        ))}
      </div>
    ) : (
      <EmptyState
        title={t("No runtime tools reported")}
        detail={t("The Agent returned no tool entries to display.")}
      />
    );
  } else {
    content = (
      <EmptyState
        title={t("Runtime status unavailable")}
        detail={t("This runtime could not be verified.")}
      />
    );
  }

  return (
    <Panel title={t("Runtime status")} icon={<Cpu size={18} />}>
      <p className="panel-description">
        {t("Runtime status is checked manually. It never downloads or installs tools.")}
      </p>
      <div className="panel-toolbar">
        <span className="toolbar-count">
          {state.phase === "success" && state.status
            ? t("API {version}", { version: String(state.status.api_version) })
            : t("Manual check")}
        </span>
        <ActionButton disabled={checkDisabled} onClick={onCheck}>
          {state.phase === "loading" ? (
            <Pulse size={16} className="spin" />
          ) : (
            <ArrowClockwise size={16} />
          )}
          {state.phase === "loading"
            ? t("Checking runtime...")
            : state.phase === "success"
              ? t("Refresh runtime status")
              : t("Check runtime")}
        </ActionButton>
      </div>
      {content}
    </Panel>
  );
}
