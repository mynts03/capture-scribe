import type { CaptureArea, DisplayInfo, PixelSize, RegionSelectionResponse } from "./types";

// The saved mode is independent of the saved rectangle. Never turn an explicit
// full-display choice into region mode just because a rectangle exists.
export function restoreCaptureMode(value?: unknown): "display" | "region" {
  return value === "region" ? "region" : "display";
}

export function restoreCaptureArea(value: unknown): CaptureArea | null {
  if (!value || typeof value !== "object") return null;
  const a = value as Partial<CaptureArea>;
  if (a.mode !== "region" || typeof a.displayRef !== "string" || !a.displayRef || typeof a.snapshotToken !== "string" || !a.snapshotToken || !a.region) return null;
  const r = a.region;
  if (r.origin !== "top-left" || r.unit !== "display-logical" || ![r.x,r.y,r.width,r.height].every(n => typeof n === "number" && Number.isFinite(n)) || r.x < 0 || r.y < 0 || r.width < 16 || r.height < 16) return null;
  return a as CaptureArea;
}

export function areaMatchesDisplay(area: CaptureArea | null, display: DisplayInfo | undefined): boolean {
  const a = restoreCaptureArea(area);
  return !!a?.region && !!display && a.displayRef === display.displayRef && a.snapshotToken === display.snapshotToken
    && a.region.x+a.region.width <= display.widthPoints && a.region.y+a.region.height <= display.heightPoints;
}

function alignedAxisPixels(start: number, length: number, extent: number, scale: number): number | null {
  // Match CaptureGeometry.swift: round the two edges independently and clamp
  // to the display. A subpixel selection that native would reject has no size.
  const limit = extent * scale;
  const first = Math.min(limit, Math.round(start * scale));
  const last = Math.max(first, Math.min(limit, Math.round((start + length) * scale)));
  const alignedLength = (last - first) / scale;
  const pixels = Math.round(alignedLength * scale);
  if (Math.abs(first / scale - start) >= 0.0001 || Math.abs(alignedLength - length) >= 0.0001
    || !Number.isSafeInteger(pixels) || pixels <= 0) return null;
  return pixels;
}

// The helper supplies the full capture size and the actual SCK scale together.
// This is source geometry only. Padding and approved alternatives are known
// independently in the recorded result.
export function captureTargetPixels(display: DisplayInfo, area: CaptureArea | null): PixelSize | null {
  const { sourcePixels, pointPixelScale: scale } = display;
  if (!sourcePixels || !Number.isSafeInteger(sourcePixels.width) || sourcePixels.width <= 0
    || !Number.isSafeInteger(sourcePixels.height) || sourcePixels.height <= 0
    || typeof scale !== "number" || !Number.isFinite(scale) || scale <= 0) return null;
  if (!area) return sourcePixels;
  if (!areaMatchesDisplay(area, display) || !area.region) return null;
  const { x, y, width, height } = area.region;
  const widthPixels = alignedAxisPixels(x, width, display.widthPoints, scale);
  const heightPixels = alignedAxisPixels(y, height, display.heightPoints, scale);
  return widthPixels !== null && heightPixels !== null ? { width: widthPixels, height: heightPixels } : null;
}

// A cancelled/failed edit never supplies a replacement for the confirmed area.
export function confirmedSelectionFromResponse(response: RegionSelectionResponse, requestId: string, display: DisplayInfo): CaptureArea | null {
  if (response.requestId !== requestId || response.protocolVersion !== 2) throw new Error("Invalid selector response");
  if (response.status === "cancelled" && response.area === null) return null;
  const area = response.area;
  if (response.status === "confirmed" && area?.displayRef === display.displayRef && area.snapshotToken === display.snapshotToken
    && ((area.mode === "display" && area.region === null) || (area.mode === "region" && areaMatchesDisplay(area, display)))) return area;
  throw { code: "capture.region_invalid", raw: response.raw ?? response.status };
}
