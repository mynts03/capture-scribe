use crate::message::AppMessage;
use crate::save_destination::MANAGEMENT_DIRECTORY;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryArtifact {
    pub role: String,
    pub path: String,
    pub physical_identity: String,
    pub availability: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecordingHistoryItem {
    pub record_id: String,
    pub job_id: String,
    pub recorded_at_unix_ms: Option<u128>,
    pub manifest_state: String,
    pub snapshot_stale: bool,
    pub last_confirmed_unix_ms: u128,
    pub artifacts: Vec<HistoryArtifact>,
    pub processing_available: bool,
    pub manifest_path: String,
    pub name: String,
    pub source_master_path: String,
    pub source_master_available: bool,
    pub duration_seconds: f64,
    pub processing_status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processing_runs: Vec<crate::recording_manifest::ProcessingRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription: Option<crate::recording_manifest::TranscriptionInfo>,
    pub warning_count: usize,
    pub warnings: Vec<AppMessage>,
    pub transcript_count: usize,
    pub user_output_count: usize,
    pub modified_at_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingHistoryResult {
    pub items: Vec<RecordingHistoryItem>,
    pub warnings: Vec<AppMessage>,
    pub capture_failures: Vec<crate::capture_failures::CaptureFailureView>,
    pub processing_run_updates: Vec<crate::processing_runs::RunView>,
}

pub fn availability(path: &str) -> String {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => "available",
        Ok(_) => "unavailable",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unavailable",
    }
    .into()
}

fn source_masters(value: &Value) -> impl Iterator<Item = &Value> {
    value["artifacts"].as_array().into_iter().flatten().filter(|artifact| {
        artifact["role"] == "source_master" && artifact["state"] == "validated"
    })
}

fn managed_recording_directory(path: &Path) -> Option<&Path> {
    let parent = path.parent()?;
    if parent.file_name()?.to_str()? == MANAGEMENT_DIRECTORY {
        parent.parent()
    } else {
        None
    }
}

fn manifest_location(path: &Path) -> Result<(), AppMessage> {
    if managed_recording_directory(path).is_some() {
        // Checking only the leaf would follow a replaced management directory
        // into another tree even when the target is itself a regular file.
        let parent = path.parent().unwrap();
        if !fs::symlink_metadata(parent).map_err(|e| e.to_string())?.is_dir()
            || fs::canonicalize(parent).map_err(|e| e.to_string())? != parent
        {
            return Err("Recording management folder is not the original regular directory".into());
        }
    }
    if !fs::symlink_metadata(path).map_err(|e| e.to_string())?.is_file() {
        return Err("Not a regular manifest file".into());
    }
    Ok(())
}

fn expected_managed_path(manifest: &Path, directory: &Path, suffix: &str) -> Option<PathBuf> {
    let stem = manifest.file_name()?.to_str()?.strip_suffix(".capture.json")?;
    if stem.is_empty() {
        return None;
    }
    Some(directory.join(format!("{stem}{suffix}")))
}

fn source_available(manifest: &Path, source: &Path, expected_identity: &str) -> bool {
    let Ok(actual_identity) = crate::recording_manifest::file_identity(source) else {
        return false;
    };
    let Some(directory) = managed_recording_directory(manifest) else {
        return true; // Existing manifests use a logical recording ID.
    };
    expected_managed_path(manifest, directory, ".mp4").as_deref() == Some(source)
        && !expected_identity.is_empty()
        && actual_identity == expected_identity
}

fn import_problem(path: &Path, detail: impl Into<String>) -> AppMessage {
    AppMessage::known("history.import_skipped", detail)
        .param("path", path.to_string_lossy().into_owned())
}

fn manifest_paths(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(|name| name.ends_with(".capture.json")) {
            entries.push(entry.path());
        }
    }
    Ok(entries)
}

