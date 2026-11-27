import { formatMessage, translate, type Locale } from "./i18n";
import type { AppMessage } from "./types";

export function MessageView({ message, language, recording = false }: { message: AppMessage; language: Locale; recording?: boolean }) {
  // Same rules for every display language; only the dictionary text differs.
  const unknownRecording = recording && (!message.code || translate(language, message.code as Parameters<typeof translate>[1]) === message.code);
  const display = unknownRecording ? translate(language, "recordingError") : formatMessage(message, language);
  const detail = message.code === "capture.start_timeout" ? translate(language, "captureStartTimeoutDetail") : null;
  return <>
    <span>{display}</span>
    {(detail || message.raw && message.raw !== display || unknownRecording && (message.code || message.params && Object.keys(message.params).length > 0)) && (
      <details className="message-details">
        <summary>{translate(language, "technicalDetails")}</summary>
        {detail && <p>{detail}</p>}
        {message.code && <code>{message.code}</code>}
        {message.raw && <pre>{message.raw}</pre>}
        {message.params && <pre>{JSON.stringify(message.params, null, 2)}</pre>}
      </details>
    )}
  </>;
}
