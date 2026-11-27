use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::capture_engine::{selected_engine, CaptureLaunch, DisplayInfo};
use crate::message::AppMessage;
use crate::recording_names::{finalize_recording, reserve_name, NameReservation};
use crate::save_destination::{validated_stem, DirectoryGuard};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCaptureRequest {
    output_directory: String,
    #[serde(default)]
    subfolder: String,
    file_name: String,
    display_id: u32,
    system_audio: bool,
    microphone: bool,
    max_duration_seconds: u64,
    #[serde(default)]
    area: Option<crate::capture_area::CaptureArea>,
    #[serde(default)]
    approved_output: Option<crate::capture_area::OutputSize>,
    #[serde(default)]
    temporary_target: Option<crate::capture_target::TargetTicket>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatus {
    state: String,
    started_at_unix_ms: Option<u128>,
    working_path: Option<String>,
    final_path: Option<String>,
    message: Option<AppMessage>,
}

impl CaptureStatus {
    fn idle() -> Self {
        Self {
            state: "idle".into(),
            started_at_unix_ms: None,
            working_path: None,
            final_path: None,
            message: Some(AppMessage::progress("capture.ready")),
        }
    }
}

/// Public control response. Do not serialize the GUI status or its message directly.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlCaptureStatus {
    state: String,
    started_at_unix_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopCaptureResult {
    recorded_at_unix_ms: u128,
    history_warnings: Vec<AppMessage>,
    status: CaptureStatus,
    validated: bool,
    duration_seconds: Option<f64>,
    video_stream_count: usize,
    audio_stream_count: usize,
    manifest_path: String,
    capture_geometry: Option<Value>,
}

#[derive(Debug, Clone)]
struct ActiveCapture {
    attempt_id: String,
    destination: DirectoryGuard,
    reservation: NameReservation,
    requested_stem: String,
    job_id: String,
    engine_id: String,
    working_path: PathBuf,
    system_audio: bool,
    microphone: bool,
    started_at_unix_ms: u128,
    display_id: u32,
    area: Option<crate::capture_area::CaptureArea>,
    approved_output: Option<crate::capture_area::OutputSize>,
}

struct CaptureInner {
    session: Option<Arc<CaptureSession>>,
    status: CaptureStatus,
    last_result: Option<StopCaptureResult>,
    last_failure: Option<crate::capture_failures::CaptureFailureView>,
}

struct CaptureSession {
    stop: mpsc::Sender<()>,
    stop_sent: AtomicBool,
    result: Mutex<Option<Result<StopCaptureResult, AppMessage>>>,
    completed: Condvar,
}

impl CaptureSession {
    fn request_stop(&self) {
        if !self.stop_sent.swap(true, Ordering::SeqCst) {
            let _ = self.stop.send(());
        }
    }

    fn wait(&self) -> Result<StopCaptureResult, AppMessage> {
        let mut result = self
            .result
            .lock()
            .map_err(|_| "capture completion lock is poisoned")?;
        while result.is_none() {
            result = self
                .completed
                .wait(result)
                .map_err(|_| "capture completion lock is poisoned")?;
        }
        result.clone().unwrap()
    }
}

#[derive(Clone)]
pub struct CaptureManager {
    inner: Arc<Mutex<CaptureInner>>,
    changed: Arc<Condvar>,
    quit_pending: Arc<AtomicBool>,
}

impl Default for CaptureManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CaptureInner {
                session: None,
                status: CaptureStatus::idle(),
                last_result: None,
                last_failure: None,
            })),
            changed: Arc::new(Condvar::new()),
            quit_pending: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl CaptureManager {
    fn status_snapshot(&self) -> Result<CaptureStatus, AppMessage> {
        self.inner
            .lock()
            .map(|inner| inner.status.clone())
            .map_err(|_| AppMessage::from("capture state lock is poisoned"))
    }

    pub(crate) fn control_status(&self) -> Result<ControlCaptureStatus, &'static str> {
        // The control listener serves requests serially. A contended manager must
        // not hold up this listener or extend a client's existing I/O deadline.
        let inner = self.inner.try_lock().map_err(|e| match e {
            std::sync::TryLockError::WouldBlock => "capture_status_busy",
            std::sync::TryLockError::Poisoned(_) => "capture_status_unavailable",
        })?;
        let status = &inner.status;
        Ok(ControlCaptureStatus {
            state: status.state.clone(),
            started_at_unix_ms: status.started_at_unix_ms,
            // Helper error codes and messages can come from process output.
            error_code: (status.state == "failed").then_some("capture.failed"),
        })
    }
}

fn emit_status(app: &AppHandle, status: &CaptureStatus) {
    let _ = app.emit("capture-status", status);
}

fn set_failed(manager: &CaptureManager, app: &AppHandle, message: AppMessage) -> AppMessage {
    if let Ok(mut inner) = manager.inner.lock() {
        inner.status.state = "failed".into();
        inner.status.message = Some(message.clone());
        manager.changed.notify_all();
        emit_status(app, &inner.status);
    }
    message
}

