import type { RecordingHistoryItem, StopCaptureResult } from "./types";

export type RecordingReference = Pick<RecordingHistoryItem,
  "manifestPath" | "name" | "sourceMasterPath" | "sourceMasterAvailable" | "processingAvailable"
> & { durationSeconds: number | null };

// A completed capture stays independent of the explicit processing selection.
// After restart, use the global catalogue's recording-date order.
export function getLatestRecording(
  lastResult: StopCaptureResult | null,
  history: readonly RecordingHistoryItem[],
): RecordingReference | null {
  if (lastResult?.validated && lastResult.status.finalPath) {
    const source = lastResult.status.finalPath;
    return {
      manifestPath: lastResult.manifestPath,
      name: source.split(/[\\/]/).pop() ?? source,
      sourceMasterPath: source,
      sourceMasterAvailable: history.find(item => item.manifestPath === lastResult.manifestPath)?.sourceMasterAvailable ?? true,
      processingAvailable: history.find(item => item.manifestPath === lastResult.manifestPath)?.processingAvailable ?? true,
      durationSeconds: lastResult.durationSeconds,
    };
  }
  return history[0] ?? null;
}

export function getProcessingTarget(
  selectedHistory: RecordingHistoryItem | null,
  latest: RecordingReference | null,
): RecordingReference | null {
  // Never silently substitute another recording for an unavailable selection.
  return selectedHistory ?? latest;
}
