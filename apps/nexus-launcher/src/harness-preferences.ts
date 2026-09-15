export const preferenceTextFields = [
  "home",
  "deepseek_base_url",
  "search_base_url",
  "search_provider",
  "fetch_provider",
  "agents_home",
  "bundled_skill_dir",
  "permission_mode",
  "tools_mode",
  "system_prompt",
] as const;
export const preferenceBooleanFields = [
  "open_browser",
  "telemetry_disabled",
  "max_tokens_as_success",
] as const;
export type PatchEntry = {
  source: string;
  enabled: boolean;
  sha256?: string;
  github_ref_kind?: string;
  github_ref_name?: string;
  github_file_path?: string;
  resolved_commit?: string;
  cache_identity?: string;
};
export function githubRefKind(entry: PatchEntry): string {
  return (
    entry.github_ref_kind ??
    (/^[a-fA-F0-9]{40}$/.test(entry.source.split("/blob/")[1]?.split("/")[0] ?? "")
      ? "commit"
      : "branch")
  );
}
export type HarnessPreferencesDraft = Record<
  | (typeof preferenceTextFields)[number]
  | (typeof preferenceBooleanFields)[number]
  | "port"
  | "context_window"
  | "patches",
  string
> & { patch_entries: PatchEntry[] };

export function preferencesDraft(value: Record<string, unknown>): HarnessPreferencesDraft {
  const draft = {} as HarnessPreferencesDraft;
  for (const key of preferenceTextFields)
    draft[key] = typeof value[key] === "string" ? (value[key] as string) : "";
  for (const key of preferenceBooleanFields)
    draft[key] = typeof value[key] === "boolean" ? String(value[key]) : "";
  for (const key of ["port", "context_window"] as const)
    draft[key] = typeof value[key] === "number" ? String(value[key]) : "";
  draft.patches = Array.isArray(value.patches)
    ? value.patches.filter((item) => typeof item === "string").join("\n")
    : "";
  const entries = new Map<string, PatchEntry>();
  for (const source of draft.patches
    .split(/\r?\n/)
    .map((source) => source.trim())
    .filter(Boolean))
    entries.set(source, { source, enabled: true });
  for (const item of Array.isArray(value.patch_entries) ? value.patch_entries : []) {
    if (
      item &&
      typeof item === "object" &&
      typeof item.source === "string" &&
      typeof item.enabled === "boolean"
    ) {
      // Structured entries carry enabled state and verification metadata;
      // prefer them when the legacy list describes the same source.
      entries.set(item.source.trim(), { ...item, source: item.source.trim() });
    }
  }
  draft.patch_entries = [...entries.values()];
  return draft;
}

/** Empty fields remove saved overrides; false and port zero remain explicit. */
export function preferencesPayload(draft: HarnessPreferencesDraft): {
  value: Record<string, unknown>;
  error?: string;
} {
  const value: Record<string, unknown> = {};
  for (const key of preferenceTextFields) {
    const text = draft[key].trim();
    if (text) value[key] = text;
  }
  for (const key of preferenceBooleanFields)
    if (draft[key] !== "") value[key] = draft[key] === "true";
  for (const key of ["port", "context_window"] as const) {
    const text = draft[key].trim();
    if (!text) continue;
    const number = Number(text);
    if (
      !/^\d+$/.test(text) ||
      !Number.isSafeInteger(number) ||
      number < (key === "port" ? 0 : 1) ||
      (key === "port" && number > 65535)
    ) {
      return {
        value: {},
        error:
          key === "port"
            ? "Port must be an integer from 0 to 65535."
            : "Context window must be a positive integer.",
      };
    }
    value[key] = number;
  }
  const patches = draft.patches
    .split(/\r?\n/)
    .map((path) => path.trim())
    .filter(Boolean);
  // The legacy text draft remains accepted by CLI/tests; the editor saves its ordered list.
  if (draft.patch_entries.length) {
    const entries = draft.patch_entries
      .map((entry) => ({ ...entry, source: entry.source.trim() }))
      .filter((entry) => entry.source);
    if (entries.length) value.patch_entries = entries;
  } else if (patches.length) value.patches = patches;
  return { value };
}
