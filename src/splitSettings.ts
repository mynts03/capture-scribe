// Minutes fit u32, JavaScript's exact integer range after conversion to ms,
// and FFmpeg's signed 64-bit microsecond duration. Not a recording-time limit.
export const MAX_SPLIT_MINUTES = 4_294_967_295;
export type SplitSettings = {
  splitLongVideo: boolean;
  splitThresholdMinutes: number;
  splitSegmentMinutes: number;
  splitThresholdLinked: boolean;
};
export type SplitDraft = { threshold: string; segment: string; linked: boolean };

export function validSplitMinutes(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= MAX_SPLIT_MINUTES;
}
export function parseSplitMinutes(value: string): number | null {
  if (!/^\d+$/.test(value.trim())) return null;
  const number = Number(value.trim());
  return validSplitMinutes(number) ? number : null;
}
export function restoreSplitSettings(saved: Partial<Record<keyof SplitSettings, unknown>>): SplitSettings {
  const enabled = typeof saved.splitLongVideo === "boolean" ? saved.splitLongVideo : false;
  if (validSplitMinutes(saved.splitThresholdMinutes) && validSplitMinutes(saved.splitSegmentMinutes)
    && typeof saved.splitThresholdLinked === "boolean"
    && (!saved.splitThresholdLinked || saved.splitThresholdMinutes === saved.splitSegmentMinutes)) {
    return { splitLongVideo: enabled, splitThresholdMinutes: saved.splitThresholdMinutes,
      splitSegmentMinutes: saved.splitSegmentMinutes, splitThresholdLinked: saved.splitThresholdLinked };
  }
  // Existing app boolean is retained; there is no old-software import or 14/10 rule.
  return { splitLongVideo: enabled, splitThresholdMinutes: 10, splitSegmentMinutes: 10, splitThresholdLinked: true };
}
export function changeSplitSegment(settings: SplitSettings, value: string): SplitSettings {
  const segment = parseSplitMinutes(value);
  if (segment === null) return settings; // Keep the last accepted settings.
  return { ...settings, splitSegmentMinutes: segment,
    splitThresholdMinutes: settings.splitThresholdLinked ? segment : settings.splitThresholdMinutes };
}
export function splitDraft(settings: SplitSettings): SplitDraft {
  return { threshold: String(settings.splitThresholdMinutes), segment: String(settings.splitSegmentMinutes), linked: settings.splitThresholdLinked };
}
export function changeSplitDraft(draft: SplitDraft, field: "threshold" | "segment", value: string): SplitDraft {
  if (field === "threshold") return { ...draft, threshold: value, linked: false };
  return { ...draft, segment: value, threshold: draft.linked ? value : draft.threshold };
}
export function linkSplitDraft(draft: SplitDraft): SplitDraft {
  return { ...draft, threshold: draft.segment, linked: true };
}
export function applySplitDraft(settings: SplitSettings, draft: SplitDraft): SplitSettings | null {
  const threshold = parseSplitMinutes(draft.threshold), segment = parseSplitMinutes(draft.segment);
  if (threshold === null || segment === null || (draft.linked && threshold !== segment)) return null;
  return { ...settings, splitThresholdMinutes: threshold, splitSegmentMinutes: segment, splitThresholdLinked: draft.linked };
}
