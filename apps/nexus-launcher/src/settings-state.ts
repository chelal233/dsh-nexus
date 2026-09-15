export type EditableDraft<T> = { value: T; dirty: boolean };

export type CleanupSelection = { previewId: string; ids: string[] };

/** Item IDs are only meaningful within the preview that supplied them. */
export function cleanupSelectedIds(selection: CleanupSelection, previewId: unknown): string[] {
  return typeof previewId === "string" && previewId.length > 0 && selection.previewId === previewId
    ? selection.ids
    : [];
}

export function homePreferencesPayload(
  saved: Record<string, unknown>,
  value: string,
): Record<string, unknown> {
  return { ...saved, home: value.trim() || null };
}

export function launchInputMatches(
  record: Record<string, unknown>,
  response: Record<string, unknown>,
): boolean {
  const runtime = response.harness as Record<string, unknown> | undefined;
  return (
    runtime?.state === "running" &&
    typeof record.run_id === "string" &&
    record.run_id.length > 0 &&
    record.run_id === response.log_session_run_id &&
    typeof record.generation === "number" &&
    record.generation === response.generation
  );
}

export function diagnosticExportResult(response: Record<string, unknown>): {
  path: string;
  manual: boolean;
} {
  const path = response.export_path;
  if (typeof path !== "string" || !path.trim() || /[\r\n\0]/.test(path)) {
    throw new Error("Diagnostic export returned no usable file path.");
  }
  return { path, manual: !!response.reveal_error };
}

export function refreshEditableDraft<T>(draft: EditableDraft<T>, saved: T): EditableDraft<T> {
  return draft.dirty ? draft : { value: saved, dirty: false };
}

export function finishDraftSave<T>(draft: EditableDraft<T>, succeeded: boolean): EditableDraft<T> {
  return succeeded ? { ...draft, dirty: false } : draft;
}

export type ArgumentRow = { key: string; value: string };
export function replacementArgumentRows(rows: ArgumentRow[], enabled: boolean): ArgumentRow[] {
  // Hidden values cannot be reconstructed. Replacement always starts a complete new list.
  return enabled ? [] : rows;
}

/** Read the nested Harness contract; session identity stays outside its state object. */
export function harnessFailureKeys(response: Record<string, unknown>): string[] {
  const runtime =
    response.harness && typeof response.harness === "object"
      ? (response.harness as Record<string, unknown>)
      : response;
  if (runtime.state !== "failed") return [];
  return [
    response.log_session_run_id
      ? `${response.generation ?? ""}:${response.log_session_run_id}`
      : `${response.generation ?? ""}:${runtime.started_at_unix ?? ""}:${runtime.updated_at_unix ?? ""}`,
  ];
}

export function updateSourcePayload(source: string): Record<string, unknown> {
  // The Agent merges only this field with its current private configuration.
  return { source: source.trim() };
}

export function offlineArchivePathValid(value: string): boolean {
  const path = value.trim();
  return (
    !/[\r\n\0]/.test(path) &&
    /\.tar\.gz$/i.test(path) &&
    (/^[a-z]:[\\/]/i.test(path) || /^\\\\[^\\/]+[\\/][^\\/]+[\\/]/.test(path))
  );
}

export function offlinePackageCommand(
  action: "offline_import" | "offline_export",
  path: string,
  releaseId?: string,
): Record<string, unknown> {
  return {
    action,
    archive_path: path.trim(),
    ...(action === "offline_export" ? { release_id: releaseId } : {}),
  };
}

export function offlineImportDefaults(data: Record<string, unknown> | null | undefined) {
  return {
    runtime: data?.runtime !== false,
    profiles: Array.isArray(data?.profiles) ? data.profiles.map(String) : [],
    configuration: data?.configuration === true,
    environment: (data?.environment ?? data?.configuration) === true,
    sessions: data?.sessions === true,
    plugins: data?.plugins === true,
    credentials: false,
    credential_policy: "preserve" as "preserve" | "replace",
  };
}

export function releasePromotionCommand(
  id: string,
  confirmation: string | null,
  accepted: boolean,
): Record<string, unknown> | null {
  if (confirmation && !accepted) return null;
  return {
    action: "promote",
    id,
    ...(confirmation ? { rollback_confirmation: confirmation } : {}),
  };
}

/** Display grouping only; backend eligibility and preview identity stay authoritative. */
export function cleanupGroups(areas: Record<string, unknown>[], items: Record<string, unknown>[]) {
  const normalize = (value: unknown) =>
    String(value ?? "")
      .replace(/\\/g, "/")
      .replace(/\/+$/, "")
      .toLowerCase();
  const groups = areas.map((area) => ({ area, items: [] as Record<string, unknown>[] }));
  const other: Record<string, unknown>[] = [];
  for (const item of items) {
    const path = normalize(item.path);
    const candidates = groups.filter((group) => {
      const root = normalize(group.area.path);
      return root && (path === root || path.startsWith(root + "/"));
    });
    candidates.sort((a, b) => normalize(b.area.path).length - normalize(a.area.path).length);
    if (candidates.length) candidates[0].items.push(item);
    else other.push(item);
  }
  if (other.length) groups.push({ area: { kind: "Other cleanup items", path: "" }, items: other });
  return groups;
}

export function toggleCleanupGroup(
  selected: string[],
  items: Record<string, unknown>[],
  checked: boolean,
): string[] {
  const eligible = items
    .filter((item) => item.eligible === true && typeof item.id === "string")
    .map((item) => String(item.id));
  return checked
    ? [...new Set([...selected, ...eligible])]
    : selected.filter((id) => !eligible.includes(id));
}
