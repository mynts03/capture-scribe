import type { HistoryWindowSnapshot, RecordingHistoryItem } from "./types";
import { latestRunStatus } from "./ProcessingRunsView";
import { captureHistoryRows } from "./captureFailureSelection";

export function selectableRecording(item: RecordingHistoryItem) {
  return item.processingAvailable && item.sourceMasterAvailable && !item.snapshotStale && item.manifestState === "available";
}
export function historyEmptyStateKey(snapshot: HistoryWindowSnapshot | null): "emptyHistory" | "noMatchingHistory" {
  const catalog = snapshot?.catalog;
  return catalog && (catalog.items.length > 0 || (catalog.captureFailures?.length ?? 0) > 0)
    ? "noMatchingHistory" : "emptyHistory";
}
export function historyView(snapshot: HistoryWindowSnapshot | null, query: string, saveFilter: string, processFilter: string) {
  const catalog = snapshot?.catalog;
  if (!catalog) return [];
  return captureHistoryRows(catalog.items, catalog.captureFailures ?? []).filter(row => {
    const name = row.kind === "saved" ? row.item.name : row.failure.record.recordingName;
    const identity = row.kind === "saved" ? row.item.jobId : row.failure.record.jobId;
    if (!`${name} ${identity}`.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase())) return false;
    if (saveFilter === "saved" && row.kind !== "saved" || saveFilter === "failed" && row.kind !== "failed") return false;
    if (processFilter === "all") return true;
    if (row.kind !== "saved") return processFilter === "unprocessed";
    const updates = (catalog.processingRunUpdates ?? []).filter(u => u.manifestPath === row.item.manifestPath && u.run.jobId === row.item.jobId);
    const status = latestRunStatus(row.item.processingRuns ?? [], updates) ?? row.item.processingStatus ?? "unprocessed";
    return processFilter === "active" ? ["accepted", "running"].includes(status) : processFilter === "unknown" ? ["unknown", "interrupted"].includes(status) : status === processFilter;
  });
}