fn managed_manifest_paths(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let managed = directory.join(MANAGEMENT_DIRECTORY);
    match fs::symlink_metadata(&managed) {
        Ok(meta) if meta.is_dir() => manifest_paths(&managed),
        Ok(_) => Err(std::io::Error::new(ErrorKind::InvalidData, "Not a regular management folder")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub(crate) fn has_manifest_counterpart(path: &Path) -> std::io::Result<bool> {
    let Some(parent) = path.parent() else { return Ok(false); };
    let Some(name) = path.file_name() else { return Ok(false); };
    let peer = if let Some(directory) = managed_recording_directory(path) {
        directory.join(name)
    } else {
        let peer_dir = parent.join(MANAGEMENT_DIRECTORY);
        match fs::symlink_metadata(&peer_dir) {
            Ok(meta) if meta.is_dir() => peer_dir.join(name),
            Ok(_) => return Ok(false), // Never follow a management-folder symlink.
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    };
    match fs::symlink_metadata(peer) {
        Ok(_) => Ok(true), // Even a malformed or symbolic-link peer reserves this name.
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn same_source(left: &Path, right: &Path) -> bool {
    left.is_absolute() && right.is_absolute()
        && (left == right
            || crate::recording_manifest::file_identity(left).is_ok_and(|identity| {
                crate::recording_manifest::file_identity(right)
                    .is_ok_and(|right_identity| identity == right_identity)
            }))
}

fn processing_manifest_problem(detail: impl Into<String>) -> AppMessage {
    AppMessage::known("processing.record_unavailable", detail)
}

pub(crate) fn require_unambiguous_manifest(
    path: &Path,
    source: &Path,
    expected_identity: &str,
) -> Result<(), AppMessage> {
    manifest_location(path).map_err(|error| processing_manifest_problem(error.to_string()))?;
    if !source_available(path, source, expected_identity) {
        if fs::symlink_metadata(source).is_err_and(|error| error.kind() == ErrorKind::NotFound) {
            return Err(AppMessage::known("processing.source_missing", "Source Master unavailable")
                .param("path", source.to_string_lossy().into_owned()));
        }
        return Err(processing_manifest_problem("Source Master does not match this manifest"));
    }
    if has_manifest_counterpart(path).map_err(|error| processing_manifest_problem(error.to_string()))? {
        return Err(processing_manifest_problem("Competing manifest in the other recording layout"));
    }
    let directory = managed_recording_directory(path).or_else(|| path.parent())
        .ok_or_else(|| processing_manifest_problem("Manifest has no containing folder"))?;
    let media_directory = source.parent()
        .ok_or_else(|| processing_manifest_problem("Source Master has no containing folder"))?;
    let candidates = |directory: &Path| -> Result<Vec<PathBuf>, AppMessage> {
        let mut paths = manifest_paths(directory).map_err(|e| processing_manifest_problem(e.to_string()))?;
        match managed_manifest_paths(directory) {
            Ok(mut managed) => paths.append(&mut managed),
            Err(error) if error.kind() == ErrorKind::InvalidData => (), // Not a supported placement.
            Err(error) => return Err(processing_manifest_problem(error.to_string())),
        }
        Ok(paths)
    };
    let mut peers = candidates(directory)?;
    if media_directory != directory {
        peers.extend(candidates(media_directory)?);
    }
    for peer in peers {
        if peer == path || fs::canonicalize(&peer).is_ok_and(|canonical| canonical == path) {
            continue;
        }
        if manifest_claims_source(&peer, source) {
            return Err(processing_manifest_problem("Multiple manifests refer to the same Source Master"));
        }
    }
    Ok(())
}

// A different manifest may be unusable for other reasons while still claiming
// this source. Record that claim before choosing any one of the manifests.
pub(crate) fn manifest_claims_source(manifest: &Path, source: &Path) -> bool {
    let value: Option<Value> = manifest_location(manifest).ok()
        .and_then(|_| fs::read(manifest).ok())
        .and_then(|data| serde_json::from_slice(&data).ok());
    value.is_some_and(|value| {
        value["schemaVersion"].as_u64() == Some(2)
            && source_masters(&value)
                .filter_map(|artifact| artifact["path"].as_str())
                .any(|claim| same_source(Path::new(claim), source))
    })
}

pub(crate) fn block_conflicting_manifest(item: &mut RecordingHistoryItem) {
    item.processing_available = false;
    item.manifest_state = "conflicting".into();
}

pub(crate) fn mark_source_conflicts(items: &mut [RecordingHistoryItem], warnings: &mut Vec<AppMessage>) {
    let mut claims: HashMap<_, Vec<_>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if !matches!(item.manifest_state.as_str(), "available" | "conflicting") {
            continue;
        }
        let source = Path::new(&item.source_master_path);
        if source.is_absolute() {
            claims.entry(("path", item.source_master_path.clone())).or_default().push(i);
            if let Ok(identity) = crate::recording_manifest::file_identity(source) {
                claims.entry(("file", identity)).or_default().push(i);
            }
        }
    }
    for matches in claims.values().filter(|matches| matches.len() > 1) {
        let first = matches[0];
        if matches.iter().all(|&i| items[i].manifest_path == items[first].manifest_path) {
            continue;
        }
        for &i in matches {
            if items[i].manifest_state != "conflicting" {
                warnings.push(import_problem(
                    Path::new(&items[i].manifest_path), "Multiple manifests refer to the same Source Master",
                ));
                block_conflicting_manifest(&mut items[i]);
            }
        }
    }
}

fn manifest_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".capture.json"))
        .unwrap_or("録画")
        .to_string()
}

pub(crate) fn parse_item(path: &Path) -> Result<RecordingHistoryItem, AppMessage> {
    manifest_location(path)?;
    let data = fs::read(path).map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_slice(&data).map_err(|error| error.to_string())?;
    if value.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        return Err(
            AppMessage::known("manifest.unsupported", "未対応のmanifest schemaです")
                .param("version", value["schemaVersion"].to_string()),
        );
    }
    // Keep the job visible even when its Source Master was moved or removed.
    // The history is also the place where that state should be discoverable.
    let mut sources = source_masters(&value);
    let source = sources.next();
    if sources.next().is_some() {
        return Err(import_problem(path, "Manifest declares multiple validated Source Masters"));
    }
    let source_path = source.and_then(|item| item["path"].as_str()).unwrap_or_default();
    let source_identity = source.and_then(|item| item["physicalIdentity"].as_str()).unwrap_or_default();
    let source_master_path = source_path.to_owned();
    let source_master_available = source_available(path, Path::new(source_path), source_identity);
    let artifacts = value
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or("artifact一覧がありません")?;
    let processing_runs = value
        .get("processingRuns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // An active/accepted run is not the latest confirmed result. Reconciliation
    // may append an older result later: prefer recorded acceptance/finish time.
    let latest_run = processing_runs.iter().enumerate()
        .filter(|(_,r)|!matches!(r["status"].as_str(),Some("accepted"|"running")))
        .max_by_key(|(i,r)|(r["acceptedAtUnixMs"].as_u64().or_else(||r["finishedAtUnixMs"].as_u64()).unwrap_or(0),*i)).map(|(_,r)|r);
    let warning_messages = latest_run
        .and_then(|run| run.get("warnings"))
        .map(|warnings| serde_json::from_value::<Vec<AppMessage>>(warnings.clone()))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let modified_at_unix_ms = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let recorded_at_unix_ms = artifacts
        .iter()
        .find(|a| a["role"] == "capture_report")
        .and_then(|a| a["path"].as_str())
        .filter(|report| {
            managed_recording_directory(path).is_none()
                || expected_managed_path(path, path.parent().unwrap(), ".run.json").as_deref()
                    == Some(Path::new(report))
        })
        .filter(|report| fs::symlink_metadata(report).is_ok_and(|m| m.is_file()))
        .and_then(|report| fs::read(report).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|report| report["startedAt"].as_str().map(str::to_owned))
        .and_then(|date| chrono::DateTime::parse_from_rfc3339(&date).ok())
        .and_then(|date| u128::try_from(date.timestamp_millis()).ok());
    let references = artifacts
        .iter()
        .filter_map(|a| {
            let path = a["path"].as_str()?;
            if !Path::new(path).is_absolute() {
                return None;
            }
            Some(HistoryArtifact {
                role: a["role"].as_str()?.into(),
                path: path.into(),
                physical_identity: a["physicalIdentity"].as_str().unwrap_or("").into(),
                availability: {
                    let current = availability(path);
                    let current = if path == source_path && !source_master_available && current == "available" {
                        "unavailable".into()
                    } else {
                        current
                    };
                    let expected=processing_runs.iter().filter_map(|r|r["publishedArtifacts"].as_array()).flatten().find(|a|a["path"].as_str()==Some(path)).and_then(|a|a["fileIdentity"].as_str());
                    if current=="available" && expected.is_some_and(|expected|!crate::recording_manifest::file_identity(Path::new(path)).is_ok_and(|id|id==expected)){"unavailable".into()}else{current}
                },
            })
        })
        .collect();
    if !source_master_path.is_empty() && !Path::new(&source_master_path).is_absolute() {
        return Err("Source Master path must be absolute".into());
    }

    Ok(RecordingHistoryItem {
        record_id: String::new(),
        job_id: value["jobId"].as_str().unwrap_or("").into(),
        recorded_at_unix_ms,
        manifest_state: "available".into(),
        snapshot_stale: false,
        last_confirmed_unix_ms: std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        artifacts: references,
        processing_available: source_master_available,
        manifest_path: path.to_string_lossy().into_owned(),
        name: manifest_name(path),
        source_master_available,
        source_master_path,
        duration_seconds: value
            .pointer("/sourceTimeline/durationSeconds")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        processing_status: latest_run
            .and_then(|run| run.get("status"))
            .and_then(Value::as_str)
            .map(str::to_string),
        processing_runs: processing_runs.iter().map(|r|serde_json::from_value(r.clone()).map_err(|e|e.to_string())).collect::<Result<_,_>>()?,
        warning_count: warning_messages.len(),
        transcription: latest_run.and_then(|run| run.get("transcription"))
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()))
            .transpose().map_err(|error| error.to_string())?,
        warnings: warning_messages,
        transcript_count: artifacts
            .iter()
            .filter(|artifact| artifact.get("role").and_then(Value::as_str) == Some("transcript"))
            .count(),
        user_output_count: artifacts
            .iter()
            .filter(|artifact| artifact.get("role").and_then(Value::as_str) == Some("user_output"))
            .count(),
        modified_at_unix_ms,
    })
}

