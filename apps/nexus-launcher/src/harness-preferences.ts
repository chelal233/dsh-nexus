export const preferenceTextFields = [
  "home", "deepseek_base_url", "search_base_url", "search_provider", "fetch_provider",
  "agents_home", "bundled_skill_dir", "permission_mode", "tools_mode", "system_prompt",
] as const;
export const preferenceBooleanFields = ["open_browser", "telemetry_disabled", "max_tokens_as_success"] as const;
export type HarnessPreferencesDraft = Record<typeof preferenceTextFields[number] | typeof preferenceBooleanFields[number] | "port" | "context_window" | "patches", string>;

export function preferencesDraft(value: Record<string, unknown>): HarnessPreferencesDraft {
  const draft = {} as HarnessPreferencesDraft;
  for (const key of preferenceTextFields) draft[key] = typeof value[key] === "string" ? value[key] as string : "";
  for (const key of preferenceBooleanFields) draft[key] = typeof value[key] === "boolean" ? String(value[key]) : "";
  for (const key of ["port", "context_window"] as const) draft[key] = typeof value[key] === "number" ? String(value[key]) : "";
  draft.patches = Array.isArray(value.patches) ? value.patches.filter(item => typeof item === "string").join("\n") : "";
  return draft;
}

/** Empty fields remove saved overrides; false and port zero remain explicit. */
export function preferencesPayload(draft: HarnessPreferencesDraft): { value: Record<string, unknown>; error?: string } {
  const value: Record<string, unknown> = {};
  for (const key of preferenceTextFields) {
    const text = draft[key].trim();
    if (text) value[key] = text;
  }
  for (const key of preferenceBooleanFields) if (draft[key] !== "") value[key] = draft[key] === "true";
  for (const key of ["port", "context_window"] as const) {
    const text = draft[key].trim();
    if (!text) continue;
    const number = Number(text);
    if (!/^\d+$/.test(text) || !Number.isSafeInteger(number) || number < (key === "port" ? 0 : 1) || (key === "port" && number > 65535)) {
      return { value: {}, error: key === "port" ? "Port must be an integer from 0 to 65535." : "Context window must be a positive integer." };
    }
    value[key] = number;
  }
  const patches = draft.patches.split(/\r?\n/).map(path => path.trim()).filter(Boolean);
  if (patches.length) value.patches = patches;
  return { value };
}
