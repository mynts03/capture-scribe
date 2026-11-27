import type { CaptureFailureView, RecordingHistoryItem, StopCaptureResult } from "./types";

export function mergeCaptureFailures(saved: readonly CaptureFailureView[], memory: CaptureFailureView | null): CaptureFailureView[] {
  // Prefer fresh file observations from disk. A failed disk write remains visible
  // in memory and must not be mistaken for a persisted history entry.
  const entries = new Map(saved.map(f => [f.record.attemptId, f]));
  if (memory && !entries.has(memory.record.attemptId)) entries.set(memory.record.attemptId, memory);
  return [...entries.values()].sort((a, b) => b.record.recordedAtUnixMs - a.record.recordedAtUnixMs || a.record.attemptId.localeCompare(b.record.attemptId));
}

export function latestCaptureFailure(failures: readonly CaptureFailureView[], history: readonly RecordingHistoryItem[], lastResult: StopCaptureResult | null): CaptureFailureView | null {
  const latest = failures[0];
  if (!latest) return null;
  const successTime = history.reduce((latest, item) => Math.max(latest, item.recordedAtUnixMs ?? 0), lastResult?.recordedAtUnixMs ?? 0);
  return latest.record.recordedAtUnixMs >= successTime ? latest : null;
}

export function captureHistoryRows(history: readonly RecordingHistoryItem[], failures: readonly CaptureFailureView[]) {
  const rows = [
    ...history.map(item => ({ kind: "saved" as const, id: item.recordId, time: item.recordedAtUnixMs ?? 0, item })),
    ...failures.map(failure => ({ kind: "failed" as const, id: failure.record.attemptId, time: failure.record.recordedAtUnixMs, failure })),
  ];
  return rows.sort((a,b) => b.time - a.time || a.id.localeCompare(b.id));
}
