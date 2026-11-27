import type { Locale } from "./i18n";

export const recognitionLocales = ["ja-JP", "en-US"] as const;
export type RecognitionLocale = typeof recognitionLocales[number];
export type TranscriptionLanguage = { transcriptionLanguageMode: "auto" | "manual"; transcriptionManualLocale: RecognitionLocale };
export function isRecognitionLocale(value: unknown): value is RecognitionLocale {
  return value === "ja-JP" || value === "en-US";
}
export function restoreTranscriptionLanguage(saved: Partial<Record<keyof TranscriptionLanguage, unknown>>): TranscriptionLanguage {
  const manual = isRecognitionLocale(saved.transcriptionManualLocale) ? saved.transcriptionManualLocale : "ja-JP";
  return { transcriptionLanguageMode: saved.transcriptionLanguageMode === "manual" && isRecognitionLocale(saved.transcriptionManualLocale) ? "manual" : "auto", transcriptionManualLocale: manual };
}
export function resolveTranscriptionLocale(settings: TranscriptionLanguage, display: Locale): RecognitionLocale {
  return settings.transcriptionLanguageMode === "manual" ? settings.transcriptionManualLocale : display === "ja" ? "ja-JP" : "en-US";
}
