import { translate, type Locale } from "./i18n";
import { MessageView } from "./MessageView";
import type { CaptureFailureView as Failure, FileObservation } from "./types";

export function CaptureFailureView({ failure, language, busy, reveal }: {
  failure: Failure; language: Locale; busy: boolean; reveal: (path: string) => void;
}) {
  const text = (key: Parameters<typeof translate>[1]) => translate(language, key);
  const record = failure.record;
  const state = (file: FileObservation) => text(file.state === "present" ? "failureFilePresent" : file.state === "missing" ? "failureFileMissing" : file.state === "unavailable" ? "failureFileUnavailable" : "failureFileUnknown");
  return <article className="history-item capture-save-failure" data-attempt-id={record.attemptId}>
    <div>
      <h3>{text("captureSaveFailed")}</h3>
      <h4>{record.recordingName}</h4>
      <p>{new Date(record.recordedAtUnixMs).toLocaleString(language === "ja" ? "ja-JP" : "en-US")}</p>
      <p>{text(failure.persisted ? "failureRecordSaved" : "failureRecordNotSaved")}</p>
      {failure.persistenceError && <div className="warning" role="alert"><MessageView message={failure.persistenceError} language={language} /></div>}
      <p>{text("failureNoValidatedVideo")}</p>
      <p>{text("failurePlannedFile")}: {state(failure.plannedFinalFile)}</p>
      <p className="history-path">{failure.plannedFinalFile.path}</p>
      <p>{text("failureWorkingFile")}: {state(failure.workingFile)}</p>
      {failure.workingFile.state === "present" && <>
        <p className="history-path">{failure.workingFile.path}</p>
        <button type="button" className="secondary" disabled={busy} onClick={() => reveal(failure.workingFile.path)}>{text("showInFinder")}</button>
      </>}
      <p className="note">{text("failurePresenceLimit")}</p>
      <details className="message-details"><summary>{text("technicalDetails")}</summary>
        <p>{record.attemptId} · {record.jobId}</p>
        <p>{text("failureCheckedAt")}: {new Date(failure.workingFile.checkedAtUnixMs).toLocaleString(language === "ja" ? "ja-JP" : "en-US")}</p>
        <MessageView message={record.error} language={language} />
        <pre>{JSON.stringify(record.error, null, 2)}</pre>
      </details>
    </div>
  </article>;
}
