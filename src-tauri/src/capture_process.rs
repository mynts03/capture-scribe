//! One owner waits for each child. UI/control threads never wait under a state lock.
use crate::message::AppMessage;
use std::io::Write;
use std::process::{Child, ExitStatus};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub fn wait_for_exit(
    child: &mut Child,
    stop: Receiver<()>,
    stopping: impl FnOnce(),
) -> Result<ExitStatus, AppMessage> {
    wait_with_timeout(child, stop, stopping, Duration::from_secs(30))
}

fn wait_with_timeout(
    child: &mut Child,
    stop: Receiver<()>,
    stopping: impl FnOnce(),
    timeout: Duration,
) -> Result<ExitStatus, AppMessage> {
    let mut stopping = Some(stopping);
    let mut stop_started = None;
    loop {
        if let Some(exit) = child
            .try_wait()
            .map_err(|e| AppMessage::known("capture.helper_failed", e.to_string()))?
        {
            return Ok(exit);
        }
        if stop_started.is_some_and(|start: Instant| start.elapsed() >= timeout) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppMessage::known(
                "capture.stop_timeout",
                "Capture helper did not finish within 30 seconds; working files retained",
            ));
        }
        match stop.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) if stop_started.is_none() => {
                stop_started = Some(Instant::now());
                if let Some(notify) = stopping.take() {
                    notify();
                }
                // A deadline exit can race this write. An EPIPE is not itself
                // success/failure: the actual exit/report/media decide below.
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(b"stop\n");
                    let _ = stdin.flush();
                }
            }
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(100)),
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    };

    #[test]
    fn manual_deadline_and_nonzero_exit_are_distinguished() {
        for (script, manual, success) in [
            ("read command; exit 0", true, true),
            ("sleep 0.03; exit 0", false, true),
            ("exit 7", false, false),
        ] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let (tx, rx) = mpsc::channel();
            if manual {
                tx.send(()).unwrap();
                tx.send(()).unwrap();
            }
            let count = AtomicUsize::new(0);
            let exit = wait_for_exit(&mut child, rx, || {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
            assert_eq!(exit.success(), success);
            assert!(count.load(Ordering::SeqCst) <= 1);
        }
    }

    #[test]
    fn deadline_exit_racing_a_manual_stop_is_still_one_successful_exit() {
        // The helper can exit before, during or after the stop request. The
        // owner must reap it once, including the closed-stdin/EPIPE case.
        for delay in [0, 15, 30] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", "sleep 0.015; exit 0"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let (tx, rx) = mpsc::channel();
            let sender = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                let _ = tx.send(());
            });
            let notifications = AtomicUsize::new(0);
            assert!(wait_for_exit(&mut child, rx, || {
                notifications.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap()
            .success());
            sender.join().unwrap();
            assert!(notifications.load(Ordering::SeqCst) <= 1);
        }
    }

    #[test]
    fn simultaneous_stop_requests_do_not_duplicate_child_finalization() {
        let root = std::env::temp_dir().join(format!(
            "capturescribe-stop-race-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let count = root.join("done");
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read command; printf done > \"$1\"", "fixture"])
            .arg(&count)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let notified = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let tx = tx.clone();
                s.spawn(move || {
                    let _ = tx.send(());
                });
            }
            assert!(wait_for_exit(&mut child, rx, || {
                notified.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap()
            .success());
        });
        assert_eq!(notified.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&count).unwrap(), b"done");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hanging_stop_has_a_bound_and_disconnected_control_stops() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 10"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        drop(tx);
        let error =
            wait_with_timeout(&mut child, rx, || {}, Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("capture.stop_timeout"));
        assert!(child.try_wait().unwrap().is_some());
    }
}
