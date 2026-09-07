export type EditableDraft<T> = { value: T; dirty: boolean };

export function homePreferencesPayload(saved: Record<string, unknown>, value: string): Record<string, unknown> {
  return { ...saved, home: value.trim() || null };
}

export function launchInputMatches(record: Record<string, unknown>, response: Record<string, unknown>): boolean {
  const runtime = response.harness as Record<string, unknown> | undefined;
  return runtime?.state === "running" && typeof record.run_id === "string" && record.run_id.length > 0
    && record.run_id === response.log_session_run_id && typeof record.generation === "number"
    && record.generation === response.generation;
}

export function diagnosticExportResult(response: Record<string, unknown>): { path: string; manual: boolean } {
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
  const runtime = response.harness && typeof response.harness === "object"
    ? response.harness as Record<string, unknown> : response;
  if (runtime.state !== "failed") return [];
  return [response.log_session_run_id
    ? `${response.generation ?? ""}:${response.log_session_run_id}`
    : `${response.generation ?? ""}:${runtime.started_at_unix ?? ""}:${runtime.updated_at_unix ?? ""}`];
}

export function updateSourcePayload(source: string): Record<string, unknown> {
  // The Agent merges only this field with its current private configuration.
  return { source: source.trim() };
}

export function offlineArchivePathValid(value: string): boolean {
  const path = value.trim();
  return !/[\r\n\0]/.test(path) && /\.tar\.gz$/i.test(path)
    && (/^[a-z]:[\\/]/i.test(path) || /^\\\\[^\\/]+[\\/][^\\/]+[\\/]/.test(path));
}

export function offlinePackageCommand(action: "offline_import" | "offline_export", path: string, releaseId?: string): Record<string, unknown> {
  return { action, archive_path: path.trim(), ...(action === "offline_export" ? { release_id: releaseId } : {}) };
}