pub(crate) fn scan_directory(
    output_directory: String,
) -> Result<RecordingHistoryResult, AppMessage> {
    let directory = PathBuf::from(output_directory.trim());
    if !directory.is_dir() {
        return Ok(RecordingHistoryResult {
            items: Vec::new(),
            warnings: Vec::new(),
            capture_failures: Vec::new(),
            processing_run_updates: Vec::new(),
        });
    }

    let mut items = Vec::new();
    let mut warnings = Vec::new();
    let mut paths = manifest_paths(&directory)
        .map_err(|error| format!("録画履歴の保存先を読み取れません: {error}"))?;
    let managed_unavailable = match managed_manifest_paths(&directory) {
        Ok(mut managed) => {
            paths.append(&mut managed);
            false
        }
        Err(error) => {
            warnings.push(import_problem(&directory.join(MANAGEMENT_DIRECTORY), error.to_string()));
            error.kind() != ErrorKind::InvalidData
        }
    };
    let mut invalid = Vec::new();
    for path in paths {
        match parse_item(&path) {
            Ok(mut item) => {
                let peer = has_manifest_counterpart(&path);
                if managed_unavailable || !matches!(peer, Ok(false)) {
                    block_conflicting_manifest(&mut item);
                    warnings.push(import_problem(&path, "Could not establish a unique recording manifest"));
                }
                items.push(item);
            }
            Err(error) => {
                warnings.push(error.param("path", path.to_string_lossy().into_owned()));
                invalid.push(path);
            }
        }
    }
    for item in &mut items {
        let source = Path::new(&item.source_master_path);
        if item.processing_available && invalid.iter().any(|path| manifest_claims_source(path, source)) {
            block_conflicting_manifest(item);
            warnings.push(import_problem(
                Path::new(&item.manifest_path), "Another manifest claims this Source Master",
            ));
        }
    }
    mark_source_conflicts(&mut items, &mut warnings);
    items.sort_by(|left, right| right.modified_at_unix_ms.cmp(&left.modified_at_unix_ms));
    Ok(RecordingHistoryResult { items, warnings, capture_failures: vec![], processing_run_updates:vec![] })
}

