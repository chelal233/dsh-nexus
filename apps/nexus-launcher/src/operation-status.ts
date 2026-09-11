type ObjectValue = Record<string, unknown>;
const object = (value: unknown): ObjectValue => value && typeof value === "object" && !Array.isArray(value) ? value as ObjectValue : {};
const text = (value: unknown): string => typeof value === "string" ? value : "";
export type OperationSummary = { id: string; title: string; status: string; phase: string; error: string; progress?: number; time?: number; module: "guide" | "versions" | "profiles" | "maintenance"; anchor: string; archivePath?: string };

/** Only operations observed executing may announce completion; catalog hydration is not execution. */
export function advanceOperationNotices(records: OperationSummary[], previous: Map<string, boolean>) {
  const pending = new Map<string, boolean>();
  const notices: OperationSummary[] = [];
  for (const item of records) {
    const terminal = ["prepared", "succeeded", "completed", "failed", "cancelled", "interrupted"].includes(item.phase);
    let eligible = previous.get(item.id) === true || !terminal;
    if (eligible && terminal && ["Prepared; awaiting manual confirmation", "Completed", "Cancelled", "Failed", "Partially failed", "Interrupted"].includes(item.status)) {
      notices.push(item);
      eligible = false;
    }
    pending.set(item.id, eligible);
  }
  return { pending, notices };
}

export function releaseCatalogIsCurrent(snapshot: ObjectValue): boolean {
  return object(snapshot.startup).available === true && !snapshot.lifecycleBusy && !object(snapshot.endpointErrors)["/v1/releases"] && Array.isArray(object(snapshot.releases).releases);
}

/** Presentation only. Server records remain the sole execution and cancellation authority. */
export function operationSummaries(snapshot: ObjectValue): OperationSummary[] {
  const updates = object(snapshot.updates), checkpoints = object(snapshot.checkpoints);
  const records: OperationSummary[] = [];
  for (const [key, title] of [["install_operation", "Installation"], ["operation", "Cold switch"]]) {
    const item = object(updates[key]), phase = text(item.phase);
    if (!item.operation_id) continue;
    const offlineExport = item.kind === "offline_export", offlineImport = item.kind === "offline_import";
    const archivePath = text(item.archive_path);
    const operationTitle = offlineExport ? "Offline package export" : offlineImport ? "Offline package import" : title;
    const error = [text(item.error), text(item.cleanup_error)].filter(Boolean).join("\n");
    const catalog = object(snapshot.releases).releases;
    const verifiedCatalog = releaseCatalogIsCurrent(snapshot);
    const slotMissing = object(item.offline_contents).runtime !== false && verifiedCatalog && !(catalog as unknown[]).some(slot => object(slot).id === item.release_id);
    let status = item.cleanup_pending ? offlineExport && phase === "succeeded" ? "Package exported; cleanup required" : "Cleanup required" : phase === "failed" || error ? "Failed" : phase === "cancelled" ? "Cancelled" : phase === "succeeded" ? offlineExport ? archivePath ? "Completed" : "Status unavailable" : !verifiedCatalog ? "Verification pending" : slotMissing ? "Installed version unavailable" : "Completed" : phase === "awaiting_confirmation" ? "Confirmation required" : "In progress";
    if (phase === "prepared" && !item.cleanup_pending && !error) status = "Prepared; awaiting manual confirmation";
    records.push({ id: key + ":" + item.operation_id, title: operationTitle, status, phase, error, archivePath: archivePath || undefined, progress: status === "In progress" && typeof item.progress_percent === "number" ? Math.max(0, Math.min(100, item.progress_percent)) : undefined, time: typeof item.updated_at_unix === "number" ? item.updated_at_unix : undefined, module: "versions", anchor: offlineExport || offlineImport ? "offline-packages" : "installation-status" });
  }
  const restore = object(checkpoints.pending_restore ?? object(snapshot.recovery).pending_restore);
  if (restore.checkpoint_id) records.push({ id: "restore:" + restore.ticket_id, title: "Pending restore", status: "Recovery required", phase: text(restore.state), error: text(restore.error), module: "profiles", anchor: "restore-status" });
  const capture = object(checkpoints.last_capture);
  if (capture.state) records.push({ id: "capture:" + text(capture.id), title: capture.kind === "healthy" ? "Healthy snapshot capture" : "Snapshot capture", status: capture.state === "succeeded" ? "Completed" : capture.state === "running" ? "In progress" : capture.state === "interrupted" ? "Interrupted" : capture.state === "failed" ? "Failed" : "Status unavailable", phase: text(capture.state), error: [text(capture.error), text(capture.persistence_error)].filter(Boolean).join("\n"), time: typeof capture.updated_at_unix === "number" ? capture.updated_at_unix : undefined, module: "profiles", anchor: "restore-status" });
  const cleanup = object(object(snapshot.maintenance).result), items = Array.isArray(cleanup.items) ? cleanup.items.map(object) : [];
  if (cleanup.state) records.push({ id: "cleanup:" + cleanup.preview_id, title: "Data cleanup", status: cleanup.state === "interrupted" ? "Interrupted" : cleanup.state === "running" ? "In progress" : items.some(item => item.state === "failed") ? "Partially failed" : cleanup.state === "completed" ? "Completed" : "Status unavailable", phase: text(cleanup.state), error: items.filter(item => item.error).map(item => text(item.error)).join("\n"), time: typeof cleanup.started_at_unix === "number" ? cleanup.started_at_unix : undefined, module: "maintenance", anchor: "maintenance-cleanup" });
  const rank = (status: string) => ["Failed", "Recovery required", "Cleanup required", "Package exported; cleanup required", "Interrupted", "Partially failed", "Installed version unavailable", "Status unavailable", "Confirmation required"].includes(status) ? 0 : status === "In progress" || status === "Verification pending" ? 1 : 2;
  return records.sort((a, b) => rank(a.status) - rank(b.status));
}

export function operationResponseNotice(path: string, response: ObjectValue): string | undefined {
  if (path === "/v1/checkpoints" && (response.pending_restore || response.restored === false)) return "Restore requires attention. Open the recovery controls to continue or abort.";
  if (path === "/v1/updates" && (object(response.install_operation).cleanup_pending || object(response.operation).cleanup_pending)) return "Cleanup is incomplete. Open installation details to retry cleanup.";
  return undefined;
}

export function operationNoticeKind(key: string, exported?: { manual: boolean } | null): "success" | "warning" | "info" {
  if (exported) return exported.manual ? "warning" : "success";
  if (key === "complete") return "success";
  if (key === "Restore requires attention. Open the recovery controls to continue or abort." || key === "Cleanup is incomplete. Open installation details to retry cleanup.") return "warning";
  return "info";
}

export function operationRetryCommand(operation: ObjectValue, source: string, mode: string): ObjectValue | null {
  const kind = text(operation.kind) || "cold_switch";
  if (kind === "offline_import" && text(operation.archive_path)) return { action: kind, archive_path: operation.archive_path, ...(operation.offline_contents ? { offline_contents: operation.offline_contents } : {}) };
  if (kind === "offline_export" && text(operation.archive_path) && (text(operation.release_id) || object(operation.offline_contents).runtime === false)) return { action: kind, archive_path: operation.archive_path, ...(text(operation.release_id) ? { release_id: operation.release_id } : {}), ...(operation.offline_contents ? { offline_contents: operation.offline_contents } : {}) };
  if (kind === "cold_switch" && text(operation.tag)) return { action: "switch", tag: operation.tag, source, mode };
  return null;
}