#[tauri::command]
pub fn default_output_directory(app: AppHandle) -> Result<String, AppMessage> {
    let mut path = app.path().video_dir().map_err(|error| error.to_string())?;
    path.push("CaptureScribe");
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn capture_status(manager: State<'_, CaptureManager>) -> Result<CaptureStatus, AppMessage> {
    manager.status_snapshot()
}

#[tauri::command]
pub fn last_capture_result(
    manager: State<'_, CaptureManager>,
) -> Result<Option<StopCaptureResult>, AppMessage> {
    manager
        .inner
        .lock()
        .map(|inner| inner.last_result.clone())
        .map_err(|_| "capture state lock is poisoned".into())
}

#[tauri::command]
pub fn last_capture_failure(manager: State<'_, CaptureManager>) -> Result<Option<crate::capture_failures::CaptureFailureView>, AppMessage> {
    let failure = manager.inner.lock().map(|inner| inner.last_failure.clone())
        .map_err(|_| AppMessage::from("capture state lock is poisoned"))?;
    // Metadata checks can block on an unavailable volume; don't hold state lock.
    Ok(failure.map(|failure| crate::capture_failures::CaptureFailureView::new(failure.record, failure.persistence_error)))
}

// Called only by the shared terminal owner after recording-start confirmation,
// not by start-request validation or by each waiter (stop and quit can race).
fn record_save_failure(active: &ActiveCapture, message: AppMessage, history_root: Result<PathBuf, AppMessage>) -> crate::capture_failures::CaptureFailureView {
    use crate::capture_failures::{CaptureFailureView, CaptureSaveFailure};
    let record = CaptureSaveFailure::new(active.attempt_id.clone(), active.job_id.clone(),
        active.requested_stem.clone(), active.started_at_unix_ms, message,
        &active.reservation.final_path, &active.working_path);
    let persistence_error = history_root
        .map_err(|e| crate::history_index::storage_problem(e, "capture.failure_record_unavailable"))
        .and_then(|root| crate::capture_failures::persist(&root, &record))
        .err();
    CaptureFailureView::new(record, persistence_error)
}

#[tauri::command]
pub fn list_displays(app: AppHandle) -> Result<Vec<DisplayInfo>, AppMessage> {
    let result=selected_engine(&app).and_then(|engine|engine.list_displays());
    app.state::<crate::capture_target::TargetControl>().update_displays(result.clone().map_err(|e|e.to_string()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, ActiveCapture) {
        let root =
            std::env::temp_dir().join(format!("capturescribe-completion-{}", unix_ms().unwrap()));
        // Each test thread has a separate suffix; no existing location is reused.
        let root = root.with_extension(format!("{:?}", std::thread::current().id()));
        std::fs::create_dir(&root).unwrap();
        let destination = DirectoryGuard::prepare(root.to_str().unwrap(), "").unwrap();
        std::fs::write(root.join("Recording.mp4"), b"previous source").unwrap();
        let reservation = reserve_name(&destination.directory, "Recording").unwrap();
        let working_path = reservation.directory.join("source.mp4");
        std::fs::write(&working_path, b"new original").unwrap();
        let report = serde_json::json!({"schemaVersion":1,"status":"completed","stopReason":"max_duration",
            "options":{"outputURL":tauri::Url::from_file_path(&working_path).unwrap()},"selectedDisplay":{"id":1},
            "outputSizeBytes":12,"recordingDurationSeconds":2});
        std::fs::write(
            reservation.directory.join("report.json"),
            report.to_string(),
        )
        .unwrap();
        let active = ActiveCapture {
            attempt_id: format!("attempt-{}", crate::history_index::token()),
            destination,
            reservation,
            requested_stem: "Recording".into(),
            job_id: "job".into(),
            engine_id: "test".into(),
            working_path,
            system_audio: false,
            microphone: false,
            started_at_unix_ms: 1,
            display_id: 1,
            area: None,
            approved_output: None,
        };
        (root, active)
    }

    #[test]
    fn manual_and_quit_waiters_share_one_persisted_save_failure() {
        let (root,active)=fixture();let store=root.join("history");
        let original=std::fs::read(&active.working_path).unwrap();
        let message=AppMessage::known("capture.validation_failed","injected validation error").param("detail",42);
        // Both normal stop and request_exit wait on this same terminal owner.
        let notice=record_save_failure(&active,message.clone(),Ok(store.clone()));assert!(notice.persisted);
        let (stop,_)=mpsc::channel();let session=Arc::new(CaptureSession {stop,stop_sent:AtomicBool::new(false),result:Mutex::new(None),completed:Condvar::new()});
        let manual=session.clone();let quit=session.clone();
        let manual_waiter=std::thread::spawn(move||manual.wait());let quit_waiter=std::thread::spawn(move||quit.wait());
        *session.result.lock().unwrap()=Some(Err(message.clone()));session.completed.notify_all();
        assert_eq!(manual_waiter.join().unwrap().unwrap_err(),message);assert_eq!(quit_waiter.join().unwrap().unwrap_err(),message);
        let (records,warnings)=crate::capture_failures::list(&store);assert!(warnings.is_empty());assert_eq!(records.len(),1);assert_eq!(records[0].record.error,message);
        assert_eq!(std::fs::read(&active.working_path).unwrap(),original);assert!(!active.reservation.final_path.with_extension("capture.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failure_record_write_error_remains_separate_from_capture_error() {
        let (root,active)=fixture();let store=root.join("not-a-directory");std::fs::write(&store,b"keep").unwrap();
        let notice=record_save_failure(&active,AppMessage::known("capture.report_invalid","original capture error"),Ok(store.clone()));
        assert!(!notice.persisted);assert!(notice.persistence_error.is_some());assert_eq!(notice.record.error.code.as_deref(),Some("capture.report_invalid"));assert_eq!(std::fs::read(store).unwrap(),b"keep");std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_history_location_is_reported_as_an_unsaved_failure_record() {
        let (root,active)=fixture();let original=std::fs::read(&active.working_path).unwrap();
        let location=AppMessage::known("history.storage_unavailable","app data unavailable").param("detail",7);
        let notice=record_save_failure(&active,AppMessage::known("capture.validation_failed","original capture error"),Err(location));
        assert!(!notice.persisted);let error=notice.persistence_error.unwrap();
        assert_eq!(error.code.as_deref(),Some("capture.failure_record_unavailable"));assert_eq!(error.raw.as_deref(),Some("app data unavailable"));assert_eq!(error.params["detail"],7);
        assert_eq!(notice.record.error.code.as_deref(),Some("capture.validation_failed"));assert_eq!(std::fs::read(&active.working_path).unwrap(),original);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quit_does_not_attach_an_older_capture_failure_to_new_preparation(){
        let (root,active)=fixture();let error=AppMessage::known("capture.validation_failed","fixture error");let failure=record_save_failure(&active,error.clone(),Ok(root.join("history")));let manager=CaptureManager::default();
        manager.inner.lock().unwrap().last_failure=Some(failure.clone());assert!(failure_snapshot(&manager).is_none());
        let (stop,_)=mpsc::channel();let session=Arc::new(CaptureSession{stop,stop_sent:AtomicBool::new(false),result:Mutex::new(Some(Err(error))),completed:Condvar::new()});manager.inner.lock().unwrap().session=Some(session);assert_eq!(failure_snapshot(&manager).unwrap().record.attempt_id,failure.record.attempt_id);
        manager.inner.lock().unwrap().session=None;assert!(failure_snapshot(&manager).is_none());std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn spatial_mismatch_or_unapproved_scaling_never_publishes_source() {
        for scale in [1.0, 0.5] {
            let (root, active) = fixture();
            let path = active.reservation.directory.join("report.json");
            let mut report: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            report["captureGeometry"] =
                serde_json::json!({"outputPixels":{"width":640,"height":480},"scaleFactor":scale});
            std::fs::write(&path, report.to_string()).unwrap();
            assert!(finalize_capture(&active, || Ok(MediaValidation {
                duration_seconds: Some(2.0),
                video_stream_count: 1,
                audio_stream_count: 0,
                video_size: Some((638, 478))
            }))
            .is_err());
            assert!(active.working_path.is_file());
            assert!(!active.reservation.final_path.exists());
            assert_eq!(
                std::fs::read(root.join("Recording.mp4")).unwrap(),
                b"previous source"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn successful_terminal_publication_is_non_overwriting_and_not_repeatable() {
        let (root, active) = fixture();
        let result = finalize_capture(&active, || {
            Ok(MediaValidation {
                duration_seconds: Some(2.0),
                video_stream_count: 1,
                audio_stream_count: 0,
                video_size: None,
            })
        })
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("Recording.mp4")).unwrap(),
            b"previous source"
        );
        assert_eq!(
            std::fs::read(result.status.final_path.unwrap()).unwrap(),
            b"new original"
        );
        assert_eq!(
            crate::history::parse_item(Path::new(&result.manifest_path))
                .unwrap()
                .job_id,
            "job"
        );
        assert!(finalize_capture(&active, || panic!(
            "must reject missing working report before probing"
        ))
        .is_err());
        assert_eq!(
            std::fs::read_dir(root.join(crate::save_destination::MANAGEMENT_DIRECTORY))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "json")
                    && e.file_name().to_string_lossy().ends_with(".capture.json"))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_invalid_or_mismatched_report_keeps_working_and_previous_files() {
        for field in [
            "missing", "invalid", "status", "path", "display", "size", "duration", "reason",
        ] {
            let (root, active) = fixture();
            let path = active.reservation.directory.join("report.json");
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            match field {
                "missing" => {
                    std::fs::remove_file(&path).unwrap();
                }
                "invalid" => {
                    std::fs::write(&path, b"invalid report").unwrap();
                }
                _ => {
                    match field {
                        "status" => value["status"] = "failed".into(),
                        "path" => value["options"]["outputURL"] = "file:///missing.mp4".into(),
                        "display" => value["selectedDisplay"]["id"] = 99.into(),
                        "size" => value["outputSizeBytes"] = 999.into(),
                        "duration" => value["recordingDurationSeconds"] = 0.into(),
                        _ => value["stopReason"] = "unknown_control".into(),
                    }
                    std::fs::write(&path, value.to_string()).unwrap();
                }
            }
            assert_eq!(
                finalize_capture(&active, || panic!("invalid report must precede probe"))
                    .unwrap_err()
                    .code
                    .as_deref(),
                Some("capture.report_invalid")
            );
            assert_eq!(
                std::fs::read(&active.working_path).unwrap(),
                b"new original"
            );
            assert_eq!(
                std::fs::read(root.join("Recording.mp4")).unwrap(),
                b"previous source"
            );
            assert!(!active.reservation.final_path.exists());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn video_validation_failure_keeps_report_and_original_working_media() {
        let (root, active) = fixture();
        assert!(finalize_capture(&active, || Err(AppMessage::known(
            "capture.validation_failed",
            "invalid media fixture"
        )))
        .is_err());
        assert!(active.reservation.directory.join("report.json").is_file());
        assert_eq!(
            std::fs::read(&active.working_path).unwrap(),
            b"new original"
        );
        assert!(!active.reservation.final_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_stop_callers_receive_the_same_single_completion() {
        let (tx, rx) = mpsc::channel();
        let session = Arc::new(CaptureSession {
            stop: tx,
            stop_sent: AtomicBool::new(false),
            result: Mutex::new(None),
            completed: Condvar::new(),
        });
        let (root, active) = fixture();
        std::thread::scope(|threads| {
            let mut waiters = Vec::new();
            for _ in 0..8 {
                let session = session.clone();
                waiters.push(threads.spawn(move || {
                    session.request_stop();
                    session.wait().unwrap().manifest_path
                }));
            }
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(rx.try_recv().is_err());
            let value = finalize_capture(&active, || {
                Ok(MediaValidation {
                    duration_seconds: Some(2.0),
                    video_stream_count: 1,
                    audio_stream_count: 0,
                    video_size: None,
                })
            })
            .unwrap();
            *session.result.lock().unwrap() = Some(Ok(value.clone()));
            session.completed.notify_all();
            for waiter in waiters {
                assert_eq!(waiter.join().unwrap(), value.manifest_path);
            }
            assert_eq!(session.wait().unwrap().manifest_path, value.manifest_path);
        });
        assert_eq!(
            crate::history::parse_item(Path::new(&session.wait().unwrap().manifest_path))
                .unwrap()
                .job_id,
            "job"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn filename_strips_one_or_more_mp4_suffixes_without_changing_content() {
        assert_eq!(validated_stem("  Lecture 01.mp4  ").unwrap(), "Lecture 01");
    }

    #[test]
    fn filename_rejects_paths_and_parent_components() {
        assert_eq!(
            validated_stem("../recording").unwrap_err().code.as_deref(),
            Some("capture.filename_invalid")
        );
        assert_eq!(
            validated_stem(" ").unwrap_err().code.as_deref(),
            Some("capture.filename_required")
        );
        assert!(validated_stem("folder/recording")
            .unwrap_err()
            .raw
            .is_some());
    }
}

fn unix_ms() -> Result<u128, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn start_capture(
    app: AppHandle,
    manager: State<'_, CaptureManager>,
    mut request: StartCaptureRequest,
) -> Result<CaptureStatus, AppMessage> {
    let targets=app.state::<crate::capture_target::TargetControl>();
    let lease = match app.state::<crate::operation_gate::OperationGate>().acquire(crate::operation_gate::Owner::Capture) {
        Ok(lease)=>lease,
        Err(error)=>{targets.discard_failed_start(request.temporary_target.as_ref());return Err(error);}
    };
    if let Some(target)=targets.consume_for_capture(request.temporary_target.as_ref(),request.display_id,request.area.as_ref())
        .map_err(|e|AppMessage::known("capture.region_invalid",e))? {
        request.display_id=target.display_id;
        request.area=Some(target.area);
    }
    let manager = manager.inner().clone();
    // An accepted preparation must not reuse an older completed session when
    // quit races validation before the new helper reaches `starting`.
    {let mut inner=manager.inner.lock().map_err(|_|AppMessage::from("capture state lock is poisoned"))?;inner.session=None;inner.status=CaptureStatus::idle();}
    tauri::async_runtime::spawn_blocking(move || start_capture_inner(app, &manager, request, lease))
        .await
        .map_err(|e| AppMessage::from(e.to_string()))?
}

fn start_capture_inner(
    app: AppHandle,
    manager: &CaptureManager,
    request: StartCaptureRequest,
    lease: crate::operation_gate::Lease,
) -> Result<CaptureStatus, AppMessage> {
    let _operation = crate::capture_area::AREA_OPERATION
        .try_lock()
        .map_err(|_| {
            AppMessage::known(
                "capture.selection_failed",
                "A region selector or capture preparation is active",
            )
        })?;
    {
        let inner = manager
            .inner
            .lock()
            .map_err(|_| "capture state lock is poisoned")?;
        if matches!(
            inner.status.state.as_str(),
            "starting" | "recording" | "finalizing"
        ) {
            return Err(AppMessage::known(
                "capture.already_recording",
                "すでに録画中です。",
            ));
        }
    }

    let engine = selected_engine(&app)?;
    if request
        .approved_output
        .is_some_and(|s| !crate::capture_area::output_verified(s))
    {
        return Err(AppMessage::known(
            "capture.output_configuration_failed",
            "This output size has not been verified by ScreenCaptureKit in this app run",
        ));
    }
    if crate::capture_area::SELECTING.load(Ordering::SeqCst) {
        return Err(AppMessage::known(
            "capture.selection_failed",
            "Close the region selector before recording",
        ));
    }
    if let Some(area) = &request.area {
        let displays = engine.list_displays()?;
        let display = displays
            .iter()
            .find(|d| d.id == request.display_id)
            .ok_or_else(|| {
                AppMessage::known("capture.region_invalid", "Display is no longer available")
            })?;
        area.validate(display.width_points as f64, display.height_points as f64)?;
        if area.display_ref != display.display_ref || area.snapshot_token != display.snapshot_token
        {
            return Err(AppMessage::known(
                "capture.region_invalid",
                "Display configuration changed; select again",
            ));
        }
    }
    let engine_id = engine.id().to_string();
    let stem = validated_stem(&request.file_name)?;
    let destination = DirectoryGuard::prepare(&request.output_directory, &request.subfolder)?;
    let output_directory = &destination.directory;
    let reservation = reserve_name(output_directory, &stem)?;
    let final_path = reservation.final_path.clone();
    let started_at_unix_ms = unix_ms()?;
    let working_path = reservation.directory.join("source.mp4");
    let report_path = reservation.directory.join("report.json");

    let starting = CaptureStatus {
        state: "starting".into(),
        started_at_unix_ms: Some(started_at_unix_ms),
        working_path: Some(working_path.to_string_lossy().into_owned()),
        final_path: Some(final_path.to_string_lossy().into_owned()),
        message: Some(AppMessage::progress("capture.preparing")),
    };
    {
        let mut inner = manager
            .inner
            .lock()
            .map_err(|_| "capture state lock is poisoned")?;
        if matches!(
            inner.status.state.as_str(),
            "starting" | "recording" | "finalizing"
        ) {
            return Err(AppMessage::known(
                "capture.already_recording",
                "Another capture is starting or finalizing",
            ));
        }
        inner.status = starting.clone();
    }
    emit_status(&app, &starting);

    destination
        .verify()
        .map_err(|message| set_failed(&manager, &app, message))?;
    let mut process = engine
        .start(
            &app,
            CaptureLaunch {
                output_path: &working_path,
                report_path: &report_path,
                display_id: request.display_id,
                system_audio: request.system_audio,
                microphone: request.microphone,
                max_duration_seconds: request.max_duration_seconds,
                area: request.area.as_ref(),
                approved_output: request.approved_output,
            },
        )
        .map_err(|message| set_failed(&manager, &app, message))?;

    let recording_started_at = match process
        .ready
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| {
            Err(AppMessage::known(
                "capture.start_timeout",
                "録画開始を15秒以内に確認できませんでした。権限または画面状態を確認してください。",
            ))
        }) {
        Ok(value) => value,
        Err(message) => {
            let child = &mut process.child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(set_failed(&manager, &app, message));
        }
    };

    let active = ActiveCapture {
        attempt_id: format!("attempt-{}", crate::history_index::token()),
        destination,
        reservation,
        requested_stem: stem,
        job_id: started_at_unix_ms.to_string(),
        engine_id,
        working_path: working_path.clone(),
        system_audio: request.system_audio,
        microphone: request.microphone,
        started_at_unix_ms: recording_started_at,
        display_id: request.display_id,
        area: request.area,
        approved_output: request.approved_output,
    };
    let recording = CaptureStatus {
        state: "recording".into(),
        started_at_unix_ms: Some(recording_started_at),
        working_path: Some(working_path.to_string_lossy().into_owned()),
        final_path: Some(final_path.to_string_lossy().into_owned()),
        message: Some(AppMessage::progress("capture.recording")),
    };
    let (stop, receiver) = mpsc::channel();
    let session = Arc::new(CaptureSession {
        stop,
        stop_sent: AtomicBool::new(false),
        result: Mutex::new(None),
        completed: Condvar::new(),
    });
    {
        let mut inner = manager
            .inner
            .lock()
            .map_err(|_| "capture state lock is poisoned")?;
        inner.session = Some(session.clone());
        inner.status = recording.clone();
        manager.changed.notify_all();
    }
    emit_status(&app, &recording);
    let manager = manager.clone();
    std::thread::spawn(move || {
        let _lease = lease;
        let exit = crate::capture_process::wait_for_exit(&mut process.child, receiver, || {
            mark_finalizing(&manager, &app)
        });
        mark_finalizing(&manager, &app);
        let _ = process.events.join();
        let result = exit.and_then(|exit| {
            if !exit.success() {
                return Err(process
                    .failure
                    .lock()
                    .ok()
                    .and_then(|v| v.clone())
                    .unwrap_or_else(|| {
                        AppMessage::known("capture.helper_failed", exit.to_string())
                    }));
            }
            finalize_capture(&active, || {
                validate_media(
                    &active.working_path,
                    active.system_audio || active.microphone,
                )
            })
            .map(|mut result| {
                if let Some(g) = &result.capture_geometry {
                    if let Some((width, height)) = g["outputPixels"]["width"]
                        .as_u64()
                        .zip(g["outputPixels"]["height"].as_u64())
                    {
                        crate::capture_area::remember_output(crate::capture_area::OutputSize {
                            width,
                            height,
                        });
                    }
                }
                result.history_warnings = crate::history_index::register(
                    &app,
                    Path::new(&result.manifest_path),
                    active.started_at_unix_ms,
                );
                result
            })
        });
        match &result {
            Ok(value) => {
                if let Ok(mut inner) = manager.inner.lock() {
                    inner.status = value.status.clone();
                    inner.last_result = Some(value.clone());
                    inner.last_failure = None;
                    emit_status(&app, &inner.status);
                    let _ = app.emit("capture-completed", value);
                }
            }
            Err(message) => {
                let failure = record_save_failure(&active, message.clone(), crate::history_index::root(&app));
                if let Ok(mut inner) = manager.inner.lock() {
                    inner.last_failure = Some(failure.clone());
                }
                let _ = app.emit("capture-save-failed", &failure);
                set_failed(&manager, &app, message.clone());
            }
        }
        if let Ok(mut completed) = session.result.lock() {
            *completed = Some(result);
            session.completed.notify_all();
        }
        manager.changed.notify_all();
    });
    Ok(recording)
}

#[derive(Default)]
struct MediaValidation {
    duration_seconds: Option<f64>,
    video_stream_count: usize,
    audio_stream_count: usize,
    video_size: Option<(u64, u64)>,
}

fn validate_media(path: &Path, audio_required: bool) -> Result<MediaValidation, AppMessage> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type,width,height",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|error| AppMessage::known("capture.probe_unavailable", error.to_string()))?;
    if !output.status.success() {
        return Err(AppMessage::known(
            "capture.validation_failed",
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    let streams = value
        .get("streams")
        .and_then(Value::as_array)
        .ok_or("ffprobe stream情報がありません")?;
    let video_stream_count = streams
        .iter()
        .filter(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("video"))
        .count();
    let audio_stream_count = streams
        .iter()
        .filter(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("audio"))
        .count();
    let duration_seconds = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(Value::as_str)
        .and_then(|duration| duration.parse::<f64>().ok());
    if video_stream_count == 0 {
        return Err(AppMessage::known(
            "capture.video_stream_missing",
            "完成ファイルに映像streamがありません。元録画は保持します。",
        ));
    }
    if audio_required && audio_stream_count == 0 {
        return Err(AppMessage::known(
            "capture.audio_stream_missing",
            "有効にした音声streamが完成ファイルにありません。元録画は保持します。",
        ));
    }
    if !duration_seconds.is_some_and(|value| value.is_finite() && value >= 0.5) {
        return Err(AppMessage::known(
            "capture.duration_invalid",
            "完成ファイルのdurationが短すぎます。元録画は保持します。",
        ));
    }
    Ok(MediaValidation {
        duration_seconds,
        video_stream_count,
        audio_stream_count,
        video_size: streams
            .iter()
            .find(|s| s["codec_type"] == "video")
            .and_then(|s| Some((s["width"].as_u64()?, s["height"].as_u64()?))),
    })
}

#[tauri::command]
pub async fn stop_capture(
    manager: State<'_, CaptureManager>,
) -> Result<StopCaptureResult, AppMessage> {
    let session = {
        let inner = manager
            .inner
            .lock()
            .map_err(|_| "capture state lock is poisoned")?;
        if inner.status.state == "failed" {
            return Err(inner
                .status
                .message
                .clone()
                .unwrap_or_else(|| "capture failed".into()));
        }
        if inner.status.state == "starting" {
            return Err(AppMessage::known(
                "capture.not_recording",
                "Capture is still preparing",
            ));
        }
        inner
            .session
            .clone()
            .ok_or_else(|| AppMessage::known("capture.not_recording", "録画中ではありません。"))?
    };
    session.request_stop();
    tauri::async_runtime::spawn_blocking(move || session.wait())
        .await
        .map_err(|e| AppMessage::from(e.to_string()))?
}

fn mark_finalizing(manager: &CaptureManager, app: &AppHandle) {
    if let Ok(mut inner) = manager.inner.lock() {
        if inner.status.state == "recording" {
            inner.status.state = "finalizing".into();
            inner.status.message = Some(AppMessage::progress("capture.finalizing"));
            emit_status(app, &inner.status);
        }
    }
}

fn validate_report(active: &ActiveCapture) -> Result<(), AppMessage> {
    let check = || -> Result<(), String> {
        let report: Value = serde_json::from_slice(
            &std::fs::read(active.reservation.directory.join("report.json"))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if report["schemaVersion"] != 1 || report["status"] != "completed" {
            return Err("report schema/status is not completed".into());
        }
        if !matches!(
            report["stopReason"].as_str(),
            Some("user_stop" | "max_duration" | "parent_stdin_closed")
        ) {
            return Err("unexpected report stop reason".into());
        }
        let output = report["options"]["outputURL"]
            .as_str()
            .ok_or("report output URL missing")?;
        let path = tauri::Url::parse(output)
            .map_err(|e| e.to_string())?
            .to_file_path()
            .map_err(|_| "report output URL is not a file URL")?;
        if path.canonicalize().map_err(|e| e.to_string())?
            != active
                .working_path
                .canonicalize()
                .map_err(|e| e.to_string())?
        {
            return Err("report refers to another recording".into());
        }
        if report["selectedDisplay"]["id"].as_u64() != Some(active.display_id as u64) {
            return Err("report display mismatch".into());
        }
        if let Some(area) = &active.area {
            let reported: crate::capture_area::CaptureArea =
                serde_json::from_value(report["captureGeometry"]["area"].clone())
                    .map_err(|e| e.to_string())?;
            if &reported != area {
                return Err("capture area in report differs from the request".into());
            }
        }
        if let Some(g) = report.get("captureGeometry") {
            let scale = g["scaleFactor"].as_f64().ok_or("capture scale missing")?;
            if !scale.is_finite()
                || scale <= 0.0
                || scale > 1.0
                || (active.approved_output.is_none() && scale != 1.0)
            {
                return Err("unapproved capture scaling".into());
            }
            if let Some(size) = active.approved_output {
                if g["outputPixels"]["width"].as_u64() != Some(size.width)
                    || g["outputPixels"]["height"].as_u64() != Some(size.height)
                {
                    return Err("output differs from approved resolution".into());
                }
            }
        }
        if report["outputSizeBytes"].as_u64()
            != Some(
                std::fs::metadata(&active.working_path)
                    .map_err(|e| e.to_string())?
                    .len(),
            )
        {
            return Err("report file size mismatch".into());
        }
        if !report["recordingDurationSeconds"]
            .as_f64()
            .is_some_and(|d| d.is_finite() && d > 0.0)
        {
            return Err("report duration invalid".into());
        }
        Ok(())
    };
    check().map_err(|e| {
        AppMessage::known("capture.report_invalid", e).param(
            "path",
            active.reservation.directory.to_string_lossy().into_owned(),
        )
    })
}

fn finalize_capture(
    active: &ActiveCapture,
    probe: impl FnOnce() -> Result<MediaValidation, AppMessage>,
) -> Result<StopCaptureResult, AppMessage> {
    active.destination.verify()?;
    validate_report(active)?;
    let validation = probe()?;
    let report: Value = serde_json::from_slice(
        &std::fs::read(active.reservation.directory.join("report.json"))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let geometry = report.get("captureGeometry").cloned();
    if let Some(g) = &geometry {
        let size = g["outputPixels"]["width"]
            .as_u64()
            .zip(g["outputPixels"]["height"].as_u64());
        if size.is_none() || size != validation.video_size {
            return Err(AppMessage::known(
                "capture.geometry_mismatch",
                "Output pixels differ from the capture report; working files retained",
            ));
        }
    }
    let (final_path, manifest_path) = finalize_recording(
        &active.destination,
        &active.reservation,
        &active.requested_stem,
        &active.job_id,
        &active.engine_id,
        validation.duration_seconds.unwrap_or(0.0),
    )?;

    let complete = CaptureStatus {
        state: "idle".into(),
        started_at_unix_ms: None,
        working_path: None,
        final_path: Some(final_path.to_string_lossy().into_owned()),
        message: Some(AppMessage::progress("capture.saved")),
    };
    Ok(StopCaptureResult {
        recorded_at_unix_ms: active.started_at_unix_ms,
        history_warnings: Vec::new(),
        status: complete,
        validated: true,
        duration_seconds: validation.duration_seconds,
        video_stream_count: validation.video_stream_count,
        audio_stream_count: validation.audio_stream_count,
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        capture_geometry: geometry,
    })
}

impl CaptureManager {
    pub fn busy(&self) -> bool {
        self.inner
            .lock()
            .map(|v| {
                matches!(
                    v.status.state.as_str(),
                    "starting" | "recording" | "finalizing"
                )
            })
            .unwrap_or(true)
    }

    /// Normal close/quit requests wait off the UI thread. Failure leaves the
    /// app open with diagnostics; force-kill/power-loss recovery is not promised.
    pub fn request_exit(&self, app: AppHandle, complete: impl FnOnce(Result<(),AppMessage>)+Send+'static) {
        if self.quit_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let manager = self.clone();
        std::thread::spawn(move || {
            // A gate reservation can precede `starting` during validation.
            // Do not accidentally wait on an older completed session.
            let deadline=std::time::Instant::now()+Duration::from_secs(45);
            let result=loop {
                let inner=manager.inner.lock().unwrap();
                let state=inner.status.state.clone();
                let session=if matches!(state.as_str(),"recording"|"finalizing"){inner.session.clone()}else{None};
                drop(inner);
                if let Some(session)=session {
                    session.request_stop();
                    let mut value=session.result.lock().unwrap();
                    while value.is_none()&&std::time::Instant::now()<deadline {value=session.completed.wait_timeout(value,Duration::from_millis(50)).unwrap().0;}
                    break value.clone().unwrap_or_else(||Err(AppMessage::known("capture.completion_unknown","Capture finalization not confirmed within 45 seconds"))).map(|_|());
                }
                if app.state::<crate::operation_gate::OperationGate>().owner()!=Some(crate::operation_gate::Owner::Capture) {
                    break if state=="failed"{Err(manager.inner.lock().unwrap().status.message.clone().unwrap_or_else(||AppMessage::known("capture.completion_unknown","Capture failed")))}else{Ok(())};
                }
                if std::time::Instant::now()>=deadline{break Err(AppMessage::known("capture.completion_unknown","Capture preparation or finalization not confirmed within 45 seconds"));}
                std::thread::sleep(Duration::from_millis(30));
            };
            manager.quit_pending.store(false, Ordering::SeqCst);
            complete(result);
        });
    }
}

pub(crate) fn failure_snapshot(manager:&CaptureManager)->Option<crate::capture_failures::CaptureFailureView>{
    let inner=manager.inner.lock().unwrap();
    // A failure before recording-start confirmation has no save-failure record;
    // never attach the previous recording's failure to that quit request.
    let error=inner.session.as_ref()?.result.lock().ok()?.clone()?.err()?;
    inner.last_failure.clone().filter(|f|f.record.error==error)
}

#[cfg(test)]
mod control_status_tests {
    use super::*;
    use crate::{capture_target::TargetControl, control, selection_session::SelectionManager};
    use std::{fs, thread};

    struct Fixture {
        root: PathBuf,
        server: control::ControlServer,
        endpoint: control::Endpoint,
        manager: CaptureManager,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cs-capture-status-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            let manager = CaptureManager::default();
            let server = control::ControlServer::start(
                &root.join("private"),
                &std::env::current_exe().unwrap(),
                SelectionManager::default(),
                TargetControl::default(),
                manager.clone(),
            )
            .unwrap();
            let endpoint = control::read_endpoint(&root.join("private/endpoint.json")).unwrap();
            Self {
                root,
                server,
                endpoint,
                manager,
            }
        }

        fn call(&self) -> control::Response {
            control::call(
                &self.root.join("private/endpoint.json"),
                &self.endpoint.app_executable,
                &self.endpoint.executable_sha256,
                Some(&self.endpoint.instance_id),
                control::Operation::CaptureStatus {},
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.shutdown();
            fs::remove_dir(&self.root).unwrap();
        }
    }

    #[test]
    fn authenticated_status_projects_the_gui_snapshot_without_mutation_or_private_data() {
        let fixture = Fixture::new();
        let (stop, receiver) = mpsc::channel();
        let session = Arc::new(CaptureSession {
            stop,
            stop_sent: AtomicBool::new(false),
            result: Mutex::new(None),
            completed: Condvar::new(),
        });
        fixture.manager.inner.lock().unwrap().session = Some(session.clone());
        let descriptor = fixture.root.join("private/endpoint.json");
        let descriptor_bytes = fs::read(&descriptor).unwrap();
        for (state, started_at_unix_ms) in [
            ("idle", None),
            ("starting", None),
            ("recording", Some(1763460000123)),
            ("finalizing", Some(1763460000123)),
            ("failed", Some(1763460000123)),
            ("failed", None),
        ] {
            let status = CaptureStatus {
                state: state.into(),
                started_at_unix_ms,
                working_path: Some("/private/fixture/working.mp4".into()),
                final_path: Some("/private/fixture/final.mp4".into()),
                message: Some(
                    AppMessage::known("/private/fixture/code", "private diagnostic")
                        .param("path", "/private/fixture/message"),
                ),
            };
            fixture.manager.inner.lock().unwrap().status = status;
            let gui = fixture.manager.status_snapshot().unwrap();
            let mut expected = serde_json::json!({
                "state": gui.state,
                "startedAtUnixMs": gui.started_at_unix_ms,
            });
            if state == "failed" {
                expected["errorCode"] = serde_json::json!("capture.failed");
            }
            for _ in 0..2 {
                let response = fixture.call();
                assert!(response.ok);
                assert_eq!(response.result, Some(expected.clone()));
                assert_eq!(response.error, None);
            }
            assert_eq!(
                serde_json::to_value(fixture.manager.status_snapshot().unwrap()).unwrap(),
                serde_json::to_value(gui).unwrap(),
            );
        }
        let inner = fixture.manager.inner.lock().unwrap();
        assert!(Arc::ptr_eq(inner.session.as_ref().unwrap(), &session));
        assert!(inner.last_result.is_none() && inner.last_failure.is_none());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(!session.stop_sent.load(Ordering::SeqCst));
        assert_eq!(fs::read(&descriptor).unwrap(), descriptor_bytes);
        assert_eq!(
            fs::read_dir(descriptor.parent().unwrap()).unwrap().count(),
            1
        );
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 1);
    }

    #[test]
    fn contended_and_poisoned_status_are_explicit_errors_and_do_not_block_the_listener() {
        let fixture = Fixture::new();
        {
            let _transition = fixture.manager.inner.lock().unwrap();
            let response = fixture.call();
            assert!(!response.ok);
            assert!(response.result.is_none());
            assert_eq!(response.error.as_deref(), Some("capture_status_busy"));
        }
        assert_eq!(fixture.call().result.unwrap()["state"], "idle");
        let manager = fixture.manager.clone();
        assert!(thread::spawn(move || {
            let _guard = manager.inner.lock().unwrap();
            panic!("poison only the isolated test manager");
        })
        .join()
        .is_err());
        let response = fixture.call();
        assert!(!response.ok);
        assert!(response.result.is_none());
        assert_eq!(
            response.error.as_deref(),
            Some("capture_status_unavailable")
        );
    }

    #[test]
    fn concurrent_status_reads_report_coherent_transitions() {
        use std::sync::Barrier;
        let fixture = Fixture::new();
        let go = Barrier::new(5);
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    go.wait();
                    for _ in 0..8 {
                        let response = fixture.call();
                        if let Some(snapshot) = response.result {
                            assert!(response.ok);
                            match snapshot["state"].as_str().unwrap() {
                                "idle" | "starting" => {
                                    assert!(snapshot["startedAtUnixMs"].is_null())
                                }
                                "recording" | "finalizing" => {
                                    assert_eq!(snapshot["startedAtUnixMs"], 42)
                                }
                                state => panic!("unexpected state: {state}"),
                            }
                        } else {
                            assert!(!response.ok);
                            assert_eq!(response.error.as_deref(), Some("capture_status_busy"));
                        }
                    }
                });
            }
            go.wait();
            for _ in 0..12 {
                for (state, timestamp) in [
                    ("starting", None),
                    ("recording", Some(42)),
                    ("finalizing", Some(42)),
                ] {
                    {
                        let mut inner = fixture.manager.inner.lock().unwrap();
                        inner.status.state = state.into();
                        inner.status.started_at_unix_ms = timestamp;
                    }
                    thread::sleep(Duration::from_millis(4));
                }
            }
        });
        let gui = fixture.manager.status_snapshot().unwrap();
        assert_eq!(gui.state, "finalizing");
        assert_eq!(gui.started_at_unix_ms, Some(42));
    }
}
