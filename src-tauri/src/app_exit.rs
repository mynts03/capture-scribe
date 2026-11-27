//! All application-quit entry points share one decision state, independent of busy().
use crate::{
    capture::CaptureManager,
    message::AppMessage,
    operation_gate::{OperationGate, Owner},
    processing_manager::{ProcessingManager, ProcessingSnapshot},
};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager};
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitSnapshot {
    pub revision: u64,
    pub phase: String,
    pub message: Option<AppMessage>,
    pub capture_failure: Option<crate::capture_failures::CaptureFailureView>,
    pub run_id: Option<String>,
    pub request_id: Option<String>,
}
#[derive(Clone)]
pub struct ExitManager {
    state: Arc<Mutex<ExitSnapshot>>,
    gate: OperationGate,
}
enum Decision {
    Back,
    Exit,
    Stop(Option<String>),
}
impl ExitManager {
    pub fn new(gate: OperationGate) -> Self {
        Self {
            gate,
            state: Arc::new(Mutex::new(ExitSnapshot {
                revision: 0,
                phase: "idle".into(),
                message: None,
                capture_failure: None,
                run_id: None,
                request_id: None,
            })),
        }
    }
    pub fn snapshot(&self) -> ExitSnapshot {
        self.state.lock().unwrap().clone()
    }
    fn publish(&self, app: &AppHandle) {
        // A quit request during selection is still rejected/published, but must
        // not reveal the product over the separate helper's input surface.
        if !crate::capture_area::SELECTING.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }
        let _ = app.emit("app-exit-state", self.snapshot());
    }
    fn phase(&self, phase: &str) {
        let mut s = self.state.lock().unwrap();
        s.phase = phase.into();
        s.revision += 1;
    }
    fn allow(&self, app: &AppHandle) {
        self.phase("allowed");
        app.exit(0);
    }
    fn issue_state(
        &self,
        error: AppMessage,
        failure: Option<crate::capture_failures::CaptureFailureView>,
    ) {
        let mut s = self.state.lock().unwrap();
        s.phase = "issue".into();
        s.message = Some(error);
        s.capture_failure = failure;
        s.revision += 1;
    }
    fn issue(&self, app: &AppHandle, error: AppMessage) {
        let failure = if self.snapshot().phase == "stopping-capture" {
            crate::capture::failure_snapshot(&app.state::<CaptureManager>())
        } else {
            None
        };
        self.issue_state(error, failure);
        self.publish(app);
    }
    fn begin(&self) -> &'static str {
        let mut s = self.state.lock().unwrap();
        if s.phase == "allowed" {
            return "allowed";
        }
        if s.phase != "idle" {
            return "pending";
        }
        s.phase = "requested".into();
        s.revision += 1;
        self.gate.quitting(true);
        "new"
    }
    // Admission can remain held after an uncertain stop; it is not proof of live work.
    // True means completion is known and request() can allow quitting without a prompt.
    fn processing_state(&self, run: ProcessingSnapshot) -> bool {
        if run.admission_blocked || matches!(run.state.as_str(), "unknown" | "interrupted") {
            let error = run
                .result
                .and_then(|result| result.run)
                .and_then(|view| view.run.error)
                .unwrap_or_else(|| {
                    AppMessage::known(
                        "processing.completion_unknown",
                        "Processing termination was not confirmed; owned processes may remain",
                    )
                });
            self.issue_state(error, None);
            false
        } else if run.active {
            let mut s = self.state.lock().unwrap();
            s.phase = "confirming".into();
            s.run_id = run.run_id;
            s.request_id = run.request_id;
            s.revision += 1;
            false
        } else {
            true
        }
    }
    // Returns true only after a decision has allowed application termination.
    pub fn request(&self, app: AppHandle) -> bool {
        match self.begin() {
            "allowed" => return true,
            "pending" => {
                self.publish(&app);
                return false;
            }
            _ => {}
        }
        match self.gate.owner() {
            Some(Owner::Capture) => {
                self.phase("stopping-capture");
                self.publish(&app);
                let this = self.clone();
                let a = app.clone();
                app.state::<CaptureManager>()
                    .request_exit(app.clone(), move |r| match r {
                        Ok(()) => this.allow(&a),
                        Err(e) => this.issue(&a, e),
                    });
            }
            Some(Owner::Processing) => {
                let run = app.state::<ProcessingManager>().snapshot();
                if self.processing_state(run) {
                    self.allow(&app);
                } else {
                    self.publish(&app);
                }
            }
            Some(Owner::Selection) => self.issue(
                &app,
                AppMessage::known("operation.busy", "Close the range selector before quitting"),
            ),
            None => self.allow(&app),
        }
        false
    }
    pub fn choose(
        &self,
        app: AppHandle,
        revision: u64,
        choice: &str,
    ) -> Result<ExitSnapshot, AppMessage> {
        match self.decide(revision, choice)? {
            Decision::Back => self.publish(&app),
            Decision::Exit => self.allow(&app),
            Decision::Stop(id) => {
                self.publish(&app);
                let this = self.clone();
                let request_id = self.snapshot().request_id;
                std::thread::spawn(move || {
                    let manager = app.state::<ProcessingManager>();
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    let id = if id.is_some() {
                        id
                    } else {
                        request_id
                            .and_then(|id| manager.wait_identity(&id, Duration::from_secs(10)))
                            .and_then(|snapshot| snapshot.run_id)
                    };
                    let Some(id) = id else {
                        if crate::operation_gate::OperationGate::owner(
                            &app.state::<OperationGate>(),
                        )
                        .is_none()
                        {
                            this.allow(&app);
                            return;
                        }
                        this.issue(
                            &app,
                            AppMessage::known(
                                "processing.completion_unknown",
                                "Accepted run identity not available",
                            ),
                        );
                        return;
                    };
                    if let Err(e) = manager.stop(&id) {
                        this.issue(&app, e);
                        return;
                    }
                    match manager.wait_terminal(&id,deadline.saturating_duration_since(std::time::Instant::now())){
                        Some(s)=>match s.result.and_then(|r|r.run){
                            Some(v) if !matches!(v.run.status.as_str(),"unknown"|"interrupted")&&v.outbox_saved&&v.record_error.is_none()=>this.allow(&app),
                            Some(v)=>this.issue(&app,v.record_error.or(v.run.error).unwrap_or_else(||AppMessage::known("processing.completion_unknown","Termination or durable recording was not confirmed"))),
                            None=>this.issue(&app,AppMessage::known("processing.completion_unknown","Terminal record unavailable")),
                        },
                        None=>this.issue(&app,AppMessage::known("processing.completion_unknown","Worker stop/terminal recording not confirmed within 10 seconds; owned processes may remain")),
                    }
                });
            }
        }
        Ok(self.snapshot())
    }
    fn decide(&self, revision: u64, choice: &str) -> Result<Decision, AppMessage> {
        let mut s = self.state.lock().unwrap();
        if s.revision != revision {
            return Err(AppMessage::known(
                "exit.stale",
                "Quit prompt changed; inspect the current prompt",
            ));
        }
        match (s.phase.as_str(), choice) {
            ("confirming", "continue") | ("issue", "back") => {
                s.phase = "idle".into();
                s.message = None;
                s.capture_failure = None;
                s.run_id = None;
                s.request_id = None;
                s.revision += 1;
                self.gate.quitting(false);
                Ok(Decision::Back)
            }
            ("issue", "exit") => {
                s.phase = "allowed".into();
                s.revision += 1;
                Ok(Decision::Exit)
            }
            ("confirming", "stop") => {
                let id = s.run_id.clone();
                s.phase = "stopping-processing".into();
                s.revision += 1;
                Ok(Decision::Stop(id))
            }
            _ => Err(AppMessage::known(
                "exit.stale",
                "Quit action not applicable",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn processing_snapshot(gate: &OperationGate, state: &str) -> ProcessingSnapshot {
        let mut snapshot = ProcessingManager::new(gate.clone()).snapshot();
        snapshot.state = state.into();
        snapshot.run_id = Some("owned-run".into());
        snapshot.request_id = Some("owned-request".into());
        snapshot
    }
    #[test]
    fn active_processing_still_offers_continue_or_stop() {
        for state in ["preparing", "accepted", "running", "stopping"] {
            let gate = OperationGate::default();
            let _lease = gate.acquire(Owner::Processing).unwrap();
            let exit = ExitManager::new(gate.clone());
            let mut run = processing_snapshot(&gate, state);
            run.active = true;
            assert_eq!(exit.begin(), "new");
            assert!(!exit.processing_state(run));
            let prompt = exit.snapshot();
            assert_eq!(prompt.phase, "confirming");
            assert_eq!(prompt.run_id.as_deref(), Some("owned-run"));
            assert_eq!(prompt.request_id.as_deref(), Some("owned-request"));
            assert!(matches!(
                exit.decide(prompt.revision, "continue"),
                Ok(Decision::Back)
            ));
            assert_eq!(gate.owner(), Some(Owner::Processing));
        }
    }
    #[test]
    fn unknown_stop_then_back_and_quit_again_never_offers_a_second_stop() {
        for state in ["unknown", "interrupted"] {
            let gate = OperationGate::default();
            let _quarantine = gate.acquire(Owner::Processing).unwrap();
            let exit = ExitManager::new(gate.clone());
            let mut run = processing_snapshot(&gate, state);
            run.admission_blocked = true;
            for _ in 0..2 {
                assert_eq!(exit.begin(), "new");
                assert!(!exit.processing_state(run.clone()));
                let prompt = exit.snapshot();
                assert_eq!(prompt.phase, "issue");
                assert_eq!(
                    prompt.message.unwrap().code.as_deref(),
                    Some("processing.completion_unknown")
                );
                for _ in 0..3 {
                    assert_eq!(exit.begin(), "pending");
                    assert_eq!(exit.snapshot().revision, prompt.revision);
                }
                assert!(exit.decide(prompt.revision, "stop").is_err());
                assert!(matches!(
                    exit.decide(prompt.revision, "back"),
                    Ok(Decision::Back)
                ));
                assert_eq!(gate.owner(), Some(Owner::Processing));
            }
            assert_eq!(exit.begin(), "new");
            assert!(!exit.processing_state(run));
            assert!(matches!(
                exit.decide(exit.snapshot().revision, "exit"),
                Ok(Decision::Exit)
            ));
            assert_eq!(exit.begin(), "allowed");
        }
    }
    #[test]
    fn confirmed_terminal_processing_does_not_repeat_the_stop_prompt() {
        for state in [
            "idle",
            "completed",
            "partial",
            "failed",
            "cancelled",
            "rejected",
        ] {
            let gate = OperationGate::default();
            // Cover completion between gate.owner() and snapshot(), before request() allows exit.
            let _lease = gate.acquire(Owner::Processing).unwrap();
            let exit = ExitManager::new(gate.clone());
            assert_eq!(exit.begin(), "new");
            assert!(exit.processing_state(processing_snapshot(&gate, state)));
            assert_eq!(exit.snapshot().phase, "requested");
            exit.phase("allowed");
            assert_eq!(exit.begin(), "allowed");
        }
    }
    #[test]
    fn duplicate_quit_sources_and_allowed_bypass_are_one_decision() {
        let g = OperationGate::default();
        let e = ExitManager::new(g.clone());
        assert_eq!(e.begin(), "new");
        let rev = e.snapshot().revision;
        for _ in 0..5 {
            assert_eq!(e.begin(), "pending");
            assert_eq!(e.snapshot().revision, rev);
        }
        assert!(g.acquire(Owner::Processing).is_err());
        e.phase("allowed");
        assert_eq!(e.begin(), "allowed");
    }
    #[test]
    fn failed_capture_quit_stays_held_even_when_capture_is_not_busy() {
        let g = OperationGate::default();
        let e = ExitManager::new(g.clone());
        assert_eq!(e.begin(), "new");
        e.phase("stopping-capture");
        e.issue_state(
            AppMessage::known("capture.validation_failed", "fixture failure"),
            None,
        );
        assert_eq!(g.owner(), None);
        assert_eq!(e.begin(), "pending");
        assert_eq!(e.snapshot().phase, "issue");
        assert!(g.acquire(Owner::Capture).is_err());
    }
    #[test]
    fn continue_stop_and_back_exit_are_distinct_revision_guarded_decisions() {
        let g = OperationGate::default();
        let e = ExitManager::new(g.clone());
        let lease = g.acquire(Owner::Processing).unwrap();
        e.begin();
        e.phase("confirming");
        let rev = e.snapshot().revision;
        assert!(e.decide(rev - 1, "stop").is_err());
        assert!(matches!(e.decide(rev, "continue").unwrap(), Decision::Back));
        assert_eq!(g.owner(), Some(Owner::Processing));
        assert_eq!(e.snapshot().phase, "idle");
        e.begin();
        e.phase("confirming");
        assert!(matches!(
            e.decide(e.snapshot().revision, "stop").unwrap(),
            Decision::Stop(_)
        ));
        assert!(e.decide(e.snapshot().revision, "continue").is_err());
        e.issue_state(
            AppMessage::known("processing.completion_unknown", "fixture timeout"),
            None,
        );
        assert!(matches!(
            e.decide(e.snapshot().revision, "back").unwrap(),
            Decision::Back
        ));
        e.begin();
        e.issue_state(
            AppMessage::known("processing.record_unavailable", "fixture write failure"),
            None,
        );
        assert!(matches!(
            e.decide(e.snapshot().revision, "exit").unwrap(),
            Decision::Exit
        ));
        assert_eq!(e.begin(), "allowed");
        drop(lease);
    }
}
#[tauri::command]
pub fn app_exit_snapshot(manager: tauri::State<'_, ExitManager>) -> ExitSnapshot {
    manager.snapshot()
}
#[tauri::command]
pub fn choose_app_exit(
    app: AppHandle,
    manager: tauri::State<'_, ExitManager>,
    revision: u64,
    choice: String,
) -> Result<ExitSnapshot, AppMessage> {
    manager.choose(app, revision, &choice)
}
