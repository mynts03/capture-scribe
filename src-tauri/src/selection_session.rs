//! One app-owned selection session. GUI and control IPC share this manager.
use serde::Serialize;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionSnapshot {
    pub revision: u64,
    pub request_id: Option<String>,
    pub phase: String,
    pub helper_pid: Option<u32>,
    pub cancel_requested: bool,
    pub cancel_sent: bool,
    pub helper_exited: bool,
    pub windows_restored: Option<bool>,
    pub outcome: Option<String>,
}
impl Default for SelectionSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            request_id: None,
            phase: "idle".into(),
            helper_pid: None,
            cancel_requested: false,
            cancel_sent: false,
            helper_exited: false,
            windows_restored: None,
            outcome: None,
        }
    }
}
#[derive(Default)]
struct Inner {
    snapshot: SelectionSnapshot,
    used: HashSet<String>,
    closing: bool,
}
#[derive(Clone, Default)]
pub struct SelectionManager(Arc<Mutex<Inner>>);
pub struct SelectionSession {
    manager: SelectionManager,
    request_id: String,
    finished: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelReceipt {
    pub disposition: String,
    pub completed: bool,
    pub selection: SelectionSnapshot,
}
impl SelectionManager {
    pub fn snapshot(&self) -> SelectionSnapshot {
        self.0.lock().unwrap().snapshot.clone()
    }
    pub fn begin(&self, request_id: &str) -> Result<SelectionSession, String> {
        let mut s = self.0.lock().map_err(|_| "selection_lock_failed")?;
        if s.closing {
            return Err("app_exiting".into());
        }
        if request_id.is_empty() || request_id.len() > 256 {
            return Err("invalid_request_id".into());
        }
        if matches!(
            s.snapshot.phase.as_str(),
            "starting" | "running" | "cancelling" | "restoring"
        ) {
            return Err("selection_busy".into());
        }
        // Never let an old request identify a later session in this instance.
        if !s.used.insert(request_id.into()) {
            return Err("request_id_reused".into());
        }
        s.snapshot = SelectionSnapshot {
            revision: s.snapshot.revision + 1,
            request_id: Some(request_id.into()),
            phase: "starting".into(),
            ..Default::default()
        };
        Ok(SelectionSession {
            manager: self.clone(),
            request_id: request_id.into(),
            finished: false,
        })
    }
    pub fn cancel(&self, request_id: &str) -> Result<CancelReceipt, String> {
        let mut s = self.0.lock().map_err(|_| "selection_lock_failed")?;
        if s.snapshot.request_id.as_deref() != Some(request_id) {
            return Err("selection_request_mismatch".into());
        }
        let disposition = match s.snapshot.phase.as_str() {
            "starting" | "running" | "cancelling" => {
                if s.snapshot.cancel_requested {
                    "already_requested"
                } else {
                    s.snapshot.cancel_requested = true;
                    s.snapshot.phase = "cancelling".into();
                    s.snapshot.revision += 1;
                    "accepted"
                }
            }
            "restoring" | "finished" => "too_late",
            _ => return Err("no_active_selection".into()),
        };
        Ok(CancelReceipt {
            disposition: disposition.into(),
            completed: s.snapshot.phase == "finished",
            selection: s.snapshot.clone(),
        })
    }
    pub fn shutdown(&self) {
        let mut s = self.0.lock().unwrap();
        s.closing = true;
        if matches!(
            s.snapshot.phase.as_str(),
            "starting" | "running" | "cancelling"
        ) {
            s.snapshot.cancel_requested = true;
            s.snapshot.phase = "cancelling".into();
            s.snapshot.revision += 1;
        }
    }
}
impl SelectionSession {
    pub fn started(&self, pid: u32) {
        let mut s = self.manager.0.lock().unwrap();
        s.snapshot.helper_pid = Some(pid);
        s.snapshot.phase = if s.snapshot.cancel_requested {
            "cancelling"
        } else {
            "running"
        }
        .into();
        s.snapshot.revision += 1;
    }
    pub fn needs_cancel(&self) -> bool {
        let s = self.manager.0.lock().unwrap();
        s.snapshot.cancel_requested && !s.snapshot.cancel_sent
    }
    pub fn cancel_sent(&self) {
        let mut s = self.manager.0.lock().unwrap();
        s.snapshot.cancel_sent = true;
        s.snapshot.revision += 1;
    }
    /// Linearization point: later cancellation cannot replace a completed choice.
    /// Accepted cancellation wins over a simultaneously arriving confirmation.
    pub fn helper_finished(&self) -> bool {
        let mut s = self.manager.0.lock().unwrap();
        s.snapshot.helper_exited = true;
        s.snapshot.phase = "restoring".into();
        s.snapshot.revision += 1;
        s.snapshot.cancel_requested
    }
    pub fn finish(&mut self, outcome: &str, restored: bool) {
        let mut s = self.manager.0.lock().unwrap();
        debug_assert_eq!(
            s.snapshot.request_id.as_deref(),
            Some(self.request_id.as_str())
        );
        s.snapshot.windows_restored = Some(restored);
        s.snapshot.phase = "finished".into();
        s.snapshot.outcome = Some(outcome.into());
        s.snapshot.revision += 1;
        self.finished = true;
    }
}
impl Drop for SelectionSession {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("failed", false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_cancel_receipt_is_not_completion() {
        let m = SelectionManager::default();
        assert_eq!(m.snapshot().phase, "idle");
        let mut s = m.begin("a").unwrap();
        s.started(42);
        let r = m.cancel("a").unwrap();
        assert_eq!(r.disposition, "accepted");
        assert!(!r.completed);
        assert_eq!(m.cancel("a").unwrap().disposition, "already_requested");
        assert!(s.needs_cancel());
        s.cancel_sent();
        assert!(!s.needs_cancel());
        assert!(s.helper_finished());
        assert_eq!(m.snapshot().phase, "restoring");
        s.finish("cancelled", true);
        assert!(m.snapshot().helper_exited);
        assert_eq!(m.snapshot().windows_restored, Some(true));
        assert!(m.cancel("a").unwrap().completed);
    }
    #[test]
    fn stale_ids_busy_and_reuse_are_rejected() {
        let m = SelectionManager::default();
        let s = m.begin("a").unwrap();
        assert!(m.begin("b").is_err());
        assert!(m.cancel("old").is_err());
        drop(s);
        assert!(m.begin("a").is_err());
        let _s = m.begin("b").unwrap();
        assert!(m.cancel("a").is_err());
    }
    #[test]
    fn natural_end_and_cancel_have_one_order() {
        let m = SelectionManager::default();
        let mut s = m.begin("a").unwrap();
        s.started(1);
        assert!(!s.helper_finished());
        assert_eq!(m.cancel("a").unwrap().disposition, "too_late");
        s.finish("confirmed", true);
        assert_eq!(m.snapshot().outcome.as_deref(), Some("confirmed"));
    }
    #[test]
    fn simultaneous_cancels_and_shutdown_are_idempotent() {
        let m = SelectionManager::default();
        let mut s = m.begin("a").unwrap();
        let ts: Vec<_> = (0..8)
            .map(|_| {
                let m = m.clone();
                std::thread::spawn(move || m.cancel("a").unwrap().disposition)
            })
            .collect();
        assert_eq!(
            ts.into_iter()
                .map(|t| t.join().unwrap())
                .filter(|s| s == "accepted")
                .count(),
            1
        );
        s.started(2);
        assert!(s.needs_cancel());
        m.shutdown();
        assert!(s.helper_finished());
        s.finish("cancelled", true);
        assert!(m.begin("b").is_err());
    }
    #[test]
    fn racing_natural_completion_and_cancel_have_one_linearized_result() {
        for _ in 0..40 {
            let manager = SelectionManager::default();
            let mut session = manager.begin("race").unwrap();
            session.started(1);
            let barrier = std::sync::Barrier::new(2);
            let (receipt, cancel_won) = std::thread::scope(|scope| {
                let a = scope.spawn(|| {
                    barrier.wait();
                    manager.cancel("race").unwrap()
                });
                let b = scope.spawn(|| {
                    barrier.wait();
                    session.helper_finished()
                });
                (a.join().unwrap(), b.join().unwrap())
            });
            assert_eq!(receipt.disposition == "accepted", cancel_won);
            session.finish(if cancel_won { "cancelled" } else { "confirmed" }, true);
            assert_eq!(manager.snapshot().cancel_requested, cancel_won);
        }
    }
    #[test]
    fn failure_unwind_does_not_leave_active_session() {
        let m = SelectionManager::default();
        {
            let _s = m.begin("a").unwrap();
        }
        assert_eq!(m.snapshot().phase, "finished");
        assert_eq!(m.snapshot().outcome.as_deref(), Some("failed"));
        assert!(m.begin("b").is_ok());
    }
}

/// Retains exclusive ownership of the child's stdin and lifetime. Cancellation
/// writes only to this child; timeout/unwind escalation never targets an arbitrary PID.
struct ChildOwner {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    reaped: bool,
}
impl ChildOwner {
    fn reap(&mut self, session: &SelectionSession) -> bool {
        if self.reaped {
            return session.manager.snapshot().cancel_requested;
        }
        self.stdin.take();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        self.reaped = true;
        session.helper_finished()
    }
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        if !self.reaped {
            self.stdin.take();
            // Only an unexpected unwind reaches this fallback. Closing stdin is
            // the first cancel signal; then reclaim this owned child if needed.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    _ => {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                }
            }
        }
    }
}
pub(crate) struct HelperOutput {
    pub bytes: Vec<u8>,
    pub diagnostic: Vec<u8>,
    pub cancel_won: bool,
}
pub(crate) fn run_helper(
    mut command: std::process::Command,
    payload: &[u8],
    session: &SelectionSession,
    timeout: std::time::Duration,
) -> Result<HelperOutput, String> {
    use std::{
        io::{Read, Write},
        process::Stdio,
        time::{Duration, Instant},
    };
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut child = ChildOwner {
        child,
        stdin: None,
        reaped: false,
    };
    session.started(child.child.id());
    let result = (|| {
        child.stdin = child.child.stdin.take();
        let stdin = child.stdin.as_mut().ok_or("Selection stdin unavailable")?;
        stdin
            .write_all(payload)
            .and_then(|_| stdin.write_all(b"\n"))
            .map_err(|e| e.to_string())?;
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or("Selection stdout unavailable")?;
        let stderr = child
            .child
            .stderr
            .take()
            .ok_or("Selection stderr unavailable")?;
        let read = std::thread::spawn(move || {
            let mut b = Vec::new();
            let r = stdout.take(65537).read_to_end(&mut b);
            (r, b)
        });
        let errors = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = stderr.take(65537).read_to_end(&mut b);
            b
        });
        let start = Instant::now();
        let status = loop {
            match child.child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if start.elapsed() < timeout => {
                    if session.needs_cancel() {
                        // Data/EOF is the helper's existing cancellation protocol.
                        // Do not kill on accepted cancellation; wait for its reply.
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(b"cancel\n");
                        }
                        session.cancel_sent();
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => {
                    let _ = child.child.kill();
                    let _ = child.child.wait();
                    break Err(format!("Selection timeout or wait failure: {other:?}"));
                }
            }
        };
        child.reaped = true;
        child.stdin.take();
        let cancel_won = session.helper_finished();
        let (read_result, bytes) = read.join().map_err(|_| "Selection reader failed")?;
        let diagnostic = errors
            .join()
            .map_err(|_| "Selection stderr reader failed")?;
        read_result.map_err(|e| e.to_string())?;
        if !status?.success() || bytes.len() > 65536 {
            return Err(String::from_utf8_lossy(&diagnostic).into_owned());
        }
        Ok(HelperOutput {
            bytes,
            diagnostic,
            cancel_won,
        })
    })();
    if !child.reaped {
        child.reap(session);
    }
    result
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use std::{
        process::Command,
        time::{Duration, Instant},
    };
    fn command(code: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", code]);
        c
    }
    fn wait_running(m: &SelectionManager) {
        let end = Instant::now() + Duration::from_secs(3);
        while m.snapshot().helper_pid.is_none() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn real_pipe_cancel_reaps_helper_and_releases_gate_without_kill() {
        let m = SelectionManager::default();
        let gate = crate::operation_gate::OperationGate::default();
        let g = gate.clone();
        let state = m.clone();
        let t = std::thread::spawn(move || {
            let lease = g.acquire(crate::operation_gate::Owner::Selection).unwrap();
            let mut s = state.begin("gui-request").unwrap();
            let out = run_helper(
                command("read first; read cancel; echo cancelled"),
                b"request",
                &s,
                Duration::from_secs(3),
            )
            .unwrap();
            assert!(out.cancel_won);
            assert_eq!(out.bytes, b"cancelled\n");
            drop(lease);
            s.finish("cancelled", true);
        });
        wait_running(&m);
        assert!(m.cancel("old").is_err());
        assert_eq!(m.cancel("gui-request").unwrap().disposition, "accepted");
        t.join().unwrap();
        assert!(m.snapshot().helper_exited);
        assert!(m.snapshot().cancel_sent);
        assert!(gate.acquire(crate::operation_gate::Owner::Capture).is_ok());
    }
    #[test]
    fn natural_exit_abnormal_exit_spawn_failure_and_timeout_clear_state() {
        for (i, code) in [
            "read first; echo confirmed",
            "read first; exit 7",
            "read first; read second",
        ]
        .iter()
        .enumerate()
        {
            let m = SelectionManager::default();
            let mut s = m.begin("x").unwrap();
            let r = run_helper(command(code), b"request", &s, Duration::from_millis(80));
            assert_eq!(r.is_ok(), i == 0);
            assert!(m.snapshot().helper_exited);
            s.finish(if r.is_ok() { "confirmed" } else { "failed" }, true);
            assert!(m.begin("next").is_ok());
        }
        let m = SelectionManager::default();
        {
            let s = m.begin("x").unwrap();
            assert!(run_helper(
                Command::new("/missing-cs-helper"),
                b"request",
                &s,
                Duration::from_secs(1)
            )
            .is_err());
        }
        assert!(m.begin("next").is_ok());
    }
    #[test]
    fn accepted_cancel_wins_over_confirmation_and_shutdown_uses_stdin() {
        for shutdown in [false, true] {
            let m = SelectionManager::default();
            let state = m.clone();
            let t = std::thread::spawn(move || {
                let mut s = state.begin("x").unwrap();
                let r = run_helper(
                    command("read first; read cancel; echo confirmed"),
                    b"request",
                    &s,
                    Duration::from_secs(3),
                )
                .unwrap();
                assert!(r.cancel_won);
                s.finish("cancelled", true);
            });
            wait_running(&m);
            if shutdown {
                m.shutdown()
            } else {
                m.cancel("x").unwrap();
            }
            t.join().unwrap();
            assert_eq!(m.snapshot().outcome.as_deref(), Some("cancelled"));
        }
    }
}
