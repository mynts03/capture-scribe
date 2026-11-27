//! One app-owned execution, not a queue. Execution owns durable terminalization.
use crate::{
    message::AppMessage,
    operation_gate::{OperationGate, Owner},
    postprocess::{PostProcessRequest, PostProcessResult, Progress},
    processing_runs::Execution,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingSnapshot {
    pub session_id: String,
    pub revision: u64,
    pub request_id: Option<String>,
    pub run_id: Option<String>,
    pub state: String,
    pub admission_blocked: bool,
    pub active: bool,
    pub stopping: bool,
    pub request: Option<PostProcessRequest>,
    pub message: Option<AppMessage>,
    pub result: Option<PostProcessResult>,
}
struct Inner {
    snapshot: ProcessingSnapshot,
    seen: HashMap<String, PostProcessRequest>,
    stop: Arc<AtomicBool>,
    quarantine: Option<crate::operation_gate::Lease>,
}
#[derive(Clone)]
pub struct ProcessingManager {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    gate: OperationGate,
}
pub type Notify = Arc<dyn Fn(ProcessingSnapshot) + Send + Sync>;
impl ProcessingManager {
    pub fn new(gate: OperationGate) -> Self {
        Self {
            gate,
            inner: Arc::new((
                Mutex::new(Inner {
                    snapshot: ProcessingSnapshot {
                        session_id: crate::history_index::token(),
                        revision: 0,
                        request_id: None,
                        run_id: None,
                        state: "idle".into(),
                        admission_blocked: false,
                        active: false,
                        stopping: false,
                        request: None,
                        message: None,
                        result: None,
                    },
                    seen: HashMap::new(),
                    stop: Arc::new(AtomicBool::new(false)),
                    quarantine: None,
                }),
                Condvar::new(),
            )),
        }
    }
    pub fn snapshot(&self) -> ProcessingSnapshot {
        self.inner.0.lock().unwrap().snapshot.clone()
    }
    pub fn submit(
        &self,
        root: &Path,
        id: String,
        request: PostProcessRequest,
        notify: Notify,
        work: impl FnOnce(Execution, &PostProcessRequest, &AtomicBool, Progress) -> PostProcessResult
            + Send
            + 'static,
    ) -> Result<ProcessingSnapshot, AppMessage> {
        if !crate::processing_runs::valid_id(&id) {
            return Err(AppMessage::known(
                "processing.request_invalid",
                "Invalid request ID",
            ));
        }
        let mut s = self.inner.0.lock().unwrap();
        if s.snapshot.active
            && s.snapshot.request_id.as_ref() == Some(&id)
            && s.snapshot.request.as_ref() == Some(&request)
        {
            return Ok(s.snapshot.clone());
        }
        if let Some(old) = s.seen.get(&id) {
            return if old == &request && s.snapshot.request_id.as_ref() == Some(&id) {
                Ok(s.snapshot.clone())
            } else {
                Err(AppMessage::known(
                    "processing.request_duplicate",
                    "Request ID already used; no worker started",
                ))
            };
        }
        let lease = self.gate.acquire(Owner::Processing)?;
        s.stop = Arc::new(AtomicBool::new(false));
        let stop = s.stop.clone();
        let revision = s.snapshot.revision + 1;
        let session = s.snapshot.session_id.clone();
        s.snapshot = ProcessingSnapshot {
            session_id: session,
            revision,
            request_id: Some(id.clone()),
            run_id: None,
            state: "preparing".into(),
            admission_blocked: false,
            active: true,
            stopping: false,
            request: Some(request.clone()),
            message: Some(AppMessage::progress("processing.preparing")),
            result: None,
        };
        let preparing = s.snapshot.clone();
        drop(s);
        notify(preparing);
        // No shared-state mutex spans filesystem IO on the source volume.
        let execution = match crate::postprocess::prepare(root, &request) {
            Ok(e) => e,
            Err(error) => {
                let mut s = self.inner.0.lock().unwrap();
                s.snapshot.active = false;
                s.snapshot.state = "rejected".into();
                s.snapshot.message = Some(error.clone());
                s.snapshot.revision += 1;
                drop(lease);
                let snapshot = s.snapshot.clone();
                self.inner.1.notify_all();
                drop(s);
                notify(snapshot);
                return Err(error);
            }
        };
        let mut s = self.inner.0.lock().unwrap();
        s.seen.insert(id, request.clone());
        s.snapshot.run_id = execution.run().run_id.clone();
        s.snapshot.state = "accepted".into();
        s.snapshot.revision += 1;
        let accepted = s.snapshot.clone();
        self.inner.1.notify_all();
        drop(s);
        notify(accepted.clone());
        let manager = self.clone();
        let run_id = accepted.run_id.clone();
        std::thread::spawn(move || {
            let progress_manager = manager.clone();
            let progress_notify = notify.clone();
            let progress_id = run_id.clone();
            let progress: Progress = Arc::new(move |m| {
                let mut s = progress_manager.inner.0.lock().unwrap();
                if s.snapshot.run_id != progress_id || !s.snapshot.active {
                    return;
                }
                s.snapshot.message = Some(m);
                if !s.snapshot.stopping {
                    s.snapshot.state = "running".into();
                }
                s.snapshot.revision += 1;
                let snap = s.snapshot.clone();
                drop(s);
                progress_notify(snap);
            });
            // One terminal owner; no React callback and no stop requester writes run history.
            let result = work(execution, &request, &stop, progress);
            let mut s = manager.inner.0.lock().unwrap();
            let uncertain = matches!(result.status.as_str(), "unknown" | "interrupted");
            s.snapshot.state = result.status.clone();
            s.snapshot.admission_blocked = uncertain;
            s.snapshot.result = Some(result);
            s.snapshot.active = false;
            s.snapshot.stopping = false;
            s.snapshot.revision += 1;
            if uncertain {
                s.quarantine = Some(lease);
            } else {
                drop(lease);
            } // Unknown ownership must not admit another media job.
            let snapshot = s.snapshot.clone();
            manager.inner.1.notify_all();
            drop(s);
            notify(snapshot);
        });
        Ok(accepted)
    }
    pub fn stop(&self, run_id: &str) -> Result<ProcessingSnapshot, AppMessage> {
        let mut s = self.inner.0.lock().unwrap();
        if s.snapshot.run_id.as_deref() != Some(run_id) {
            return Err(AppMessage::known(
                "processing.run_mismatch",
                "Stop request does not match owned run",
            ));
        }
        if s.snapshot.active && !s.stop.swap(true, Ordering::SeqCst) {
            s.snapshot.stopping = true;
            s.snapshot.state = "stopping".into();
            s.snapshot.revision += 1;
        }
        Ok(s.snapshot.clone())
    }
    pub fn wait_terminal(&self, id: &str, timeout: Duration) -> Option<ProcessingSnapshot> {
        let deadline = Instant::now() + timeout;
        let mut s = self.inner.0.lock().unwrap();
        while s.snapshot.run_id.as_deref() == Some(id) && s.snapshot.active {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            s = self.inner.1.wait_timeout(s, deadline - now).unwrap().0;
        }
        if s.snapshot.run_id.as_deref() == Some(id) {
            Some(s.snapshot.clone())
        } else {
            None
        }
    }
    pub fn wait_identity(&self, request_id: &str, timeout: Duration) -> Option<ProcessingSnapshot> {
        let deadline = Instant::now() + timeout;
        let mut s = self.inner.0.lock().unwrap();
        while s.snapshot.request_id.as_deref() == Some(request_id)
            && s.snapshot.active
            && s.snapshot.run_id.is_none()
        {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            s = self.inner.1.wait_timeout(s, deadline - now).unwrap().0;
        }
        (s.snapshot.request_id.as_deref() == Some(request_id)).then(|| s.snapshot.clone())
    }
}
#[tauri::command]
pub fn processing_snapshot(manager: tauri::State<'_, ProcessingManager>) -> ProcessingSnapshot {
    manager.snapshot()
}
#[tauri::command]
pub fn stop_processing(
    manager: tauri::State<'_, ProcessingManager>,
    run_id: String,
) -> Result<ProcessingSnapshot, AppMessage> {
    manager.stop(&run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processing_runs::tests::Fixture;
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run alone: deliberate fork inherits parallel fixtures' flock descriptors"]
    fn inherited_flock_is_active_until_the_owned_fork_exits() {
        let f = Fixture::new();
        let mut e = f.execution();
        e.start().unwrap();
        // Child uses only async-signal-safe functions; no Rust/ObjC after fork.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                let delay = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 150_000_000,
                };
                libc::nanosleep(&delay, std::ptr::null_mut());
                libc::_exit(0);
            }
        }
        drop(e);
        let (during, w) = crate::processing_runs::reconcile(&f.history);
        assert!(w.is_empty());
        assert!(during[0].active);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        let (after, w) = crate::processing_runs::reconcile(&f.history);
        assert!(w.is_empty());
        assert!(!after[0].active);
        assert_eq!(after[0].run.status, "unknown");
    }
    fn request(f: &Fixture) -> PostProcessRequest {
        serde_json::from_value(serde_json::json!({"manifestPath":f.path,"trimTrailingSilence":false,"splitLongVideo":false,"splitThresholdMinutes":10,"splitSegmentMinutes":7,"transcribe":false})).unwrap()
    }
    fn result(e: &Execution, status: &str) -> PostProcessResult {
        serde_json::from_value(serde_json::json!({"status":status,"manifestPath":e.identity().manifest_path,"sourceMaster":e.identity().source_master_path,"sourceDurationSeconds":12,"videoOutputs":[],"transcripts":[],"trim":{},"split":{},"artifacts":[],"warnings":[],"elapsedSeconds":0.1})).unwrap()
    }
    #[test]
    fn requests_snapshots_stop_and_settings_have_one_terminal_owner() {
        let f = Fixture::new();
        let original = std::fs::read(&f.source).unwrap();
        let gate = OperationGate::default();
        let m = ProcessingManager::new(gate.clone());
        let input = request(&f);
        let (started, wait) = std::sync::mpsc::channel();
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let n = notifications.clone();
        let first = m
            .submit(
                &f.history,
                "request-one".into(),
                input.clone(),
                Arc::new(move |s| n.lock().unwrap().push(s)),
                move |e, request, stop, progress| {
                    crate::postprocess::execute_prepared(e, |_e| {
                        started.send(request.clone()).unwrap();
                        progress(AppMessage::progress("processing.worker_starting"));
                        while !stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(AppMessage::known(
                            "processing.cancelled",
                            "Fixture stop confirmed",
                        ))
                    })
                },
            )
            .unwrap();
        let fixed = wait.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(fixed, input);
        assert!(first.active);
        assert_eq!(gate.owner(), Some(Owner::Processing));
        assert!(gate.acquire(Owner::Capture).is_err());
        assert_eq!(
            m.submit(
                &f.history,
                "request-one".into(),
                input.clone(),
                Arc::new(|_| {}),
                |_, _, _, _| panic!()
            )
            .unwrap()
            .run_id,
            first.run_id
        );
        assert!(m
            .submit(
                &f.history,
                "request-two".into(),
                input.clone(),
                Arc::new(|_| {}),
                |_, _, _, _| panic!()
            )
            .is_err());
        let mut changed = input.clone();
        changed.manifest_path = "other".into();
        assert!(m
            .submit(
                &f.history,
                "request-one".into(),
                changed,
                Arc::new(|_| {}),
                |_, _, _, _| panic!()
            )
            .is_err());
        assert_eq!(m.snapshot().request, Some(input));
        assert!(m.stop("different-run").is_err());
        let id = first.run_id.unwrap();
        let stopped = m.stop(&id).unwrap();
        let again = m.stop(&id).unwrap();
        assert_eq!(stopped.revision, again.revision);
        let done = m.wait_terminal(&id, Duration::from_secs(2)).unwrap();
        assert!(!done.active);
        let v = done.result.unwrap().run.unwrap();
        assert_eq!(v.run.status, "cancelled");
        assert!(v.outbox_saved && v.manifest_applied);
        assert_eq!(gate.owner(), None);
        assert_eq!(std::fs::read(&f.source).unwrap(), original);
        assert!(notifications
            .lock()
            .unwrap()
            .windows(2)
            .all(|w| w[0].revision < w[1].revision));
    }
    #[test]
    fn successful_natural_result_is_not_changed_by_a_late_stop() {
        let f = Fixture::new();
        let m = ProcessingManager::new(OperationGate::default());
        let s = m
            .submit(
                &f.history,
                "natural".into(),
                request(&f),
                Arc::new(|_| {}),
                |e, _, _, _| {
                    crate::postprocess::execute_prepared(e, |e| Ok(result(e, "completed")))
                },
            )
            .unwrap();
        let id = s.run_id.unwrap();
        let done = m.wait_terminal(&id, Duration::from_secs(2)).unwrap();
        assert_eq!(done.result.unwrap().run.unwrap().run.status, "completed");
        assert!(!m.stop(&id).unwrap().active);
    }
    #[test]
    fn rejected_input_does_not_hold_admission_and_unknown_does_not_claim_safety() {
        let f = Fixture::new();
        let g = OperationGate::default();
        let m = ProcessingManager::new(g.clone());
        let mut invalid = request(&f);
        invalid.manifest_path = "/nonexistent-fixture".into();
        assert!(m
            .submit(
                &f.history,
                "invalid".into(),
                invalid,
                Arc::new(|_| {}),
                |_, _, _, _| panic!()
            )
            .is_err());
        assert_eq!(g.owner(), None);
        let s = m
            .submit(
                &f.history,
                "unknown".into(),
                request(&f),
                Arc::new(|_| {}),
                |e, _, _, _| {
                    crate::postprocess::execute_prepared(e, |_| {
                        Err(AppMessage::known(
                            "processing.completion_unknown",
                            "Fixture child state unknown",
                        ))
                    })
                },
            )
            .unwrap();
        let done = m
            .wait_terminal(&s.run_id.unwrap(), Duration::from_secs(2))
            .unwrap();
        let run = done.result.unwrap().run.unwrap().run;
        assert_eq!(run.status, "unknown");
        assert!(run.finished_at_unix_ms.is_none());
        assert!(g.acquire(Owner::Capture).is_err());
    }
    #[test]
    fn wait_timeout_does_not_fabricate_a_terminal_result() {
        let f = Fixture::new();
        let m = ProcessingManager::new(OperationGate::default());
        let (send, receive) = std::sync::mpsc::channel();
        let s = m
            .submit(
                &f.history,
                "delayed".into(),
                request(&f),
                Arc::new(|_| {}),
                move |e, _, _, _| {
                    receive.recv().unwrap();
                    crate::postprocess::execute_prepared(e, |e| Ok(result(e, "completed")))
                },
            )
            .unwrap();
        let id = s.run_id.unwrap();
        assert!(m.wait_terminal(&id, Duration::from_millis(5)).is_none());
        assert!(m.snapshot().active);
        send.send(()).unwrap();
        assert!(m.wait_terminal(&id, Duration::from_secs(2)).is_some());
    }
}
