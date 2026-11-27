import { areaMatchesDisplay } from "./captureSelection.ts";
import type { CaptureArea, DisplayInfo, TemporaryCaptureTarget, TemporaryTargetSnapshot, TemporaryTargetTicket } from "./types";

// An event may announce a newer revision before an earlier status call returns.
export function acceptTemporaryTargetSnapshot(current: TemporaryTargetSnapshot | null, next: TemporaryTargetSnapshot, announcedRevision = 0): boolean {
  return Number.isSafeInteger(next.revision) && next.revision >= Math.max(current?.revision ?? 0, announcedRevision)
    && next.validationScope === "logical-cached-snapshot";
}

export function temporaryTargetTicket(target: TemporaryCaptureTarget): TemporaryTargetTicket {
  return { targetId: target.targetId, revision: target.revision };
}

// The cached logical check is only a GUI admission guard. Rust and Swift still
// validate the current display and actual ScreenCaptureKit geometry at start.
export function temporaryTargetMatchesDisplays(target: TemporaryCaptureTarget, displays: DisplayInfo[]): boolean {
  const display = displays.find(item => item.id === target.displayId);
  return !!display && target.display.id === target.displayId
    && target.display.displayRef === display.displayRef && target.display.snapshotToken === display.snapshotToken
    && target.display.widthPoints === display.widthPoints && target.display.heightPoints === display.heightPoints
    && (target.display.pointPixelScale ?? null) === (display.pointPixelScale ?? null)
    && target.display.sourcePixels?.width === display.sourcePixels?.width
    && target.display.sourcePixels?.height === display.sourcePixels?.height
    && areaMatchesDisplay(target.area, display);
}

// A pending but invalid target blocks start; it never silently falls back to a
// different saved target. These results must never be copied to SavedSettings.
export function resolveCaptureTarget(pending: TemporaryCaptureTarget | null, displays: DisplayInfo[], selectedDisplay: DisplayInfo | undefined, captureMode: "display" | "region", captureArea: CaptureArea | null): { displayId: number; area: CaptureArea; temporaryTarget?: TemporaryTargetTicket } | null {
  if (pending) return temporaryTargetMatchesDisplays(pending, displays)
    ? { displayId: pending.displayId, area: pending.area, temporaryTarget: temporaryTargetTicket(pending) } : null;
  if (!selectedDisplay || (captureMode === "region" && !areaMatchesDisplay(captureArea, selectedDisplay))) return null;
  return { displayId: selectedDisplay.id, area: captureMode === "region" ? captureArea! : {
    mode: "display", displayRef: selectedDisplay.displayRef, snapshotToken: selectedDisplay.snapshotToken, region: null,
  } };
}
