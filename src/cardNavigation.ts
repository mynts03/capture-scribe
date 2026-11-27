import type { TranslationKey } from "./i18n";

export const cardNavigationItems = [
  { headingId: "capture-heading", labelKey: "navCapture", descriptionKey: "navCaptureDescription" },
  { headingId: "save-heading", labelKey: "navSave", descriptionKey: "navSaveDescription" },
  { headingId: "session-heading", labelKey: "navSession", descriptionKey: "navSessionDescription" },
  { headingId: "process-heading", labelKey: "navProcess", descriptionKey: "navProcessDescription" },
  { headingId: "results-heading", labelKey: "navResults", descriptionKey: "navResultsDescription" },
] as const satisfies ReadonlyArray<{ headingId: string; labelKey: TranslationKey; descriptionKey: TranslationKey }>;

export function navigateToCard(heading: HTMLElement | null, reduceMotion: boolean) {
  if (!heading) return;
  // Move keyboard/screen-reader context without triggering an extra scroll.
  heading.focus({ preventScroll: true });
  heading.scrollIntoView({ behavior: reduceMotion ? "instant" : "smooth", block: "start" });
}
