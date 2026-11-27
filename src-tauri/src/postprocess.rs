use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{AppHandle, Emitter, Manager};

use crate::message::AppMessage;
use crate::processing_runs::{Execution, ManifestIdentity, RunView};
use crate::recording_manifest::{ProcessingSettings, TranscriptionInfo};

static RECORDING_UNAVAILABLE: AtomicBool = AtomicBool::new(false);
pub(crate) type Progress = Arc<dyn Fn(AppMessage) + Send + Sync>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PostProcessRequest {
    pub(crate) manifest_path: String,
    trim_trailing_silence: bool,
    split_long_video: bool,
    split_threshold_minutes: u64,
    split_segment_minutes: u64,
    transcribe: bool,
    #[serde(default)]
    transcription_locale: Option<String>,
}

impl PostProcessRequest {
    pub(crate) fn settings(&self) -> ProcessingSettings {
        ProcessingSettings {
            trim_trailing_silence: self.trim_trailing_silence,
            split_long_video: self.split_long_video,
            split_threshold_minutes: self.split_threshold_minutes,
            split_segment_minutes: self.split_segment_minutes,
            transcribe: self.transcribe,
            transcription_locale: if self.transcribe {
                self.transcription_locale.clone()
            } else {
                None
            },
        }
    }
    fn transcription_arguments(&self) -> Result<Vec<String>, AppMessage> {
        if !self.transcribe {
            return Ok(Vec::new());
        }
        match self.transcription_locale.as_deref() {
            Some(locale @ ("ja-JP" | "en-US")) => {
                Ok(vec!["--transcription-locale".into(), locale.into()])
            }
            _ => Err(AppMessage::known(
                "transcription.locale_invalid",
                "A supported recognition locale is required when transcription is enabled",
            )),
        }
    }
    fn split_arguments(&self) -> Result<[String; 4], AppMessage> {
        // u32 minutes keep milliseconds exact in JS and microseconds in i64.
        let valid = |value| (1..=u32::MAX as u64).contains(&value);
        if !valid(self.split_threshold_minutes) || !valid(self.split_segment_minutes) {
            return Err(AppMessage::known(
                "processing.split_invalid",
                "Split minutes must be integers in 1..=u32::MAX",
            ));
        }
        Ok([
            "--split-threshold-minutes".into(),
            self.split_threshold_minutes.to_string(),
            "--split-segment-minutes".into(),
            self.split_segment_minutes.to_string(),
        ])
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerArtifact {
    role: String,
    path: String,
    state: String,
    source_timeline_offset_seconds: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostProcessResult {
    #[serde(skip_deserializing)]
    pub(crate) run: Option<RunView>,
    #[serde(skip_deserializing)]
    error: Option<AppMessage>,
    #[serde(default)]
    pub(crate) history_warnings: Vec<AppMessage>,
    pub(crate) status: String,
    pub(crate) manifest_path: String,
    source_master: String,
    source_duration_seconds: f64,
    video_outputs: Vec<String>,
    transcripts: Vec<Value>,
    #[serde(default)]
    transcription: Option<TranscriptionInfo>,
    trim: Value,
    split: Value,
    artifacts: Vec<WorkerArtifact>,
    warnings: Vec<AppMessage>,
    elapsed_seconds: f64,
    #[serde(default)]
    worker_log: String,
}

fn resource_or_development_path(
    app: &AppHandle,
    bundled: &str,
    development: &str,
) -> Result<PathBuf, AppMessage> {
    if let Ok(resource_dir) = app.path().resource_dir() {
        let path = resource_dir.join(bundled);
        if path.is_file() {
            return Ok(path);
        }
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(development);
    if path.is_file() {
        return Ok(path);
    }
    Err(AppMessage::known(
        "processing.resource_missing",
        format!("required resource not found: {bundled}"),
    )
    .param("path", bundled))
}

fn executable(env_name: &str, standard_paths: &[&str], fallback: &str) -> String {
    if let Ok(value) = std::env::var(env_name) {
        if !value.trim().is_empty() {
            return value;
        }
    }
    standard_paths
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback.to_string())
}

pub(crate) fn run_worker(
    progress: Progress,
    worker: PathBuf,
    transcriber: PathBuf,
    request: &PostProcessRequest,
    execution: &Execution,
    stop: &AtomicBool,
) -> Result<PostProcessResult, AppMessage> {
    let split_arguments = request.split_arguments()?;
    let transcription_arguments = request.transcription_arguments()?;
    let manifest_path = PathBuf::from(request.manifest_path.trim());
    if !manifest_path.is_file() {
        return Err(AppMessage::known(
            "processing.input_failed",
            format!("manifest not found: {}", manifest_path.display()),
        )
        .param("path", manifest_path.to_string_lossy().into_owned()));
    }

    let python = executable(
        "CAPTURE_SCRIBE_PYTHON",
        &[
            "/usr/bin/python3",
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
        ],
        "python3",
    );
    let ffmpeg = executable(
        "CAPTURE_SCRIBE_FFMPEG",
        &["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"],
        "ffmpeg",
    );
    let ffprobe = executable(
        "CAPTURE_SCRIBE_FFPROBE",
        &["/opt/homebrew/bin/ffprobe", "/usr/local/bin/ffprobe"],
        "ffprobe",
    );

    let mut command = Command::new(python);
    command
        .arg("-B")
        .arg(worker)
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--ffmpeg")
        .arg(ffmpeg)
        .arg("--ffprobe")
        .arg(ffprobe)
        .arg("--transcriber")
        .arg(transcriber)
        .arg("--run-id")
        .arg(execution.run().run_id.as_deref().unwrap())
        .arg("--receipt-directory")
        .arg(&execution.directory)
        .arg("--expected-job-id")
        .arg(&execution.identity().job_id)
        .arg("--expected-source-master")
        .arg(&execution.identity().source_master_path)
        .arg("--expected-source-identity")
        .arg(&execution.identity().source_physical_identity)
        .args(split_arguments)
        .args(transcription_arguments);
    if request.trim_trailing_silence {
        command.arg("--trim");
    }
    if request.split_long_video {
        command.arg("--split");
    }
    if request.transcribe {
        command.arg("--transcribe");
    }

    collect_owned_worker(command, progress, stop)
}

#[cfg(test)]
fn collect_worker(
    command: Command,
    progress: Progress,
) -> Result<PostProcessResult, AppMessage> {
    collect_owned_worker(command, progress, &AtomicBool::new(false))
}

fn collect_owned_worker(command: Command, progress: Progress, stop: &AtomicBool) -> Result<PostProcessResult, AppMessage> {
    progress(AppMessage::progress("processing.worker_starting"));
    let mut failure=None;
    let output=crate::owned_worker::collect(command,stop,|line|{
        if let Some((event,payload))=worker_message(line){if event=="error"{failure=Some(payload);}else{progress(payload);}}
    })?;
    let worker_log=String::from_utf8_lossy(&output.stderr).into_owned();
    if output.cancelled {return Err(AppMessage::known("processing.cancelled",worker_log));}
    if !output.status.success() {
        let mut message = failure.unwrap_or_else(|| {
            AppMessage::known(
                "processing.worker_failed",
                if worker_log.is_empty() {
                    output.status.to_string()
                } else {
                    worker_log.clone()
                },
            )
        });
        if message.raw.as_deref().unwrap_or("").is_empty() && !worker_log.is_empty() {
            message.raw = Some(worker_log);
        }
        return Err(message);
    }

    let mut result: PostProcessResult =
        serde_json::from_slice(&output.stdout).map_err(|error| {
            AppMessage::known(
                "processing.result_invalid",
                format!("Invalid worker JSON: {error}\n{worker_log}"),
            )
        })?;
    result.worker_log = worker_log;
    Ok(result)
}

pub(crate) fn prepare(root: &Path, request: &PostProcessRequest) -> Result<Execution, AppMessage> {
    if RECORDING_UNAVAILABLE.load(Ordering::SeqCst) {return Err(crate::processing_runs::problem("Previous terminal record unavailable; new processing disabled"));}
    request.split_arguments()?;
    request.transcription_arguments()?;
    let identity=ManifestIdentity::read(Path::new(request.manifest_path.trim()))?;
    crate::history_index::require_unambiguous_index_target(
        root, Path::new(&identity.manifest_path), Path::new(&identity.source_master_path),
    )?;
    if !Path::new(&identity.source_master_path).is_file(){return Err(AppMessage::known("processing.source_missing","Source Master unavailable").param("path",identity.source_master_path));}
    let record_id=crate::history_index::record_id_for(root,&identity.manifest_path,&identity.job_id);
    Execution::accept(root,identity,record_id,request.settings())
}

#[cfg(test)]
fn execute_with(
    root: &Path,
    request: &PostProcessRequest,
    worker: impl FnOnce(&Execution) -> Result<PostProcessResult, AppMessage>,
) -> Result<PostProcessResult, AppMessage> {
    let execution=prepare(root,request)?;
    Ok(execute_prepared(execution,worker))
}

pub(crate) fn execute_prepared(mut execution:Execution, worker:impl FnOnce(&Execution)->Result<PostProcessResult,AppMessage>)->PostProcessResult {
    let elapsed = std::time::Instant::now();
    let attempt = execution.start().and_then(|_| std::panic::catch_unwind(std::panic::AssertUnwindSafe(||worker(&execution)))
        .unwrap_or_else(|_|Err(AppMessage::known("processing.completion_unknown","Executor panicked; termination not confirmed"))));
    let (mut published, mut notices) = crate::processing_runs::receipts(
        &execution.directory,
        execution.identity(),
        execution.run().run_id.as_deref().unwrap(),
    );
    let validated = attempt.and_then(|r| {
        if !matches!(r.status.as_str(), "completed" | "partial")
            || r.manifest_path != execution.identity().manifest_path
            || r.source_master != execution.identity().source_master_path
            || !r.source_duration_seconds.is_finite()
            || r.source_duration_seconds <= 0.0
            || !r.elapsed_seconds.is_finite()
            || r.elapsed_seconds < 0.0
            || r.artifacts.iter().any(|a| {
                !a.source_timeline_offset_seconds.is_finite()
                    || !published.iter().any(|p| {
                        p.path == a.path
                            && p.role == a.role
                            && crate::recording_manifest::file_identity(Path::new(&p.path))
                                .is_ok_and(|id| id == p.file_identity)
                    })
            })
            || r.video_outputs.iter().any(|path| {
                path != &r.source_master
                    && !r
                        .artifacts
                        .iter()
                        .any(|a| &a.path == path && a.role == "user_output")
            })
            || r.transcripts.iter().any(|t| {
                !t["path"].as_str().is_some_and(|p| {
                    r.artifacts
                        .iter()
                        .any(|a| a.path == p && a.role == "transcript")
                })
            })
        {
            return Err(AppMessage::known(
                "processing.result_invalid",
                "Worker result identity, status or publication validation failed",
            ));
        }
        for p in &mut published {
            if let Some(a) = r
                .artifacts
                .iter()
                .find(|a| a.path == p.path && a.role == p.role)
            {
                p.source_timeline_offset_seconds = Some(a.source_timeline_offset_seconds);
            }
        }
        Ok(r)
    });
    let mut run = execution.run().clone();
    let mut result = match validated {
        Ok(r) => {
            run.status = r.status.clone();
            run.warnings = r.warnings.clone();
            run.transcription = r.transcription.clone();
            r
        }
        Err(error) => {
            run.status = if error.code.as_deref() == Some("processing.cancelled") {
                "cancelled"
            } else if error.code.as_deref() == Some("processing.completion_unknown") {
                "unknown"
            } else if run.started_at_unix_ms.is_some() {
                "failed"
            } else {
                "not-executed"
            }
            .into();
            run.error = Some(error.clone());
            PostProcessResult {
                status: run.status.clone(),
                manifest_path: execution.identity().manifest_path.clone(),
                source_master: execution.identity().source_master_path.clone(),
                error: Some(error),
                elapsed_seconds: elapsed.elapsed().as_secs_f64(),
                ..Default::default()
            }
        }
    };
    run.published_artifacts = published;
    run.artifact_paths = run
        .published_artifacts
        .iter()
        .map(|a| a.path.clone())
        .collect();
    run.warnings.append(&mut notices);
    let view = execution.finish(run);
    if !view.outbox_saved && !view.manifest_applied {
        RECORDING_UNAVAILABLE.store(true, Ordering::SeqCst);
    }
    if let Some(error) = &view.record_error {
        result.history_warnings.push(error.clone());
    }
    result.run = Some(view);
    result
}

#[tauri::command]
pub async fn transcription_languages(app: AppHandle) -> Result<Value, AppMessage> {
    let helper = resource_or_development_path(
        &app,
        "bin/apple-transcriber",
        "../native/apple-speech-transcriber/.build/release/apple-transcriber",
    )?;
    tauri::async_runtime::spawn_blocking(move || {
        let output = Command::new(helper)
            .arg("--locales")
            .output()
            .map_err(|error| {
                AppMessage::known("transcription.capabilities_failed", error.to_string())
            })?;
        if !output.status.success() {
            return Err(AppMessage::known(
                "transcription.capabilities_failed",
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| {
            AppMessage::known("transcription.capabilities_failed", error.to_string())
        })
    })
    .await
    .map_err(|error| AppMessage::known("transcription.capabilities_failed", error.to_string()))?
}

#[tauri::command]
pub async fn process_recording(
    app: AppHandle,
    manager: tauri::State<'_, crate::processing_manager::ProcessingManager>,
    request_id: String,
    request: PostProcessRequest,
    selection_revision: Option<u64>,
) -> Result<crate::processing_manager::ProcessingSnapshot, AppMessage> {
    if let Some(revision) = selection_revision {
        app.state::<crate::history_window::HistoryWindowState>().check_target(revision, &request.manifest_path)?;
    }
    if RECORDING_UNAVAILABLE.load(Ordering::SeqCst) {
        return Err(crate::processing_runs::problem("Previous run could not be saved in either location; no new worker started in this process"));
    }
    let worker = resource_or_development_path(
        &app,
        "worker/capture_scribe_worker.py",
        "../worker/capture_scribe_worker.py",
    )?;
    let transcriber = resource_or_development_path(
        &app,
        "bin/apple-transcriber",
        "../native/apple-speech-transcriber/.build/release/apple-transcriber",
    )?;
    // Nothing has started or been written yet; run records cannot be kept.
    let root = crate::history_index::root(&app)
        .map_err(|e| crate::history_index::storage_problem(e, "processing.storage_unavailable"))?;
    let manager=manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move||{
        let emit=app.clone();
        manager.submit(&root,request_id,request,Arc::new(move|s|{let _=emit.emit("processing-snapshot",s);}),move |execution,request,stop,progress|{
            let mut result=execute_prepared(execution,|e|run_worker(progress,worker,transcriber,request,e,stop));
            if result.run.as_ref().is_some_and(|r|r.manifest_applied){result.history_warnings.extend(crate::history_index::register(&app,Path::new(&result.manifest_path),0));}
            result
        })
    }).await.map_err(|e|AppMessage::from(e.to_string()))?
}

fn worker_message(line: &str) -> Option<(String, AppMessage)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event = value.get("event")?.as_str()?;
    if event != "progress" && event != "error" {
        return None;
    }
    let message: AppMessage = serde_json::from_value(value.get("message")?.clone()).ok()?;
    Some((event.into(), message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{executable, worker_message, PostProcessRequest};
    use crate::processing_runs::tests::Fixture;
    fn request(f: &Fixture) -> PostProcessRequest {
        PostProcessRequest {
            manifest_path: f.path.to_string_lossy().into_owned(),
            trim_trailing_silence: false,
            split_long_video: true,
            split_threshold_minutes: 10,
            split_segment_minutes: 7,
            transcribe: true,
            transcription_locale: Some("en-US".into()),
        }
    }
    fn result(e: &Execution, status: &str) -> PostProcessResult {
        PostProcessResult {
            status: status.into(),
            manifest_path: e.identity().manifest_path.clone(),
            source_master: e.identity().source_master_path.clone(),
            source_duration_seconds: 12.0,
            video_outputs: vec![e.identity().source_master_path.clone()],
            elapsed_seconds: 0.1,
            ..Default::default()
        }
    }

    #[test]
    fn one_recording_completed_partial_and_fatal_transport_failures_are_saved() {
        let f = Fixture::new();
        let source = std::fs::read(&f.source).unwrap();
        for status in ["completed", "partial"] {
            let r = execute_with(&f.history, &request(&f), |e| {
                let mut r = result(e, status);
                if status == "partial" {
                    r.warnings.push(
                        AppMessage::known("transcription.empty", "empty input")
                            .param("fileName", "Synthetic.mp4"),
                    );
                }
                Ok(r)
            })
            .unwrap();
            assert_eq!(r.status, status);
            assert!(r.run.unwrap().manifest_applied);
        }
        for code in ["spawn", "exit", "json"] {
            let r=execute_with(&f.history,&request(&f),|_|{
                let mut command=if code=="spawn"{Command::new("/capturescribe-fixture-missing-python")}else{let mut c=Command::new("/usr/bin/python3");c.args(["-c",if code=="exit"{"import sys;sys.stderr.write('{\"event\":\"error\",\"message\":{\"code\":\"processing.split_failed\",\"params\":{\"fixture\":\"keep\"},\"raw\":\"injected fatal\"}}\\n');sys.exit(3)"}else{"print('invalid JSON')"}]);c};
                command.env("PYTHONDONTWRITEBYTECODE","1");collect_worker(command,Arc::new(|_|{}))
            }).unwrap();
            assert_eq!(r.status, "failed");
            let view = r.run.unwrap();
            assert!(view.outbox_saved && view.manifest_applied);
            assert!(view.run.error.is_some());
            if code == "exit" {
                assert_eq!(view.run.error.unwrap().params["fixture"], "keep");
            }
        }
        assert_eq!(f.value()["processingRuns"].as_array().unwrap().len(), 5);
        assert_eq!(std::fs::read(&f.source).unwrap(), source);
        let item = crate::history::parse_item(&f.path).unwrap();
        assert_eq!(item.processing_status.as_deref(), Some("failed"));
        assert_eq!(item.processing_runs[0].status, "completed");
    }
    #[test]
    fn rejected_input_does_not_accept_or_start_a_worker() {
        let f = Fixture::new();
        let mut r = request(&f);
        r.split_segment_minutes = 0;
        assert!(execute_with(&f.history, &r, |_| panic!("worker must not run")).is_err());
        assert!(!f.history.exists());
        r = request(&f);
        std::fs::remove_file(&f.source).unwrap();
        assert!(
            execute_with(&f.history, &r, |_| panic!("missing input must be rejected")).is_err()
        );
        assert!(!f.history.exists());
    }
    #[test]
    fn worker_success_with_manifest_failure_is_not_reclassified_as_media_failure() {
        let f = Fixture::new();
        let original = std::fs::read(&f.path).unwrap();
        let r = execute_with(&f.history, &request(&f), |e| {
            let r = result(e, "completed");
            std::fs::write(&f.path, br#"{"schemaVersion":99}"#).unwrap();
            Ok(r)
        })
        .unwrap();
        assert_eq!(r.status, "completed");
        assert!(r.error.is_none());
        let view = r.run.unwrap();
        assert!(view.outbox_saved && !view.manifest_applied);
        assert_eq!(r.history_warnings.len(), 1);
        std::fs::write(&f.path, original).unwrap();
        let (v, w) = crate::processing_runs::reconcile(&f.history);
        assert!(w.is_empty());
        assert!(v[0].manifest_applied);
        assert_eq!(f.value()["processingRuns"][0]["status"], "completed");
    }
    #[test]
    fn partial_publication_is_attributed_only_by_receipts_and_checked_for_replacement() {
        let f = Fixture::new();
        let old = f.root.join("old.txt");
        std::fs::write(&old, b"old result").unwrap();
        let r=execute_with(&f.history,&request(&f),|e|{
            let output=f.root.join("Synthetic_part1.mp4");std::fs::write(&output,b"published fixture").unwrap();
            let receipt=serde_json::json!({"format":"capturescribe.publication","schemaVersion":1,"runId":e.run().run_id,"jobId":e.identity().job_id,"manifestPath":e.identity().manifest_path,"sourceMaster":e.identity().source_master_path,"artifact":{"role":"user_output","path":output,"validation":"publication-only","sourceTimelineOffsetSeconds":null,"fileIdentity":crate::recording_manifest::file_identity(&output).unwrap()}});
            std::fs::write(e.directory.join("published-fixture.json"),serde_json::to_vec(&receipt).unwrap()).unwrap();
            Err(AppMessage::known("processing.split_failed",format!("Not a receipt: {}",old.display())))
        }).unwrap();
        let view = r.run.unwrap();
        assert_eq!(r.status, "failed");
        assert_eq!(view.run.artifact_paths.len(), 1);
        assert_eq!(view.artifacts[0].availability, "available");
        assert_eq!(
            view.run.published_artifacts[0].source_timeline_offset_seconds,
            None
        );
        assert_eq!(std::fs::read(&old).unwrap(), b"old result");
        let output = PathBuf::from(&view.artifacts[0].path);
        let replacement = f.root.join("replacement.tmp");
        std::fs::write(&replacement, b"different file").unwrap();
        std::fs::rename(&replacement, &output).unwrap();
        let (v, _) = crate::processing_runs::reconcile(&f.history);
        assert_eq!(v[0].artifacts[0].availability, "unavailable");
        let item = crate::history::parse_item(&f.path).unwrap();
        assert_eq!(
            item.artifacts
                .iter()
                .find(|a| a.path == output.to_string_lossy())
                .unwrap()
                .availability,
            "unavailable"
        );
    }
    #[test]
    fn unreceipted_or_wrong_recording_results_fail_validation_without_guessed_outputs() {
        let f = Fixture::new();
        for mismatch in [false, true] {
            let r = execute_with(&f.history, &request(&f), |e| {
                let mut r = result(e, "completed");
                if mismatch {
                    r.source_master = "/fixture/wrong-source.mp4".into();
                } else {
                    r.artifacts.push(WorkerArtifact {
                        role: "user_output".into(),
                        path: f
                            .root
                            .join("unattributed.mp4")
                            .to_string_lossy()
                            .into_owned(),
                        state: "available".into(),
                        source_timeline_offset_seconds: 0.0,
                    });
                }
                Ok(r)
            })
            .unwrap();
            assert_eq!(r.status, "failed");
            assert!(r.run.unwrap().run.artifact_paths.is_empty());
        }
    }

    #[test]
    fn recognition_locale_is_required_only_when_enabled_and_is_forwarded_verbatim() {
        let mut base = serde_json::json!({"manifestPath":"/fixture.capture.json","trimTrailingSilence":false,"splitLongVideo":false,"splitThresholdMinutes":10,"splitSegmentMinutes":10,"transcribe":true});
        for locale in ["ja-JP", "en-US"] {
            base["transcriptionLocale"] = serde_json::json!(locale);
            let request: PostProcessRequest = serde_json::from_value(base.clone()).unwrap();
            assert_eq!(
                request.transcription_arguments().unwrap(),
                ["--transcription-locale", locale]
            );
        }
        for locale in [
            serde_json::Value::Null,
            serde_json::json!("fr-FR"),
            serde_json::json!(""),
        ] {
            base["transcriptionLocale"] = locale;
            let request: PostProcessRequest = serde_json::from_value(base.clone()).unwrap();
            assert!(request.transcription_arguments().is_err());
            base["transcribe"] = serde_json::json!(false);
            let off: PostProcessRequest = serde_json::from_value(base.clone()).unwrap();
            assert!(off.transcription_arguments().unwrap().is_empty());
            base["transcribe"] = serde_json::json!(true);
        }
    }

    #[test]
    fn split_values_are_required_validated_and_forwarded_without_defaults() {
        let base = serde_json::json!({"manifestPath":"/fixture.capture.json","trimTrailingSilence":false,"splitLongVideo":true,"transcribe":false,"splitThresholdMinutes":30,"splitSegmentMinutes":7});
        let request: PostProcessRequest = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(
            request.split_arguments().unwrap(),
            [
                "--split-threshold-minutes",
                "30",
                "--split-segment-minutes",
                "7"
            ]
        );
        for value in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("10"),
            serde_json::json!(4294967296u64),
            serde_json::Value::Null,
        ] {
            let mut bad = base.clone();
            bad["splitSegmentMinutes"] = value;
            assert!(serde_json::from_value::<PostProcessRequest>(bad)
                .map(|r| r.split_arguments().is_err())
                .unwrap_or(true));
        }
        let mut missing = base;
        missing
            .as_object_mut()
            .unwrap()
            .remove("splitThresholdMinutes");
        assert!(serde_json::from_value::<PostProcessRequest>(missing).is_err());
    }

    #[test]
    fn structured_worker_failure_keeps_swift_cause_and_raw() {
        let (_, message) = worker_message(r#"{"event":"error","message":{"code":"transcription.empty","params":{"fileName":"a.mp4"},"raw":"CLIError.emptyTranscript"}}"#).unwrap();
        assert_eq!(message.code.as_deref(), Some("transcription.empty"));
        assert_eq!(message.raw.as_deref(), Some("CLIError.emptyTranscript"));
        assert_eq!(message.params["fileName"], "a.mp4");
        assert!(worker_message("AVFoundationErrorDomain Code=-11800").is_none());
    }

    #[test]
    #[ignore = "requires the built Swift CLI; run with CAPTURE_SCRIBE_MESSAGE_CLI"]
    fn live_swift_python_rust_unknown_error_keeps_raw() {
        let helper = std::env::var("CAPTURE_SCRIBE_MESSAGE_CLI").expect("built Swift CLI path");
        let script = r#"
import json, sys
from worker.capture_scribe_worker import run_command, error_message
try:
    run_command([sys.argv[1], '/capture-scribe-message-fixture-input-not-found.wav', '/tmp/unused-transcript.txt', '--locale', 'ja-JP'], 'transcription.failed')
except Exception as error:
    print(json.dumps({'event': 'error', 'message': error_message(error)}))
"#;
        let output = std::process::Command::new("/usr/bin/python3")
            .current_dir(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap(),
            )
            .args(["-c", script, &helper])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (_, message) = worker_message(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
        assert!(message.code.is_none());
        let raw = message.raw.unwrap();
        assert!(raw.contains("inputNotFound"));
        assert!(raw.contains("/capture-scribe-message-fixture-input-not-found.wav"));
    }

    #[test]
    fn executable_uses_fallback_when_standard_paths_do_not_exist() {
        assert_eq!(
            executable(
                "CAPTURE_SCRIBE_TEST_EXECUTABLE_UNSET",
                &["/path/that/does/not/exist"],
                "fallback-tool",
            ),
            "fallback-tool"
        );
    }
}