#[cfg(test)]
mod tests {
    use super::{manifest_name, parse_item};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn history_name_removes_only_manifest_suffix() {
        assert_eq!(manifest_name(Path::new("Lecture.capture.json")), "Lecture");
        assert_eq!(
            manifest_name(Path::new("Lecture.mp4.capture.json")),
            "Lecture.mp4"
        );
    }

    #[test]
    fn history_keeps_manifest_when_source_master_is_missing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "capture-scribe-history-{}-{nonce}.capture.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{
              "schemaVersion": 2,
              "sourceTimeline": {"durationSeconds": 12.5},
              "artifacts": [
                {"role": "source_master", "state": "validated", "path": "/missing/source.mp4"},
                {"role": "user_output", "state": "available", "path": "/missing/output.mp4"}
              ],
              "processingRuns": []
            }"#,
        )
        .expect("fixture write");

        let item = parse_item(&path).expect("manifest should remain visible");
        assert_eq!(item.duration_seconds, 12.5);
        assert!(!item.source_master_available);
        assert_eq!(item.user_output_count, 1);

        fs::remove_file(path).expect("fixture cleanup");
    }

    #[test]
    fn history_exposes_every_warning_from_latest_processing_run() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "capture-scribe-history-warnings-{}-{nonce}.capture.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{
              "schemaVersion": 2,
              "sourceTimeline": {"durationSeconds": 8.0},
              "artifacts": [],
              "processingRuns": [
                {"status": "partial", "warnings": [{"raw": "以前の警告"}]},
                {"status": "partial", "warnings": [{"code": "transcription.empty", "raw": "警告1"}, {"raw": "警告2"}]}
              ]
            }"#,
        )
        .expect("fixture write");

        let item = parse_item(&path).expect("manifest should be readable");
        assert_eq!(item.warning_count, 2);
        assert_eq!(
            item.warnings[0].code.as_deref(),
            Some("transcription.empty")
        );
        assert_eq!(item.warnings[0].raw.as_deref(), Some("警告1"));
        assert_eq!(item.warnings[1].raw.as_deref(), Some("警告2"));

        fs::remove_file(path).expect("fixture cleanup");
    }

    #[test]
    fn mixed_old_invalid_and_v2_history_preserves_valid_items_and_old_files() {
        use crate::message::AppMessage;
        use crate::recording_manifest::{append_processing_result, write_initial_manifest};
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("capture-scribe-messages-{nonce}"));
        fs::create_dir(&directory).unwrap();
        let old = directory.join("old.capture.json");
        let invalid = directory.join("invalid.capture.json");
        let old_data = r#"{"schemaVersion":1,"processingRuns":[{"warnings":["old warning"]}]}"#;
        fs::write(&old, old_data).unwrap();
        fs::write(&invalid, "not JSON").unwrap();
        let source = directory.join("source.mp4");
        let report = directory.join("source.run.json");
        fs::write(&source, "fixture").unwrap();
        fs::write(&report, "{}").unwrap();
        let manifest = write_initial_manifest(
            "fixture",
            "test",
            &source,
            &report,
            2.0,
            &source.with_extension("capture.json"),
        )
        .unwrap();
        let warning = AppMessage::known("transcription.empty", "CLIError.emptyTranscript")
            .param("fileName", "source.mp4");
        append_processing_result(&manifest, "partial", &[], &[warning]).unwrap();
        let result = super::scan_directory(directory.to_string_lossy().into_owned()).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.warnings.len(), 2);
        assert_eq!(
            result.items[0].warnings[0].code.as_deref(),
            Some("transcription.empty")
        );
        assert_eq!(
            result.items[0].warnings[0].raw.as_deref(),
            Some("CLIError.emptyTranscript")
        );
        assert_eq!(fs::read_to_string(&old).unwrap(), old_data);
        assert_eq!(result.items[0].warnings[0].params["fileName"], "source.mp4");
        for file in [old, invalid, source, report, manifest] {
            fs::remove_file(file).unwrap();
        }
        // Only this test's unique fixture; persistent manifest lock is expected.
        fs::remove_dir_all(directory).unwrap();
    }
}
