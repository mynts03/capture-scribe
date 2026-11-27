import ja from "./locales/ja.json" with { type: "json" };
import en from "./locales/en.json" with { type: "json" };
import type { AppMessage } from "./types";

export type Locale = "ja" | "en";

export type TranslationKey = keyof typeof ja;
type Dictionary = Record<TranslationKey, string>;
type TranslationParams = Record<string, string | number>;

// Missing or extra English keys fail TypeScript, including direct type checks.
type EnglishDictionary = Dictionary & Record<Exclude<keyof typeof en, TranslationKey>, never>;
const dictionaries: { ja: Dictionary; en: EnglishDictionary } = { ja, en };

export function isLocale(value: unknown): value is Locale {
  return value === "ja" || value === "en";
}

export function detectPreferredLocale(): Locale {
  const preferred = navigator.languages?.[0] ?? navigator.language;
  return preferred.toLowerCase().startsWith("ja") ? "ja" : "en";
}

export function translate(locale: Locale, key: TranslationKey, params: TranslationParams = {}): string {
  const template = dictionaries[locale]?.[key];
  return (typeof template === "string" ? template : key).replace(/\{(\w+)\}/g, (match, name: string) =>
    Object.prototype.hasOwnProperty.call(params, name) ? String(params[name]) : match,
  );
}

// External payloads are checked at the boundary; unknown strings stay untouched.
export function toAppMessage(value: unknown): AppMessage {
  if (typeof value === "string") return { raw: value };
  if (value instanceof Error) return { raw: value.message };
  if (value && typeof value === "object") {
    const input = value as Record<string, unknown>;
    const params: Record<string, string | number> = {};
    if (input.params && typeof input.params === "object") {
      for (const [key, param] of Object.entries(input.params)) {
        if (typeof param === "string" || typeof param === "number") params[key] = param;
      }
    }
    const code = typeof input.code === "string" ? input.code : undefined;
    const raw = typeof input.raw === "string" ? input.raw : undefined;
    if (code !== undefined || raw !== undefined || Object.keys(input).length === 0 || "params" in input) return { code, params, raw };
  }
  if (value == null) return {};
  try { return { raw: JSON.stringify(value) }; } catch { return { raw: String(value) }; }
}

export function formatMessage(value: unknown, locale: Locale): string {
  const message = toAppMessage(value);
  const dictionary = dictionaries[locale] as Partial<Record<string, string>> | undefined;
  const template = message.code && Object.prototype.hasOwnProperty.call(dictionary ?? {}, message.code)
    ? dictionary?.[message.code] : undefined;
  if (typeof template === "string" && template.trim()) {
    let complete = true;
    const rendered = template.replace(/\{(\w+)\}/g, (match, name: string) => {
      const param = message.params?.[name];
      if (param === undefined) { complete = false; return match; }
      return String(param);
    });
    if (complete) {
      const context = message.params?.fileName ?? message.params?.path;
      return context === undefined ? rendered : rendered + " (" + String(context) + ")";
    }
  }
  if (message.raw?.trim()) return message.raw;
  if (message.code?.trim()) return message.code;
  const generic = dictionary?.genericError;
  return typeof generic === "string" && generic.trim() ? generic : en.genericError;
}
