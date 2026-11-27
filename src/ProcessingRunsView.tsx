import { translate, type Locale, type TranslationKey } from "./i18n";
import { MessageView } from "./MessageView";
import type { PostProcessResult, ProcessingRun, ProcessingRunView, RecordingHistoryItem, RecordingHistoryResult } from "./types";

export function runStatusKey(status: string): TranslationKey {
  const keys: Record<string, TranslationKey> = { completed: "processStatusCompleted", partial: "processStatusPartial", failed: "processStatusFailed", cancelled: "runCancelled", "not-executed": "runNotExecuted", accepted: "runAccepted", running: "runRunning", interrupted: "runUnknown", unknown: "runUnknown" };
  return Object.prototype.hasOwnProperty.call(keys, status) ? keys[status] : "runUnknown";
}
export function mergedRuns(runs: ProcessingRun[], updates: ProcessingRunView[]) {
  const entries = runs.map(run => ({run, view: updates.find(u => u.run.runId && u.run.runId === run.runId)}));
  for (const view of updates) {
    const existing = entries.find(e => e.run.runId && e.run.runId === view.run.runId);
    if (existing) { if (!view.manifestApplied) existing.run = view.run; existing.view = view; }
    else entries.push({run:view.run, view});
  }
  return entries.map((e,i)=>({...e,index:i})).sort((a,b)=>(b.run.acceptedAtUnixMs ?? b.run.finishedAtUnixMs ?? 0)-(a.run.acceptedAtUnixMs ?? a.run.finishedAtUnixMs ?? 0)||b.index-a.index);
}
export function latestRunStatus(runs: ProcessingRun[], updates: ProcessingRunView[]) {
  return mergedRuns(runs,updates)[0]?.run.status;
}
export function lastConfirmedRunStatus(runs: ProcessingRun[], updates: ProcessingRunView[]) {
  return mergedRuns(runs,updates).find(e=>!e.view?.active && ["completed","partial","failed","cancelled","not-executed"].includes(e.run.status))?.run.status;
}
export function ProcessingRunStatusSummary({runs,updates,language,fallbackStatus}: {
  runs:ProcessingRun[];updates:ProcessingRunView[];language:Locale;fallbackStatus?:string|null;
}) {
  const latest=latestRunStatus(runs,updates)??fallbackStatus;
  if (!latest) return <>{translate(language,"unprocessed")}</>;
  const confirmed=lastConfirmedRunStatus(runs,updates);
  const label=(status:string)=>translate(language,status==="completed"?"runCompletedNormally":runStatusKey(status));
  return <><span>{translate(language,"runLatestExecution")}: {label(latest)}</span>
    {!["completed","partial","failed","cancelled","not-executed"].includes(latest) && <><br/><span>{translate(language,"runLastConfirmed")}: {confirmed?label(confirmed):translate(language,"runNotRecorded")}</span></>}
  </>;
}

// Update display metadata only after positive, same-recording/same-run proof.
// Worker outcomes/settings/errors are immutable facts, not reconciliation flags.
export function reconcileProcessResult(current:PostProcessResult|null, history:RecordingHistoryResult):PostProcessResult|null {
  const old=current?.run;
  if (!current || !old?.run.runId || !old.run.jobId || current.manifestPath!==old.manifestPath) return current;
  const sameRun=(run:ProcessingRun,path:string)=>run.runId===old.run.runId && path===old.manifestPath && run.jobId===old.run.jobId
    && (!run.manifestPath||run.manifestPath===old.manifestPath) && run.status===old.run.status
    && (!old.run.recordId||!run.recordId||run.recordId===old.run.recordId);
  const update=history.processingRunUpdates?.find(u=>sameRun(u.run,u.manifestPath));
  let refreshed:ProcessingRunView;
  if (update) {
    if (update.active) return current;
    refreshed={...update,run:old.run,recordError:update.recordError??(!update.manifestApplied?old.recordError:null)};
  } else {
    const item=history.items.find(i=>i.manifestPath===old.manifestPath && i.jobId===old.run.jobId && i.manifestState==="available" && !i.snapshotStale
      && i.sourceMasterPath===current.sourceMaster && (!old.run.recordId||i.recordId===old.run.recordId) && i.processingRuns?.some(run=>sameRun(run,i.manifestPath)));
    if (!item) return current;
    // Manifest proof does not prove that an outbox write failure was repaired.
    refreshed={...old,manifestApplied:true,recordError:old.outboxSaved?null:old.recordError,
      artifacts:old.artifacts.map(a=>({...a,availability:item.artifacts.find(f=>f.path===a.path)?.availability??"unknown"}))};
  }
  const cleared=old.recordError && refreshed.manifestApplied && refreshed.outboxSaved && refreshed.recordError===null;
  return {...current,run:refreshed,historyWarnings:cleared
    ? current.historyWarnings.filter(w=>JSON.stringify(w)!==JSON.stringify(old.recordError)) : current.historyWarnings};
}

