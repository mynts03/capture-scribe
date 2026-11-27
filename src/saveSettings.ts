import type { TranslationKey } from "./i18n";

export type SavedDestination = { outputDirectory: string; subfolder: string };

// Compare Rust-resolved locations, never an unvalidated text-field draft.
export function shouldSwitchDestination(previous: string | null, validated: string | undefined) {
  return validated !== undefined && validated !== previous;
}

export function restoreDestination(saved: { outputDirectory?: unknown; subfolder?: unknown }, fallback: string): SavedDestination {
  return {
    outputDirectory: typeof saved.outputDirectory === "string" && saved.outputDirectory.trim() ? saved.outputDirectory : fallback,
    subfolder: typeof saved.subfolder === "string" ? saved.subfolder : "",
  };
}

export function fileNameProblem(value: string): TranslationKey | null {
  const stem = fileStem(value);
  if (!stem) return "capture.filename_required";
  return /[/\\:\u0000-\u001f\u007f-\u009f]/.test(stem) || stem === "." || stem === ".." ? "capture.filename_invalid" : null;
}

export function fileStem(value: string) { return value.trim().replace(/(?:\.mp4)+$/, "").trim(); }

// Display only. Rust validates the directory and the file name again at start.
export function previewFilePath(directory: string, fileName: string) {
  const separator = directory.includes("\\") ? "\\" : "/";
  return directory.replace(/[/\\]$/, "") + separator + fileStem(fileName) + ".mp4";
}
