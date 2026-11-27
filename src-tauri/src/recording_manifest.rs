use crate::message::AppMessage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingManifest {
    schema_version: u32,
    job_id: String,
    capture_engine_id: String,
    source_timeline: SourceTimeline,
    artifacts: Vec<Artifact>,
    recovery: RecoveryState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    processing_runs: Vec<ProcessingRun>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceTimeline {
    origin: String,
    duration_seconds: f64,
    mapping_report: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    role: String,
    path: String,
    state: String,
    physical_identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_timeline_offset_seconds: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryState {
    status: String,
    reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingRun {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_unix_ms: Option<u128>,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<AppMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription: Option<TranscriptionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_schema_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at_unix_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<ProcessingSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppMessage>,
    #[serde(default)]
    pub published_artifacts: Vec<PublishedArtifact>,
    // Retain extensions when reading old/forward-compatible v2 runs.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessingSettings {
    pub trim_trailing_silence: bool,
    pub split_long_video: bool,
    pub split_threshold_minutes: u64,
    pub split_segment_minutes: u64,
    pub transcribe: bool,
    pub transcription_locale: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishedArtifact {
    pub role: String,
    pub path: String,
    // Publication/metadata presence is not full video validation.
    pub validation: String,
    pub source_timeline_offset_seconds: Option<f64>,
    pub file_identity: String,
}

impl ProcessingRun {
    pub fn terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed"
                | "partial"
                | "failed"
                | "cancelled"
                | "not-executed"
                | "interrupted"
                | "unknown"
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptionInfo {
    pub requested_locale: String,
    pub used_locales: Vec<String>,
}

pub struct DerivedArtifact<'a> {
    pub role: &'a str,
    pub path: &'a str,
    pub state: &'a str,
    pub source_timeline_offset_seconds: f64,
}

/// Persist product-level roles separately from the current physical layout.
///
/// In the first unprocessed workflow one validated file is both Source Master
/// and User Output. Later processing can add Recovery Data and derived outputs
/// without changing the capture engine contract.
#[cfg(test)]
pub fn write_initial_manifest(
    job_id: &str,
    engine_id: &str,
    source_master: &Path,
    report: &Path,
    duration_seconds: f64,
    destination: &Path,
) -> Result<PathBuf, AppMessage> {
    write_initial_manifest_for_source(
        job_id, engine_id, source_master, report, format!("source-master-{job_id}"),
        duration_seconds, destination,
    )
}

pub(crate) fn write_initial_manifest_for_source(
    job_id: &str,
    engine_id: &str,
    source_master: &Path,
    report: &Path,
    physical_identity: String,
    duration_seconds: f64,
    destination: &Path,
) -> Result<PathBuf, AppMessage> {
    let manifest = RecordingManifest {
        schema_version: 2,
        job_id: job_id.to_string(),
        capture_engine_id: engine_id.to_string(),
        source_timeline: SourceTimeline {
            origin: "capture-start".into(),
            duration_seconds,
            mapping_report: report.to_string_lossy().into_owned(),
        },
        artifacts: vec![
            Artifact {
                role: "source_master".into(),
                path: source_master.to_string_lossy().into_owned(),
                state: "validated".into(),
                physical_identity: physical_identity.clone(),
                source_timeline_offset_seconds: Some(0.0),
            },
            Artifact {
                role: "user_output".into(),
                path: source_master.to_string_lossy().into_owned(),
                state: "available".into(),
                physical_identity,
                source_timeline_offset_seconds: Some(0.0),
            },
            Artifact {
                role: "capture_report".into(),
                path: report.to_string_lossy().into_owned(),
                state: "available".into(),
                physical_identity: format!("capture-report-{job_id}"),
                source_timeline_offset_seconds: None,
            },
        ],
        recovery: RecoveryState {
            status: "not_separately_required".into(),
            reason: "unprocessed source master and user output share one validated physical file"
                .into(),
        },
        processing_runs: Vec::new(),
    };
    let path = destination.to_path_buf();
    // Initial capture must never replace a pre-existing manifest.
    let data = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| AppMessage::known("manifest.write_failed", error.to_string()))?;
    file.write_all(&data)
        .and_then(|_| file.sync_all())
        .map_err(|error| AppMessage::known("manifest.write_failed", error.to_string()))?;
    Ok(path)
}

#[cfg(test)]
pub fn append_processing_result(
    manifest_path: &Path,
    status: &str,
    artifacts: &[DerivedArtifact<'_>],
    warnings: &[AppMessage],
) -> Result<(), AppMessage> {
    append_processing_result_with_transcription(manifest_path, status, artifacts, warnings, None)
}

pub fn append_processing_result_with_transcription(
    manifest_path: &Path,
    status: &str,
    artifacts: &[DerivedArtifact<'_>],
    warnings: &[AppMessage],
    transcription: Option<TranscriptionInfo>,
) -> Result<(), AppMessage> {
    let finished_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    let mut published_artifacts = Vec::new();
    for input in artifacts {
        if !Path::new(input.path).is_file() {
            return Err(AppMessage::known(
                "processing.output_missing",
                format!("worker output missing: {}", input.path),
            )
            .param("path", input.path));
        }
        published_artifacts.push(PublishedArtifact {
            role: input.role.into(),
            path: input.path.into(),
            validation: input.state.into(),
            file_identity: file_identity(Path::new(input.path))?,
            source_timeline_offset_seconds: Some(input.source_timeline_offset_seconds),
        });
    }
    let run = ProcessingRun {
        status: status.to_string(),
        finished_at_unix_ms: Some(finished_at_unix_ms),
        artifact_paths: artifacts.iter().map(|a| a.path.to_string()).collect(),
        warnings: warnings.to_vec(),
        transcription,
        published_artifacts,
        ..Default::default()
    };
    with_manifest(manifest_path, || {
        let mut value = read_supported(manifest_path)?;
        add_run(&mut value, &run)?;
        write_manifest_value(manifest_path, &value)
    })
}

pub(crate) fn file_identity(path: &Path) -> Result<String, AppMessage> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(format!("unix:{}:{}", meta.dev(), meta.ino()));
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "metadata:{}:{}",
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }
}

pub(crate) fn read_supported(path: &Path) -> Result<serde_json::Value, AppMessage> {
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("Manifest must be a regular file".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if value["schemaVersion"] != 2 {
        return Err(AppMessage::known(
            "manifest.unsupported",
            "Unsupported manifest; original retained",
        )
        .param("version", value["schemaVersion"].to_string()));
    }
    if !value.is_object()
        || !value["artifacts"].is_array()
        || value.get("processingRuns").is_some_and(|v| !v.is_array())
    {
        return Err("Invalid manifest structure; original retained".into());
    }
    if let Some(runs) = value["processingRuns"].as_array() {
        for run in runs {
            if run
                .get("runSchemaVersion")
                .is_some_and(|v| v.as_u64() != Some(1))
            {
                return Err(AppMessage::known(
                    "processing.record_unavailable",
                    "Unsupported run version; original retained",
                ));
            }
            serde_json::from_value::<ProcessingRun>(run.clone()).map_err(|e| e.to_string())?;
        }
    }
    Ok(value)
}

// Stable sidecar OS lock, not the rebuildable history-index lock. Only held for
// read/validate/modify/publish, never while a worker runs. Do not remove locks.
pub(crate) fn with_manifest<T>(
    path: &Path,
    action: impl FnOnce() -> Result<T, AppMessage>,
) -> Result<T, AppMessage> {
    let path = fs::canonicalize(path).map_err(|e| e.to_string())?;
    let lock_path = path.with_extension("json.processing.lock");
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file()) {
        return Err("Invalid manifest lock".into());
    }
    let lock = opts.open(lock_path).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    action()
}

pub(crate) fn add_run(
    value: &mut serde_json::Value,
    run: &ProcessingRun,
) -> Result<(), AppMessage> {
    let incoming = serde_json::to_value(run).map_err(|e| e.to_string())?;
    if let Some(id) = &run.run_id {
        if let Some(existing) = value["processingRuns"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["runId"].as_str() == Some(id)))
        {
            if existing == &incoming {
                return Ok(());
            }
            return Err(AppMessage::known(
                "processing.record_unavailable",
                "Conflicting run ID; previous run retained",
            ));
        }
    }
    let physical_run_id = run
        .run_id
        .clone()
        .unwrap_or_else(|| format!("legacy-{}", crate::history_index::token()));
    let entries = value["artifacts"]
        .as_array_mut()
        .ok_or("Missing artifacts")?;
    for (index, input) in run.published_artifacts.iter().enumerate() {
        // A receipt remains a historical reference if its file went missing;
        // history rechecks availability rather than deleting the reference.
        entries.push(
            serde_json::json!({"role":input.role,"path":input.path,"state":"published",
            "physicalIdentity":format!("processing-{physical_run_id}-{index}"),
            "sourceTimelineOffsetSeconds":input.source_timeline_offset_seconds}),
        );
    }
    if !run.published_artifacts.is_empty() {
        if value.get("recovery").is_none() {
            value["recovery"] = serde_json::json!({});
        }
        let recovery = value["recovery"]
            .as_object_mut()
            .ok_or("Invalid recovery object; original retained")?;
        recovery.insert("status".into(), serde_json::json!("source_master_retained"));
        recovery.insert(
            "reason".into(),
            serde_json::json!("derived outputs created without modifying Source Master"),
        );
    }
    if value.get("processingRuns").is_none() {
        value["processingRuns"] = serde_json::json!([]);
    }
    value["processingRuns"]
        .as_array_mut()
        .ok_or("Invalid runs")?
        .push(incoming);
    Ok(())
}

pub(crate) fn write_manifest_value(
    path: &Path,
    manifest: &serde_json::Value,
) -> Result<(), AppMessage> {
    write_manifest_value_with(path, manifest, || Ok(()))
}
fn write_manifest_value_with(
    path: &Path,
    manifest: &serde_json::Value,
    before_publish: impl FnOnce() -> Result<(), AppMessage>,
) -> Result<(), AppMessage> {
    let data = serde_json::to_vec_pretty(manifest).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!(
        "capture.json.{}.tmp",
        crate::history_index::token()
    ));
    let mut file = crate::history_index::private_file(&temporary)
        .map_err(|error| AppMessage::known("manifest.write_failed", error.to_string()))?;
    file.write_all(&data)
        .and_then(|_| file.sync_all())
        .map_err(|error| AppMessage::known("manifest.write_failed", error.to_string()))?;
    let candidate: serde_json::Value =
        serde_json::from_slice(&fs::read(&temporary).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if candidate != *manifest {
        return Err("Manifest candidate readback mismatch".into());
    }
    before_publish()?;
    fs::rename(&temporary, path)
        .map_err(|error| AppMessage::known("manifest.write_failed", error.to_string()))
}

#[cfg(test)]
mod save_tests {
    use super::*;
    #[test]
    fn failed_candidate_publication_preserves_current_manifest_and_unknown_extensions() {
        let f = crate::processing_runs::tests::Fixture::new();
        let before = fs::read(&f.path).unwrap();
        let mut candidate = f.value();
        candidate["extension"] = serde_json::json!("keep");
        assert!(write_manifest_value_with(&f.path, &candidate, || Err(
            "injected interruption before rename".into()
        ))
        .is_err());
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
    #[test]
    fn processing_update_does_not_overwrite_an_unrelated_old_temp_file() {
        let root = std::env::temp_dir().join(format!(
            "capturescribe-manifest-temp-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("clip.mp4");
        let manifest = source.with_extension("capture.json");
        write_initial_manifest(
            "job",
            "engine",
            &source,
            &root.join("clip.run.json"),
            1.0,
            &manifest,
        )
        .unwrap();
        let unrelated = manifest.with_extension("capture.json.tmp");
        fs::write(&unrelated, b"unrelated existing file").unwrap();
        append_processing_result(&manifest, "completed", &[], &[]).unwrap();
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated existing file");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(value["processingRuns"][0]["status"], "completed");
        assert!(value["processingRuns"][0].get("transcription").is_none());
        append_processing_result_with_transcription(
            &manifest,
            "completed",
            &[],
            &[],
            Some(TranscriptionInfo {
                requested_locale: "en-US".into(),
                used_locales: vec!["en-US".into()],
            }),
        )
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(
            value["processingRuns"][1]["transcription"]["usedLocales"][0],
            "en-US"
        );
        let item = crate::history::parse_item(&manifest).unwrap();
        assert_eq!(item.transcription.unwrap().requested_locale, "en-US");
        // Full-source TXT is one artifact per run; previous per-part/full TXT
        // remains associated with its own run and visible through history.
        for name in [
            "clip_part1_文字起こし.txt",
            "clip_文字起こし.txt",
            "clip_文字起こし (2).txt",
        ] {
            let txt = root.join(name);
            fs::write(&txt, name.as_bytes()).unwrap();
            append_processing_result_with_transcription(
                &manifest,
                "completed",
                &[DerivedArtifact {
                    role: "transcript",
                    path: txt.to_str().unwrap(),
                    state: "available",
                    source_timeline_offset_seconds: 0.0,
                }],
                &[],
                Some(TranscriptionInfo {
                    requested_locale: "en-US".into(),
                    used_locales: vec!["en-US".into()],
                }),
            )
            .unwrap();
        }
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(value["processingRuns"].as_array().unwrap().len(), 5);
        assert!(value["processingRuns"][2]["artifactPaths"][0]
            .as_str()
            .unwrap()
            .ends_with("clip_part1_文字起こし.txt"));
        assert!(value["processingRuns"][3]["artifactPaths"][0]
            .as_str()
            .unwrap()
            .ends_with("clip_文字起こし.txt"));
        assert!(value["processingRuns"][4]["artifactPaths"][0]
            .as_str()
            .unwrap()
            .ends_with("clip_文字起こし (2).txt"));
        let item = crate::history::parse_item(&manifest).unwrap();
        assert_eq!(item.transcript_count, 3);
        assert_eq!(
            item.artifacts
                .iter()
                .filter(|a| a.role == "transcript" && a.availability == "available")
                .count(),
            3
        );
        assert_eq!(
            fs::read(root.join("clip_文字起こし.txt")).unwrap(),
            "clip_文字起こし.txt".as_bytes()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn initial_manifest_does_not_replace_an_existing_file() {
        let root = std::env::temp_dir().join(format!(
            "capturescribe-manifest-save-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let source = root.join("clip.mp4");
        let manifest = source.with_extension("capture.json");
        fs::write(&manifest, b"existing manifest").unwrap();
        assert!(write_initial_manifest(
            "job",
            "test",
            &source,
            &root.join("clip.run.json"),
            1.0,
            &manifest
        )
        .is_err());
        assert_eq!(fs::read(&manifest).unwrap(), b"existing manifest");
        fs::remove_dir_all(root).unwrap();
    }
}
