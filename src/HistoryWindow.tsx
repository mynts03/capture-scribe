import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";
import { translate, toAppMessage, type TranslationKey } from "./i18n";
import { useHistoryWindowState } from "./historyWindowState";
import { historyView, historyEmptyStateKey, selectableRecording } from "./historyView";
import { MessageView } from "./MessageView";
import { CaptureFailureView } from "./CaptureFailureView";
import { ProcessingRunsView, ProcessingRunStatusSummary, mergedRuns, runStatusKey } from "./ProcessingRunsView";
import type { AppMessage, HistoryWindowSnapshot } from "./types";

export function HistoryWindow() {
  const [error, setError] = useState<AppMessage | null>(null);
  const { snapshot, receive } = useHistoryWindowState(setError);
  const language = snapshot?.language ?? "ja";
  const text = (key: TranslationKey, params?: Record<string, string | number>) => translate(language, key, params);
  const [query, setQuery] = useState("");
  const [saveFilter, setSaveFilter] = useState("all");
  const [processFilter, setProcessFilter] = useState("all");
  const [limit, setLimit] = useState(100);
  const [viewed, setViewed] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [pending, setPending] = useState(0);
  const [reloading, setReloading] = useState(0);
  const rows = historyView(snapshot, query, saveFilter, processFilter).slice(0, limit);
  const catalog = snapshot?.catalog;
  const selected = snapshot?.selectedIds ?? [];
  const shownIds = rows.filter(row => row.kind === "saved").map(row => row.id);
  const hidden = selected.filter(id => !shownIds.includes(id)).length;
  const detail = rows.find(row => `${row.kind}:${row.id}` === viewed) ?? (viewed ? null : rows[0]);
  useEffect(() => { setLimit(100); }, [query, saveFilter, processFilter]);
  const windowTitle = `CaptureScribe — ${text("navResults")}`;
  useEffect(() => {
    document.documentElement.lang = language;
    void getCurrentWindow().setTitle(windowTitle).catch(reason => setError(toAppMessage(reason)));
  }, [language, windowTitle]);
  useEffect(() => {
    let disposed = false;
    setLoading(true);
    // Only history is queried. No App mount, settings restore/write or media commands.
    void invoke("list_recording_history", { outputDirectory: null }).then(() => invoke<HistoryWindowSnapshot>("history_window_snapshot"))
      .then(value => { if (!disposed) receive(value); })
      .catch(reason => { if (!disposed) setError(toAppMessage(reason)); })
      .finally(() => { if (!disposed) setLoading(false); });
    return () => { disposed = true; };
  }, [reloading]);
  async function select(ids: string[], value: boolean) {
    setPending(count => count + 1);
    try { receive(await invoke<HistoryWindowSnapshot>("set_history_selection", { recordIds: ids, selected: value })); }
    catch (reason) { setError(toAppMessage(reason)); }
    finally { setPending(count => count - 1); }
  }
  async function reveal(path: string) {
    try { await invoke("reveal_path", { path }); } catch (reason) { setError(toAppMessage(reason)); }
  }
  async function importFolder() {
    try {
      const folder = await open({ directory: true, multiple: false, title: text("importHistory") });
      if (typeof folder === "string") {
        await invoke("list_recording_history", { outputDirectory: folder });
        receive(await invoke<HistoryWindowSnapshot>("history_window_snapshot"));
      }
    } catch (reason) { setError(toAppMessage(reason)); }
  }
  return <main className="shell history-window" aria-label={windowTitle}>
    <header className="app-header">
      <h1 id="results-heading" tabIndex={-1}>{text("navResults")}</h1>
      <div className="language-switch" role="group" aria-label={text("languageSelector")}>
        {(["ja", "en"] as const).map(locale => <button type="button" key={locale} aria-pressed={language === locale} className={language === locale ? "active" : ""}
          onClick={() => void invoke<HistoryWindowSnapshot>("set_history_language", { language: locale }).then(receive).catch(reason => setError(toAppMessage(reason)))}>{text(locale === "ja" ? "languageJapanese" : "languageEnglish")}</button>)}
      </div>
    </header>
    {error && <div className="alert" role="alert"><MessageView message={error} language={language}/></div>}
    <div className="history-toolbar">
      <button type="button" className="secondary" disabled={loading} onClick={() => setReloading(value => value + 1)}>{text(loading ? "loading" : "refreshHistory")}</button>
      <button type="button" className="secondary" disabled={loading} onClick={() => void importFolder()}>{text("importHistory")}</button>
      <button type="button" className="secondary" onClick={() => void invoke("open_processing_settings").catch(reason => setError(toAppMessage(reason)))}>{text("historyOpenSettings")}</button>
    </div>
    <div className="history-filters">
      <label>{text("historySearch")}<input type="search" value={query} onChange={event => setQuery(event.target.value)}/></label>
      <label>{text("historySaveFilter")}<select value={saveFilter} onChange={event => setSaveFilter(event.target.value)}>
        <option value="all">{text("historyFilterAll")}</option><option value="saved">{text("historySaved")}</option><option value="failed">{text("historySaveFailed")}</option>
      </select></label>
      <label>{text("historyProcessFilter")}<select value={processFilter} onChange={event => setProcessFilter(event.target.value)}>
        <option value="all">{text("historyFilterAll")}</option><option value="unprocessed">{text("unprocessed")}</option>
        {(["completed", "partial", "failed", "cancelled", "not-executed", "unknown", "active"] as const).map(state => <option key={state} value={state}>{text(runStatusKey(state === "active" ? "running" : state))}</option>)}
      </select></label>
    </div>
    <div className="history-toolbar">
      <p role="status">{text("historySelectionCount", { count: selected.length, hidden })}</p>
      <button type="button" className="secondary" disabled={pending > 0 || loading} onClick={() => void select(rows.flatMap(row => row.kind === "saved" && selectableRecording(row.item) ? [row.id] : []), true)}>{text("historySelectVisible")}</button>
      <button type="button" className="secondary" disabled={pending > 0 || loading} onClick={() => void select(shownIds, false)}>{text("historyClearVisible")}</button>
    </div>
    <div className="history-workspace">
      <section className="panel history-records" aria-label={text("historyHeading")}>
        {!rows.length && <p className="empty-state">{text(historyEmptyStateKey(snapshot))}</p>}
        {rows.map(row => {
          const name = row.kind === "saved" ? row.item.name : row.failure.record.recordingName;
          const available = row.kind === "saved" && selectableRecording(row.item);
          const updates = row.kind === "saved" ? catalog?.processingRunUpdates?.filter(u => u.manifestPath === row.item.manifestPath && u.run.jobId === row.item.jobId) ?? [] : [];
          const runs = row.kind === "saved" ? mergedRuns(row.item.processingRuns ?? [], updates) : [];
          const olderPaths = runs.slice(1).flatMap(entry => entry.run.artifactPaths);
          const availability = row.kind === "saved" ? runs.slice(1).flatMap(entry => entry.run.artifactPaths.map(path => entry.view?.artifacts.find(a => a.path === path)?.availability ?? (row.item.snapshotStale ? "unknown" : row.item.artifacts.find(a => a.path === path)?.availability ?? "unknown"))) : [];
          const viewId = `${row.kind}:${row.id}`;
          const checked = row.kind === "saved" && selected.includes(row.id);
          return <article className={`history-item${detail === row ? " viewed" : ""}${checked ? " selected" : ""}`} key={viewId} onClick={() => setViewed(viewId)}>
            <input type="checkbox" checked={checked} disabled={pending > 0 || (!available && !checked)} onClick={event => event.stopPropagation()}
              aria-label={text("historySelectRecord", { name })} onChange={event => void select([row.id], event.target.checked)}/>
            <button type="button" className="history-row-detail" aria-label={text("historyInspect", { name })} aria-pressed={detail === row} onClick={() => setViewed(viewId)}>
              <strong>{name}</strong>
              <span>{text(row.kind === "saved" ? "historySaved" : "historySaveFailed")}</span>
              {row.kind === "saved" && <ProcessingRunStatusSummary runs={row.item.processingRuns ?? []} updates={updates} language={language} fallbackStatus={row.item.processingStatus}/>}
              {olderPaths.length > 0 && <span>{text(availability.includes("available") ? "historyPastAvailable" : availability.includes("unknown") ? "historyPastRecorded" : "historyPastUnavailable")}</span>}
              {!available && <span className="warning">{text("processingUnavailable")}</span>}
            </button>
          </article>;
        })}
        {historyView(snapshot, query, saveFilter, processFilter).length > limit && <button className="secondary" onClick={() => setLimit(value => value + 100)}>{text("loadMoreHistory")}</button>}
      </section>
      <section className="panel history-detail" aria-labelledby="history-detail-heading">
        <h2 id="history-detail-heading">{text("historyDetail")}</h2>
        {!detail ? <p>{text("historyNoDetail")}</p> : detail.kind === "failed" ? <CaptureFailureView failure={detail.failure} language={language} busy={false} reveal={path => void reveal(path)}/> : (() => {
          const item = detail.item;
          const artifacts = item.artifacts
            .filter(a => a.role !== "capture_report")
            .map(a => item.snapshotStale ? { ...a, availability: "unknown" } : a);
          return <>
            <h3>{item.name}</h3>
            <p>{item.recordedAtUnixMs === null ? text("recordingDateUnknown") : new Date(item.recordedAtUnixMs).toLocaleString(language === "ja" ? "ja-JP" : "en-US")}</p>
            <p>{item.durationSeconds.toFixed(1)} {text("seconds")} · {text("historySaved")}</p>
            <p className="history-path">{item.recordId} · {item.jobId}</p>
            {item.snapshotStale && <p className="warning">{text("historyStale", { date: new Date(item.lastConfirmedUnixMs).toLocaleString() })}</p>}
            {item.manifestState !== "available" && <p className="warning">{text("manifestUnavailable")}</p>}
            <ProcessingRunsView runs={item.processingRuns ?? []} updates={catalog?.processingRunUpdates?.filter(u => u.manifestPath === item.manifestPath && u.run.jobId === item.jobId) ?? []} language={language} artifacts={artifacts} busy={false} reveal={path => void reveal(path)}/>
            <h3>{text("relatedFiles")}</h3>
            <ul>{artifacts.filter((a, i, all) => all.findIndex(b => a.path === b.path) === i).map(a => <li key={a.path}>
              <p className="history-path">{a.path}</p><span>{text(a.availability === "available" ? "fileAvailable" : a.availability === "unknown" ? "runAvailabilityUnknown" : "fileUnavailable")}</span>{" "}
              <button type="button" className="secondary" disabled={a.availability !== "available"} onClick={() => void reveal(a.path)}>{text("showInFinder")}</button>
            </li>)}</ul>
            {item.warnings.map((warning, index) => <div className="warning" key={index}><MessageView message={warning} language={language}/></div>)}
            <details className="message-details"><summary>{text("technicalDetails")}</summary><p className="history-path">{item.manifestPath}</p><p>{item.manifestState}</p></details>
          </>;
        })()}
      </section>
    </div>
    <ProcessingRunsView runs={[]} updates={catalog?.processingRunUpdates?.filter(u => !catalog.items.some(item => item.manifestPath === u.manifestPath && item.jobId === u.run.jobId)) ?? []} language={language} busy={false} reveal={path => void reveal(path)}/>
    {catalog?.warnings.map((warning, index) => <div className="warning" key={index}><MessageView message={warning} language={language}/></div>)}
  </main>;
}
