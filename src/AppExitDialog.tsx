import { useEffect, useRef } from "react";
import { translate, formatMessage, type Locale } from "./i18n";
import type { ExitSnapshot } from "./types";
import { CaptureFailureView } from "./CaptureFailureView";
export function AppExitDialog({state,language,choose}:{state:ExitSnapshot|null;language:Locale;choose:(choice:string)=>void}){
  const dialog=useRef<HTMLDialogElement>(null);
  const visible=!!state&&!['idle','allowed'].includes(state.phase);
  useEffect(()=>{if(visible&&!dialog.current?.open)dialog.current?.showModal();if(!visible&&dialog.current?.open)dialog.current.close();},[visible]);
  const text=(key:Parameters<typeof translate>[1])=>translate(language,key);
  const confirming=state?.phase==='confirming',issue=state?.phase==='issue';
  return <dialog ref={dialog} className="split-dialog app-exit-dialog" aria-labelledby="app-exit-heading" onCancel={event=>{event.preventDefault();if(confirming||issue)choose(confirming?'continue':'back');}}>
    <h2 id="app-exit-heading">{text(confirming?'exitProcessingQuestion':issue?'exitIssueTitle':'exitWaiting')}</h2>
    {issue&&<p>{text('exitRisk')}</p>}
    {state?.message&&<div role="alert">{formatMessage(state.message,language)}</div>}
    {state?.message&&<details className="message-details"><summary>{text('technicalDetails')}</summary><pre>{JSON.stringify(state.message,null,2)}</pre></details>}
    {state?.captureFailure&&<CaptureFailureView failure={state.captureFailure} language={language} busy={true} reveal={()=>{}}/>}
    <div className="split-dialog-actions">
      {(confirming||issue)&&<button type="button" className="secondary" onClick={()=>choose(confirming?'continue':'back')}>{text(confirming?'exitContinue':'exitBack')}</button>}
      {(confirming||issue)&&<button type="button" className="secondary" onClick={()=>choose(confirming?'stop':'exit')}>{text(confirming?'exitStop':'exitAnyway')}</button>}
    </div>
  </dialog>;
}