export function ProcessingRunsView({runs,updates,language,artifacts=[],busy,reveal}: {
  runs:ProcessingRun[];updates:ProcessingRunView[];language:Locale;artifacts?:RecordingHistoryItem["artifacts"];busy:boolean;reveal:(path:string)=>void;
}) {
  const entries=mergedRuns(runs,updates), text=(key:TranslationKey)=>translate(language,key);
  if (!entries.length) return null;
  const date=(value?:number)=>value===undefined||value===null?text("runNotRecorded"):new Date(value).toLocaleString(language==="ja"?"ja-JP":"en-US");
  return <details className="history-warning-details">
    <summary>{text("processingRunHistory")} ({entries.length}) · {text(runStatusKey(entries[0].run.status))}</summary>
    {entries.map(({run,view,index})=><section key={run.runId??`legacy-${index}`}>
      <h4>{text(runStatusKey(run.status))}</h4>
      {view && <p className={view.manifestApplied?"":"warning"}>{text(view.manifestApplied?"runApplied":view.outboxSaved?"runPending":"runUnsaved")}</p>}
      {view?.recordError && <MessageView message={view.recordError} language={language}/>}
      {run.error && <div className="warning"><MessageView message={run.error} language={language}/></div>}
      <p>{text("runAcceptedAt")}: {date(run.acceptedAtUnixMs)}<br/>{text("runStartedAt")}: {date(run.startedAtUnixMs)}<br/>{text("runFinishedAt")}: {date(run.finishedAtUnixMs)}</p>
      <details><summary>{text("runSettings")}</summary>{run.settings?<ul>
        <li>{text("trimTrailingSilence")}: {text(run.settings.trimTrailingSilence?"runOn":"runOff")}</li>
        <li>{text("splitLongVideo")}: {text(run.settings.splitLongVideo?"runOn":"runOff")}</li>
        <li>{text("splitThresholdMinutes")}: {run.settings.splitThresholdMinutes}</li>
        <li>{text("splitSegmentMinutes")}: {run.settings.splitSegmentMinutes}</li>
        <li>{text("transcribeAudio")}: {text(run.settings.transcribe?"runOn":"runOff")}</li>
        <li>{text("recognitionLanguage")}: {run.settings.transcriptionLocale??text("recognitionNotUsed")}</li>
      </ul>:<p>{text("runNotRecorded")}</p>}</details>
      {run.warnings.map((w,i)=><div className="warning" key={i}><MessageView message={w} language={language}/></div>)}
      {run.artifactPaths.length>0 && <><p>{text("runArtifactLimit")}</p><ul>{run.artifactPaths.map(path=>{
        const availability=(view?view.artifacts:artifacts).find(a=>a.path===path)?.availability??"unknown";
        const available=availability==="available";
        return <li key={path}><p className="history-path">{path}</p><span>{text(available?"fileAvailable":availability==="unknown"?"runAvailabilityUnknown":"fileUnavailable")}</span>{" "}<button className="secondary" type="button" disabled={busy||!available} onClick={()=>reveal(path)}>{text("showInFinder")}</button></li>;
      })}</ul></>}
      <details><summary>{text("technicalDetails")}</summary><pre>{JSON.stringify(run,null,2)}</pre></details>
    </section>)}
  </details>;
}
