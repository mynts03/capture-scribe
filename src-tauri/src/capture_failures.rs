//! Immutable capture-save failures, separate from successful manifests/index.
use crate::history_index::{private_file, token};
use crate::message::AppMessage;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT: &str = "capturescribe.capture-save-failure";
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn error(detail: impl ToString) -> AppMessage {
    AppMessage::known("capture.failure_record_unavailable", detail.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileObservation {
    pub path: String,
    pub state: String,
    pub checked_at_unix_ms: u128,
}
impl FileObservation {
    pub fn inspect(path: &Path) -> Self {
        let state = match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => "present",
            Ok(_) => "unavailable",
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
            Err(_) => "unknown",
        };
        Self {
            path: path.to_string_lossy().into_owned(),
            state: state.into(),
            checked_at_unix_ms: now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureSaveFailure {
    pub format: String,
    pub schema_version: u32,
    pub attempt_id: String,
    pub job_id: String,
    pub recording_name: String,
    pub recorded_at_unix_ms: u128,
    pub failed_at_unix_ms: u128,
    pub outcome: String,
    pub error: AppMessage,
    // Presence at a planned name is not proof of ownership or a validated MP4.
    pub planned_final_file: FileObservation,
    pub working_file: FileObservation,
}
impl CaptureSaveFailure {
    pub fn new(
        attempt_id: String,
        job_id: String,
        recording_name: String,
        recorded: u128,
        message: AppMessage,
        planned: &Path,
        working: &Path,
    ) -> Self {
        Self {
            format: FORMAT.into(),
            schema_version: 1,
            attempt_id,
            job_id,
            recording_name,
            recorded_at_unix_ms: recorded,
            failed_at_unix_ms: now(),
            outcome: "save_failed".into(),
            error: message,
            planned_final_file: FileObservation::inspect(planned),
            working_file: FileObservation::inspect(working),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureFailureView {
    pub record: CaptureSaveFailure,
    pub persisted: bool,
    pub persistence_error: Option<AppMessage>,
    // Recheck on listing; never replace the original stored observations.
    pub planned_final_file: FileObservation,
    pub working_file: FileObservation,
}
impl CaptureFailureView {
    pub fn new(record: CaptureSaveFailure, persistence_error: Option<AppMessage>) -> Self {
        Self {
            planned_final_file: FileObservation::inspect(Path::new(
                &record.planned_final_file.path,
            )),
            working_file: FileObservation::inspect(Path::new(&record.working_file.path)),
            record,
            persisted: persistence_error.is_none(),
            persistence_error,
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 160 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
fn decode(bytes: &[u8]) -> Result<CaptureSaveFailure, AppMessage> {
    let header: serde_json::Value = serde_json::from_slice(bytes).map_err(error)?;
    if header["format"] != FORMAT || header["schemaVersion"] != 1 {
        return Err(error(
            "Unsupported capture failure format/version; original retained",
        ));
    }
    let record: CaptureSaveFailure = serde_json::from_value(header).map_err(error)?;
    if !valid_id(&record.attempt_id)
        || record.job_id.is_empty()
        || record.outcome != "save_failed"
        || [&record.working_file, &record.planned_final_file]
            .iter()
            .any(|f| {
                !Path::new(&f.path).is_absolute()
                    || !matches!(
                        f.state.as_str(),
                        "present" | "missing" | "unknown" | "unavailable"
                    )
            })
    {
        return Err(error("Invalid capture failure ID, outcome or observation"));
    }
    Ok(record)
}
fn regular_bytes(path: &Path) -> Result<Vec<u8>, AppMessage> {
    if !fs::symlink_metadata(path).map_err(error)?.is_file() {
        return Err(error("Not a regular failure record"));
    }
    fs::read(path).map_err(error)
}
fn directory(root: &Path) -> PathBuf {
    root.join("capture-failures")
}

/// An identical retry is idempotent. Never replace a different/unknown record.
pub(crate) fn persist(root: &Path, record: &CaptureSaveFailure) -> Result<(), AppMessage> {
    persist_with(root, record, || Ok(()))
}
fn persist_with(
    root: &Path,
    record: &CaptureSaveFailure,
    before_publish: impl FnOnce() -> Result<(), AppMessage>,
) -> Result<(), AppMessage> {
    let bytes = serde_json::to_vec_pretty(record).map_err(error)?;
    decode(&bytes)?;
    let dir = directory(root);
    fs::create_dir_all(&dir).map_err(error)?;
    if !fs::symlink_metadata(&dir).map_err(error)?.is_dir() {
        return Err(error("Failure store is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(error)?;
    }
    let destination = dir.join(format!("{}.json", record.attempt_id));
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return if decode(&regular_bytes(&destination)?)? == *record {
                Ok(())
            } else {
                Err(error(
                    "Capture attempt ID collision; existing record retained",
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(error(e)),
    }
    let temporary = dir.join(format!(".{}.tmp", token()));
    let result = (|| {
        let mut file = private_file(&temporary).map_err(error)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(error)?;
        decode(&regular_bytes(&temporary)?)?;
        before_publish()?;
        match fs::hard_link(&temporary, &destination) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if decode(&regular_bytes(&destination)?)? != *record {
                    return Err(error("Concurrent attempt ID collision"));
                }
            }
            Err(e) => return Err(error(e)),
        }
        if let Ok(directory) = fs::File::open(&dir) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    // Only this call's unique temporary alias, never a user artifact.
    let _ = fs::remove_file(&temporary);
    result
}

pub(crate) fn list(root: &Path) -> (Vec<CaptureFailureView>, Vec<AppMessage>) {
    let entries = match fs::read_dir(directory(root)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (vec![], vec![]),
        Err(e) => return (vec![], vec![error(e)]),
    };
    let mut records = vec![];
    let mut warnings = vec![];
    for entry in entries {
        let read = || -> Result<Option<CaptureSaveFailure>, AppMessage> {
            let path = entry.as_ref().map_err(error)?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                return Ok(None);
            }
            let record = decode(&regular_bytes(&path)?)?;
            if path.file_stem().and_then(|s| s.to_str()) != Some(record.attempt_id.as_str()) {
                return Err(error("Failure filename/attempt mismatch"));
            }
            Ok(Some(record))
        };
        match read() {
            Ok(Some(record)) => records.push(CaptureFailureView::new(record, None)),
            Ok(None) => (),
            Err(e) => warnings.push(e),
        }
    }
    records.sort_by(|a, b| {
        b.record
            .recorded_at_unix_ms
            .cmp(&a.record.recorded_at_unix_ms)
            .then(a.record.attempt_id.cmp(&b.record.attempt_id))
    });
    (records, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("capturescribe-failure-test-{}", token()));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn record(&self) -> CaptureSaveFailure {
            CaptureSaveFailure::new(
                "attempt-1".into(),
                "job-1".into(),
                "Test".into(),
                123,
                AppMessage::known("capture.report_invalid", "specific cause").param("expected", 1),
                &self.0.join("final.mp4"),
                &self.0.join("working.mp4"),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn roundtrip_restart_duplicate_and_originals_are_preserved() {
        let f = Fixture::new();
        fs::write(f.0.join("working.mp4"), b"source").unwrap();
        for name in [
            "index.json",
            "index.previous.json",
            "source.capture.json",
            "settings.json",
            "old.txt",
        ] {
            fs::write(f.0.join(name), name).unwrap();
        }
        let record = f.record();
        persist(&f.0, &record).unwrap();
        persist(&f.0, &record).unwrap();
        let (records, warnings) = list(&f.0);
        assert!(warnings.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record, record);
        assert!(records[0].persisted);
        assert_eq!(records[0].working_file.state, "present");
        assert_eq!(records[0].planned_final_file.state, "missing");
        for name in [
            "index.json",
            "index.previous.json",
            "source.capture.json",
            "settings.json",
            "old.txt",
        ] {
            assert_eq!(fs::read_to_string(f.0.join(name)).unwrap(), name);
        }
        assert_eq!(fs::read(f.0.join("working.mp4")).unwrap(), b"source");
        fs::remove_file(f.0.join("working.mp4")).unwrap();
        let (again, _) = list(&f.0);
        assert_eq!(again[0].working_file.state, "missing");
        assert_eq!(again[0].record.working_file.state, "present");
    }
    #[test]
    fn write_failure_and_collision_do_not_destroy_existing_record() {
        let f = Fixture::new();
        let record = f.record();
        persist(&f.0, &record).unwrap();
        let original = fs::read(directory(&f.0).join("attempt-1.json")).unwrap();
        let mut changed = record.clone();
        changed.error.raw = Some("different".into());
        assert!(persist(&f.0, &changed).is_err());
        assert_eq!(
            fs::read(directory(&f.0).join("attempt-1.json")).unwrap(),
            original
        );
        changed.attempt_id = "attempt-2".into();
        assert!(persist_with(&f.0, &changed, || Err(error("injected write failure"))).is_err());
        assert!(!directory(&f.0).join("attempt-2.json").exists());
        assert_eq!(list(&f.0).0.len(), 1);
    }
    #[test]
    fn unknown_versions_and_corruption_are_reported_without_rewriting_or_fallback() {
        let f = Fixture::new();
        let record = f.record();
        persist(&f.0, &record).unwrap();
        let path = directory(&f.0).join("attempt-1.json");
        let mut future = serde_json::to_value(&record).unwrap();
        future["schemaVersion"] = 99.into();
        let bytes = future.to_string();
        fs::write(&path, &bytes).unwrap();
        assert!(persist(&f.0, &record).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        assert_eq!(list(&f.0).0.len(), 0);
        assert_eq!(list(&f.0).1.len(), 1);
        fs::write(directory(&f.0).join("broken.json"), b"broken").unwrap();
        let mut next = record;
        next.attempt_id = "attempt-2".into();
        persist(&f.0, &next).unwrap();
        let (items, warnings) = list(&f.0);
        assert_eq!(items.len(), 1);
        assert_eq!(warnings.len(), 2);
    }
    #[test]
    fn unavailable_files_and_persistence_errors_are_not_save_success() {
        let f = Fixture::new();
        let record = f.record();
        fs::create_dir(f.0.join("working.mp4")).unwrap();
        let notice = CaptureFailureView::new(record, Some(error("disk write failed")));
        assert!(!notice.persisted);
        assert_eq!(notice.working_file.state, "unavailable");
        assert_eq!(notice.planned_final_file.state, "missing");
        assert!(notice.persistence_error.is_some());
    }
    #[test]
    fn invalid_identity_and_unknown_fields_refuse_writes() {
        let f = Fixture::new();
        let mut record = f.record();
        record.attempt_id = "../unsafe".into();
        assert!(persist(&f.0, &record).is_err());
        let mut v = serde_json::to_value(f.record()).unwrap();
        v["futureInformation"] = true.into();
        assert!(decode(v.to_string().as_bytes()).is_err());
    }
    #[test]
    fn concurrent_identical_publications_are_idempotent() {
        let f = Fixture::new();
        let record = f.record();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let root = f.0.clone();
                let record = record.clone();
                std::thread::spawn(move || persist(&root, &record))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(list(&f.0).0.len(), 1);
    }
}
