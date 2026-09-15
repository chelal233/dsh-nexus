import { type JsonObject } from "./app-types";
import {
  nestedValue,
  arrayValue,
  stringValue,
  numberValue,
  booleanValue,
  asObject,
  isObject,
} from "./json-values";

type HarnessLaunchMode = "direct" | "node";

export function isLoopbackReadinessTarget(value: string | undefined): value is string {
  if (!value) return false;
  try {
    const url = new URL(value);
    if (
      !["http:", "tcp:"].includes(url.protocol) ||
      !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) ||
      url.username ||
      url.password ||
      /[\u0000-\u001f\u007f]/.test(value)
    )
      return false;
    const port = Number(url.port || (url.protocol === "http:" ? "80" : "0"));
    if (!Number.isInteger(port) || port <= 0 || port > 65535) return false;
    if (url.protocol === "tcp:") {
      return (
        Boolean(url.port) &&
        (url.pathname === "" || url.pathname === "/") &&
        !url.search &&
        !url.hash
      );
    }
    return !url.hash;
  } catch {
    return false;
  }
}

export function isLoopbackUrl(value: string | undefined): value is string {
  if (!value) return false;
  try {
    const url = new URL(value);
    return (
      url.protocol === "http:" &&
      ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) &&
      !url.username &&
      !url.password &&
      !/[\u0000-\u001f\u007f]/.test(value)
    );
  } catch {
    return false;
  }
}

export type HarnessConfigDraft = {
  mode: HarnessLaunchMode;
  program: string;
  entry: string;
  args: string;
  workingDir: string;
  readinessUrl: string;
  timeout: string;
  readinessTokenRequired: boolean;
  readinessUrlRedacted: boolean;
  argsRedacted: boolean;
  replaceRedactedArgs: boolean;
};

export const emptyHarnessDraft: HarnessConfigDraft = {
  mode: "direct",
  program: "",
  entry: "",
  args: "",
  workingDir: "",
  readinessUrl: "",
  timeout: "",
  readinessTokenRequired: false,
  readinessUrlRedacted: false,
  argsRedacted: false,
  replaceRedactedArgs: false,
};

export function harnessDraftFromConfig(config: JsonObject): HarnessConfigDraft {
  const harness = nestedValue(config, "harness");
  const args = arrayValue(harness, "args").filter(
    (item): item is string => typeof item === "string",
  );
  const argsRedacted = args.some((item) => item.includes("[REDACTED]"));
  const mode = harnessLaunchMode(harness);
  const configuredEntry = stringValue(harness, "entry") || "";
  const entry = mode === "node" ? configuredEntry || args[0] || "" : "";
  const visibleArgs = mode === "node" && !configuredEntry ? args.slice(1) : args;
  return {
    mode,
    program: stringValue(harness, "program") || "",
    entry,
    args: visibleArgs.join("\n"),
    workingDir: stringValue(harness, "working_dir") || "",
    readinessUrl: stringValue(harness, "readiness_url") || "",
    timeout: numberValue(harness, "readiness_timeout_secs")?.toString() || "",
    readinessTokenRequired: booleanValue(harness, "readiness_token_required"),
    readinessUrlRedacted: booleanValue(config, "harness_readiness_url_redacted"),
    argsRedacted,
    replaceRedactedArgs: false,
  };
}

export function harnessLaunchMode(value: unknown): HarnessLaunchMode {
  const mode = stringValue(value, "mode")?.toLowerCase();
  return mode === "node" ? "node" : "direct";
}

export type HarnessCandidate = {
  id: string;
  mode: HarnessLaunchMode;
  program: string;
  entry: string;
  args: string[];
  workingDir: string;
  readinessUrl: string;
  readinessTimeout: string;
  readinessTokenRequired: boolean;
  version: string;
  source: string;
  displayName: string;
};

export function harnessCandidates(value: unknown): HarnessCandidate[] {
  const response = asObject(value);
  const nested = nestedValue(response, "harness");
  const items = arrayValue(response, "candidates").length
    ? arrayValue(response, "candidates")
    : arrayValue(nested, "candidates");
  return items
    .filter(isObject)
    .map((item, index) => {
      const rawMode =
        stringValue(item, "mode") || stringValue(item, "kind") || stringValue(item, "type");
      const mode: HarnessLaunchMode = rawMode?.toLowerCase().includes("node") ? "node" : "direct";
      const rawArgs = arrayValue(item, "args").filter(
        (arg): arg is string => typeof arg === "string",
      );
      const configuredEntry = stringValue(item, "entry") || stringValue(item, "entry_point") || "";
      const entry = mode === "node" ? configuredEntry || rawArgs[0] || "" : "";
      const args = mode === "node" && !configuredEntry ? rawArgs.slice(1) : rawArgs;
      const program =
        stringValue(item, "program") ||
        stringValue(item, "executable") ||
        stringValue(item, "node_executable") ||
        "";
      const workingDir = stringValue(item, "working_dir") || stringValue(item, "project_dir") || "";
      const readinessUrl = stringValue(item, "readiness_url") || "";
      const readinessTimeout = numberValue(item, "readiness_timeout_secs")?.toString() || "";
      const readinessTokenRequired = booleanValue(item, "readiness_token_required");
      const id = stringValue(item, "id") || `${mode}:${program}:${entry}:${index}`;
      return {
        id,
        mode,
        program,
        entry,
        args,
        workingDir,
        readinessUrl,
        readinessTimeout,
        readinessTokenRequired,
        version: stringValue(item, "version") || "",
        source: stringValue(item, "source") || "",
        displayName:
          stringValue(item, "display_name") || stringValue(item, "name") || program || id,
      };
    })
    .filter((candidate) => candidate.program.length > 0);
}

/** Serialize the editor's split Node fields at the compatibility boundary. */
export function harnessConfigPayloadFromDraft(draft: HarnessConfigDraft): JsonObject {
  const entry = draft.entry.trim();
  const additionalArgs = draft.args
    .split(/\r?\n/)
    .map((value) => value.trim())
    .filter(Boolean);
  return {
    mode: draft.mode,
    program: draft.program.trim(),
    entry: draft.mode === "node" ? entry : null,
    args: additionalArgs,
    args_are_additional: draft.mode === "node",
    working_dir: draft.workingDir.trim() || null,
    readiness_url: draft.readinessUrl.trim() || null,
    readiness_timeout_secs: draft.timeout.trim() ? Number(draft.timeout.trim()) : null,
    readiness_token_required: draft.readinessTokenRequired,
  };
}
