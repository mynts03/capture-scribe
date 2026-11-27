//! A local, rebuildable catalogue. Media and manifests are never written here.
use crate::history::{
    availability, block_conflicting_manifest, has_manifest_counterpart, manifest_claims_source,
    mark_source_conflicts, parse_item, same_source, scan_directory, RecordingHistoryItem,
    RecordingHistoryResult,
};
use crate::message::AppMessage;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};

static SERIAL: AtomicU64 = AtomicU64::new(0);
pub(crate) fn token() -> String {
    format!(
        "{:x}-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    )
}
fn problem(code: &str, detail: impl ToString) -> AppMessage {
    AppMessage::known(code, detail.to_string())
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Index {
    schema_version: u32,
    entries: Vec<RecordingHistoryItem>,
}

fn decode(bytes: &[u8]) -> Result<Index, AppMessage> {
    let index: Index =
        serde_json::from_slice(bytes).map_err(|e| problem("history.index_unavailable", e))?;
    let mut ids = std::collections::HashSet::new();
    if index.schema_version != 1
        || index.entries.iter().any(|e| {
            e.record_id.is_empty()
                || !ids.insert(&e.record_id)
                || !Path::new(&e.manifest_path).is_absolute()
        })
    {
        return Err(problem(
            "history.index_unavailable",
            "Invalid index version, ID or path",
        ));
    }
    Ok(index)
}

// An unknown format is not a damaged current index. In particular, do not
// replace a valid future primary with a readable but older backup.
fn require_known_format(bytes: &[u8]) -> Result<(), AppMessage> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if value.get("schemaVersion").is_some_and(|v| v.as_u64() != Some(1)) {
            return Err(problem("history.index_unavailable", "Unsupported history format; no backup restored or data written"));
        }
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn atomic_bytes(path: &Path, data: &[u8]) -> Result<(), AppMessage> {
    let temp = path.with_extension(format!("{}.tmp", token()));
    let mut file = private_file(&temp).map_err(|e| problem("history.update_failed", e))?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|e| problem("history.update_failed", e))?;
    decode(&fs::read(&temp).map_err(|e| problem("history.update_failed", e))?)?;
    fs::rename(temp, path).map_err(|e| problem("history.update_failed", e))
}

fn load(root: &Path) -> Result<(Index, Option<Vec<u8>>, bool, Vec<AppMessage>), AppMessage> {
    let main = root.join("index.json");
    let backup = root.join("index.previous.json");
    match fs::read(&main) {
        Ok(bytes) => { require_known_format(&bytes)?; match decode(&bytes) {
            Ok(index) => return Ok((index, Some(bytes), false, vec![])),
            Err(_) => (),
        } },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !backup.exists() => {
            return Ok((
                Index {
                    schema_version: 1,
                    entries: vec![],
                },
                None,
                false,
                vec![],
            ))
        }
        Err(_) => (),
    }
    let data = fs::read(&backup).map_err(|e| {
        problem(
            "history.index_unavailable",
            format!("{}; backup: {e}", main.display()),
        )
    })?;
    require_known_format(&data)?;
    let index = decode(&data)?;
    Ok((
        index,
        None,
        true,
        vec![problem("history.backup_used", main.display())],
    ))
}

fn persist(
    root: &Path,
    index: &Index,
    previous: Option<&[u8]>,
    recovered: bool,
) -> Result<(), AppMessage> {
    let bytes =
        serde_json::to_vec_pretty(index).map_err(|e| problem("history.update_failed", e))?;
    decode(&bytes)?;
    // Preserve the damaged primary before replacing it. Never use it as backup.
    if recovered && root.join("index.json").exists() {
        fs::hard_link(
            root.join("index.json"),
            root.join(format!("index.corrupt-{}.json", token())),
        )
        .map_err(|e| problem("history.update_failed", e))?;
    }
    if let Some(good) = previous {
        atomic_bytes(&root.join("index.previous.json"), good)?;
    }
    atomic_bytes(&root.join("index.json"), &bytes)?;
    // Best effort directory metadata sync; no claim of power-loss immunity.
    if let Ok(dir) = fs::File::open(root) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn upsert(index: &mut Index, mut incoming: RecordingHistoryItem, started: Option<u128>) {
    if let Some(old) = index
        .entries
        .iter_mut()
        .find(|e| e.manifest_path == incoming.manifest_path && e.job_id == incoming.job_id)
    {
        if old.source_master_path != incoming.source_master_path {
            old.manifest_state = "identity_changed".into();
            old.snapshot_stale = true;
            old.processing_available = false;
            return;
        }
        incoming.record_id = old.record_id.clone();
        incoming.recorded_at_unix_ms = started
            .or(old.recorded_at_unix_ms)
            .or(incoming.recorded_at_unix_ms);
        *old = incoming;
    } else {
        incoming.record_id = format!("record-{}", token());
        incoming.recorded_at_unix_ms = started.or(incoming.recorded_at_unix_ms);
        index.entries.push(incoming);
    }
}

fn refresh(index: &mut Index, warnings: &mut Vec<AppMessage>) {
    for item in &mut index.entries {
        let path = Path::new(&item.manifest_path);
        match parse_item(path) {
            Ok(mut current)
                if current.job_id == item.job_id
                    && current.source_master_path == item.source_master_path =>
            {
                current.record_id = item.record_id.clone();
                current.recorded_at_unix_ms =
                    item.recorded_at_unix_ms.or(current.recorded_at_unix_ms);
                if !matches!(has_manifest_counterpart(path), Ok(false)) {
                    block_conflicting_manifest(&mut current);
                    warnings.push(
                        problem("history.import_skipped", "Could not establish a unique recording manifest")
                            .param("path", current.manifest_path.clone()),
                    );
                }
                *item = current;
            }
            other => {
                item.manifest_state = if other.is_ok() {
                    "identity_changed".into()
                } else {
                    match availability(&item.manifest_path).as_str() {
                        "available" => "unreadable".into(),
                        state => state.into(),
                    }
                };
                item.snapshot_stale = true;
                item.processing_available = false;
                item.source_master_available = availability(&item.source_master_path) == "available";
                for a in &mut item.artifacts {
                    a.availability = availability(&a.path);
                }
            }
        }
    }
}

// File::lock is OS-backed: all app instances serialize read-modify-write and
// crashes release the lock. The lock file itself is intentionally persistent.
fn with_index<T>(
    root: &Path,
    action: impl FnOnce() -> Result<T, AppMessage>,
) -> Result<T, AppMessage> {
    fs::create_dir_all(root).map_err(|e| problem("history.update_failed", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|e| problem("history.update_failed", e))?;
    }
    let lock_path = root.join("index.lock");
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let lock = opts
        .open(lock_path)
        .map_err(|e| problem("history.update_failed", e))?;
    lock.lock()
        .map_err(|e| problem("history.update_failed", e))?;
    action()
}

pub(crate) fn query(
    root: &Path,
    directory: Option<&str>,
    registration: Option<(&Path, u128)>,
) -> Result<RecordingHistoryResult, AppMessage> {
    with_index(root, || {
        let (mut index, previous, recovered, mut warnings) = load(root)?;
        refresh(&mut index, &mut warnings);
        if let Some(directory) = directory {
            let path = Path::new(directory);
            if !path.is_absolute() {
                return Err(problem(
                    "history.import_skipped",
                    "Choose an absolute folder",
                ));
            }
            match fs::canonicalize(path) {
                Ok(folder) if folder.is_dir() => {
                    match scan_directory(folder.to_string_lossy().into_owned()) {
                        Ok(scanned) => {
                            warnings.extend(scanned.warnings);
                            for item in scanned.items {
                                upsert(&mut index, item, None);
                            }
                        }
                        Err(error) => warnings.push(problem("history.import_skipped", error)),
                    }
                }
                _ => warnings.push(problem("history.import_skipped", path.display())),
            }
        }
        if let Some((path, started)) = registration {
            if !fs::symlink_metadata(path).map_err(|e| problem("history.update_failed", e))?.is_file() {
                return Err(problem("history.update_failed", "Not a regular manifest file"));
            }
            let canonical =
                fs::canonicalize(path).map_err(|e| problem("history.update_failed", e))?;
            upsert(
                &mut index,
                parse_item(&canonical)?,
                (started > 0).then_some(started),
            );
        }
        mark_source_conflicts(&mut index.entries, &mut warnings);
        if let Err(error) = persist(root, &index, previous.as_deref(), recovered) {
            warnings.push(error);
        }
        index.entries.sort_by(|a, b| {
            b.recorded_at_unix_ms
                .cmp(&a.recorded_at_unix_ms)
                .then(a.record_id.cmp(&b.record_id))
        });
        Ok(RecordingHistoryResult {
            items: index.entries,
            warnings,
            capture_failures: vec![],
            processing_run_updates:vec![],
        })
    })
}

pub(crate) fn root(app: &AppHandle) -> Result<PathBuf, AppMessage> {
    app.path()
        .app_data_dir()
        .map(|p| p.join("history"))
        .map_err(|e| problem("history.storage_unavailable", e))
}
// root() fails before any history file is touched. Each caller names what it
// could not do; the original detail (raw/params) is kept for diagnostics.
pub(crate) fn storage_problem(error: AppMessage, code: &str) -> AppMessage {
    AppMessage { code: Some(code.into()), ..error }
}
// Read-only optional identity lookup. A damaged/missing catalogue must not be
// repaired or invent a record ID merely to accept a processing run.
pub(crate) fn record_id_for(root:&Path, manifest:&str, job:&str)->Option<String>{
    let bytes=fs::read(root.join("index.json")).ok()?;
    let index=decode(&bytes).ok()?;
    index.entries.into_iter().find(|e|e.manifest_path==manifest && e.job_id==job).map(|e|e.record_id)
}
// The catalogue adds already-found contenders from other folders. It remains a
// cache, so a stale/missing row is never used in place of a current manifest.
pub(crate) fn require_unambiguous_index_target(
    root: &Path,
    manifest: &Path,
    source: &Path,
) -> Result<(), AppMessage> {
    let index = fs::read(root.join("index.json")).ok().and_then(|bytes| decode(&bytes).ok());
    if let Some(index) = index {
        for entry in index.entries {
            let candidate = Path::new(&entry.manifest_path);
            if candidate != manifest
                && same_source(Path::new(&entry.source_master_path), source)
                && manifest_claims_source(candidate, source)
            {
                return Err(problem("processing.record_unavailable", "Multiple manifests refer to the same Source Master"));
            }
        }
    }
    Ok(())
}

pub fn register(app: &AppHandle, path: &Path, started: u128) -> Vec<AppMessage> {
    // The recording/artifacts already exist here; only the history entry is missing.
    match root(app)
        .map_err(|e| storage_problem(e, "history.update_failed"))
        .and_then(|root| query(&root, None, Some((path, started))))
    {
        Ok(result) => result.warnings,
        Err(error) => vec![error],
    }
}

fn query_with_failures(root: &Path, directory: Option<&str>) -> RecordingHistoryResult {
    // Outbox -> manifest lock is released before taking the index lock.
    let (updates,run_warnings)=crate::processing_runs::reconcile(root);
    let (failures, failure_warnings) = crate::capture_failures::list(root);
    let mut result = query(root, directory, None)
        .unwrap_or_else(|e| RecordingHistoryResult { items: vec![], warnings: vec![e], capture_failures: vec![],processing_run_updates:vec![] });
    result.processing_run_updates=updates;
    result.warnings.extend(run_warnings);
    result.capture_failures = failures;
    result.warnings.extend(failure_warnings);
    result
}
#[tauri::command]
pub async fn list_recording_history(
    app: AppHandle,
    output_directory: Option<String>,
) -> Result<RecordingHistoryResult, AppMessage> {
    let root = root(&app)?;
    let generation = app.state::<crate::history_window::HistoryWindowState>().begin_refresh();
    let result = tauri::async_runtime::spawn_blocking(move || query_with_failures(&root, output_directory.as_deref()))
        .await
        .map_err(|e| problem("history.index_unavailable", e))?;
    let state = app.state::<crate::history_window::HistoryWindowState>();
    if state.install_catalog(generation, result.clone()) { state.publish(&app); }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("capturescribe-history-{}", token()));
            fs::create_dir(&p).unwrap();
            Self(p.canonicalize().unwrap())
        }
        fn index(&self) -> PathBuf {
            self.0.join("catalogue")
        }
        fn recording(&self, folder: &str, name: &str, job: &str, date: Option<&str>) -> PathBuf {
            let directory = self.0.join(folder);
            fs::create_dir_all(&directory).unwrap();
            let source = directory.join(format!("{name}.mp4"));
            let report = source.with_extension("run.json");
            fs::write(&source, b"original recording fixture").unwrap();
            fs::write(&report, serde_json::json!({"startedAt": date}).to_string()).unwrap();
            let manifest = source.with_extension("capture.json");
            crate::recording_manifest::write_initial_manifest(
                job, "fixture", &source, &report, 20.0, &manifest,
            )
            .unwrap();
            manifest
        }
        fn managed_recording(&self, folder: &str, name: &str) -> PathBuf {
            let directory = self.0.join(folder);
            let managed = directory.join(crate::save_destination::MANAGEMENT_DIRECTORY);
            fs::create_dir_all(&managed).unwrap();
            let source = directory.join(format!("{name}.mp4"));
            let report = managed.join(format!("{name}.run.json"));
            let manifest = managed.join(format!("{name}.capture.json"));
            fs::write(&source, b"original recording fixture").unwrap();
            fs::write(&report, br#"{"startedAt":"2026-11-27T01:00:00Z"}"#).unwrap();
            crate::recording_manifest::write_initial_manifest(
                &format!("managed-{name}"), "fixture", &source, &report, 20.0, &manifest,
            )
            .unwrap();
            let identity = crate::recording_manifest::file_identity(&source).unwrap();
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
            for artifact in value["artifacts"].as_array_mut().unwrap().iter_mut().take(2) {
                artifact["physicalIdentity"] = identity.clone().into();
            }
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
            manifest
        }
        fn import(&self, folder: &str) -> RecordingHistoryResult {
            query(
                &self.index(),
                Some(self.0.join(folder).to_str().unwrap()),
                None,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unknown_primary_version_is_not_overwritten_with_old_backup() {
        let f=Fixture::new();f.recording("a","Keep","job",None);f.import("a");f.import("a");
        let backup=fs::read(f.index().join("index.previous.json")).unwrap();
        let future=br#"{"schemaVersion":99,"entries":[],"futureData":"retain"}"#;
        fs::write(f.index().join("index.json"),future).unwrap();
        assert!(query(&f.index(),None,None).is_err());
        assert_eq!(fs::read(f.index().join("index.json")).unwrap(),future);
        assert_eq!(fs::read(f.index().join("index.previous.json")).unwrap(),backup);
    }

    #[test]
    fn successful_catalogue_and_independent_failures_survive_reopening_and_index_errors() {
        let f=Fixture::new();let manifest=f.recording("a","Keep","job",None);f.import("a");
        let before=fs::read(&manifest).unwrap();
        let failure=crate::capture_failures::CaptureSaveFailure::new("attempt-1".into(),"failed-job".into(),"Failed".into(),2,
            AppMessage::known("capture.validation_failed","injected"),&f.0.join("planned.mp4"),&f.0.join("working.mp4"));
        crate::capture_failures::persist(&f.index(),&failure).unwrap();
        let reopened=query_with_failures(&f.index(),None);assert_eq!(reopened.items.len(),1);assert_eq!(reopened.capture_failures.len(),1);assert_eq!(fs::read(&manifest).unwrap(),before);
        for file in ["index.json","index.previous.json"] {fs::write(f.index().join(file),b"broken").unwrap();}
        let broken=query_with_failures(&f.index(),None);assert!(broken.items.is_empty());assert_eq!(broken.capture_failures.len(),1);assert!(!broken.warnings.is_empty());
        assert_eq!(fs::read(f.index().join("index.json")).unwrap(),b"broken");
    }

    #[test]
    fn global_history_survives_restart_deduplicates_and_sorts_capture_dates() {
        let f = Fixture::new();
        let first = f.recording("a", "Lecture", "same-job", Some("2026-10-17T01:00:00Z"));
        let before = fs::read(&first).unwrap();
        f.recording("b", "Lecture (2)", "same-job", Some("2026-10-16T01:00:00Z"));
        let id = f.import("a").items[0].record_id.clone();
        let all = f.import("b");
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.items[0].record_id, id);
        assert_eq!(f.import("a").items.len(), 2);
        let restarted = query(&f.index(), None, None).unwrap();
        assert_eq!(restarted.items[0].record_id, id);
        assert_ne!(restarted.items[0].record_id, restarted.items[1].record_id);
        assert_eq!(fs::read(first).unwrap(), before);
    }
    #[test]
    fn unknown_date_is_not_manifest_mtime_and_new_capture_has_explicit_time() {
        let f = Fixture::new();
        let m = f.recording("a", "Unknown", "1780000000000", None);
        assert_eq!(f.import("a").items[0].recorded_at_unix_ms, None);
        let result = query(&f.index(), None, Some((&m, 123456789))).unwrap();
        assert_eq!(result.items[0].recorded_at_unix_ms, Some(123456789));
        assert_eq!(f.import("a").items[0].recorded_at_unix_ms, Some(123456789));
    }
    #[test]
    fn missing_source_and_manifest_preserve_snapshot_and_other_outputs() {
        let f = Fixture::new();
        let m = f.recording("a", "Keep", "job", None);
        let output = f.0.join("a/Keep_processed.mp4");
        fs::write(&output, b"derived").unwrap();
        crate::recording_manifest::append_processing_result(
            &m,
            "completed",
            &[crate::recording_manifest::DerivedArtifact {
                role: "user_output",
                path: output.to_str().unwrap(),
                state: "available",
                source_timeline_offset_seconds: 0.0,
            }],
            &[],
        )
        .unwrap();
        let initial = f.import("a");
        let id = &initial.items[0].record_id;
        fs::remove_file(m.with_file_name("Keep.mp4")).unwrap();
        let missing = query(&f.index(), None, None).unwrap();
        assert_eq!(&missing.items[0].record_id, id);
        assert!(!missing.items[0].processing_available);
        assert!(missing.items[0]
            .artifacts
            .iter()
            .any(|a| a.path == output.to_str().unwrap() && a.availability == "available"));
        fs::remove_file(m).unwrap();
        let stale = query(&f.index(), None, None).unwrap();
        assert!(stale.items[0].snapshot_stale);
        assert_eq!(stale.items[0].manifest_state, "missing");
        assert_eq!(
            stale.items[0].processing_status.as_deref(),
            Some("completed")
        );
        assert!(output.is_file());
    }
    #[test]
    fn backup_restores_and_preserves_corrupt_primary() {
        let f = Fixture::new();
        f.recording("a", "Keep", "job", None);
        f.import("a");
        f.import("a");
        fs::write(f.index().join("index.json"), b"broken primary").unwrap();
        let result = query(&f.index(), None, None).unwrap();
        assert_eq!(result.items.len(), 1);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.code.as_deref() == Some("history.backup_used")));
        assert!(fs::read_dir(f.index())
            .unwrap()
            .any(|e| fs::read(e.unwrap().path()).ok().as_deref() == Some(b"broken primary")));
        assert_eq!(query(&f.index(), None, None).unwrap().items.len(), 1);
    }
    #[test]
    fn two_invalid_indexes_are_not_replaced_by_empty_history() {
        let f = Fixture::new();
        fs::create_dir(f.index()).unwrap();
        for name in ["index.json", "index.previous.json"] {
            fs::write(f.index().join(name), b"broken").unwrap();
        }
        assert!(query(&f.index(), None, None).is_err());
        assert_eq!(fs::read(f.index().join("index.json")).unwrap(), b"broken");
        assert_eq!(
            fs::read(f.index().join("index.previous.json")).unwrap(),
            b"broken"
        );
    }
    #[test]
    fn storage_problem_renames_only_the_code() {
        let original = problem("history.storage_unavailable", "app data unavailable").param("detail", 3);
        for code in ["history.update_failed", "processing.storage_unavailable", "capture.failure_record_unavailable"] {
            let mapped = storage_problem(original.clone(), code);
            assert_eq!(mapped.code.as_deref(), Some(code));
            assert_eq!(mapped.raw, original.raw);
            assert_eq!(mapped.params, original.params);
        }
    }
    #[test]
    fn failed_update_keeps_recording_and_last_good_primary() {
        let f = Fixture::new();
        let m = f.recording("a", "Keep", "job", None);
        f.import("a");
        let original = fs::read(&m).unwrap();
        let good = fs::read(f.index().join("index.json")).unwrap();
        fs::create_dir(f.index().join("index.previous.json")).unwrap();
        let result = query(&f.index(), None, Some((&m, 42))).unwrap();
        assert!(result
            .warnings
            .iter()
            .any(|w| w.code.as_deref() == Some("history.update_failed")));
        assert_eq!(fs::read(f.index().join("index.json")).unwrap(), good);
        assert_eq!(fs::read(m).unwrap(), original);
    }
    #[test]
    fn only_direct_regular_v2_manifests_are_imported() {
        let f = Fixture::new();
        f.recording("a", "Keep", "job", None);
        f.recording("a/child", "Nested", "nested", None);
        let old = f.0.join("a/old.capture.json");
        fs::write(&old, br#"{"schemaVersion":1}"#).unwrap();
        fs::write(f.0.join("a/broken.capture.json"), b"broken").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            f.0.join("a/child/Nested.capture.json"),
            f.0.join("a/link.capture.json"),
        )
        .unwrap();
        let result = f.import("a");
        assert_eq!(result.items.len(), 1);
        assert!(result.warnings.len() >= 2);
        assert_eq!(fs::read(old).unwrap(), br#"{"schemaVersion":1}"#);
    }
    #[test]
    fn flat_and_managed_layouts_coexist_through_catalog_reopen() {
        let f = Fixture::new();
        let legacy = f.recording("recordings", "Legacy", "older", None);
        let managed = f.managed_recording("recordings", "New");
        let before = [fs::read(&legacy).unwrap(), fs::read(&managed).unwrap()];

        let first = f.import("recordings");
        assert_eq!(first.items.len(), 2);
        assert!(first.warnings.is_empty());
        for row in &first.items {
            assert!(row.processing_available);
            let manifest = Path::new(&row.manifest_path);
            let source = Path::new(&row.source_master_path);
            let identity = &row.artifacts.iter().find(|a| a.role == "source_master").unwrap().physical_identity;
            assert_eq!(source.parent(), Some(f.0.join("recordings").as_path()));
            crate::history::require_unambiguous_manifest(manifest, source, identity).unwrap();
        }
        let reopened = query(&f.index(), None, None).unwrap();
        assert_eq!(reopened.items.len(), 2);
        assert_eq!(reopened.items[0].record_id, first.items[0].record_id);
        assert_eq!(reopened.items[1].record_id, first.items[1].record_id);
        assert_eq!(fs::read(&legacy).unwrap(), before[0]);
        assert_eq!(fs::read(&managed).unwrap(), before[1]);
    }

    #[test]
    fn same_name_manifest_cannot_override_an_indexed_entry() {
        let f = Fixture::new();
        let managed = f.managed_recording("recordings", "New");
        let first = f.import("recordings");
        let row = &first.items[0];
        let id = row.record_id.clone();
        let source = Path::new(&row.source_master_path);
        let identity = crate::recording_manifest::file_identity(source).unwrap();
        let before = fs::read(&managed).unwrap();

        let legacy = f.0.join("recordings/New.capture.json");
        fs::write(&legacy, b"malformed competing file").unwrap();
        let reopened = query(&f.index(), None, None).unwrap();
        assert_eq!(reopened.items[0].record_id, id);
        assert!(!reopened.items[0].processing_available);
        assert_eq!(reopened.items[0].manifest_state, "conflicting");
        assert!(crate::history::require_unambiguous_manifest(&managed, source, &identity).is_err());
        assert_eq!(fs::read(&managed).unwrap(), before);
        assert_eq!(fs::read(&legacy).unwrap(), b"malformed competing file");
    }

    #[test]
    fn different_manifests_for_the_same_hard_linked_media_are_blocked() {
        let f = Fixture::new();
        let original = f.managed_recording("recordings", "New");
        let source = f.0.join("recordings/New.mp4");
        let identity = crate::recording_manifest::file_identity(&source).unwrap();
        let alias = f.0.join("recordings/Other.mp4");
        let competing = alias.with_extension("capture.json");
        fs::hard_link(&source, &alias).unwrap();
        let report = alias.with_extension("run.json");
        fs::write(&report, "{}").unwrap();
        crate::recording_manifest::write_initial_manifest(
            "other", "fixture", &alias, &report, 20.0, &competing,
        )
        .unwrap();

        let result = f.import("recordings");
        assert_eq!(result.items.len(), 2);
        assert!(result.items.iter().all(|item| {
            !item.processing_available && item.manifest_state == "conflicting"
        }));
        assert!(crate::history::require_unambiguous_manifest(&original, &source, &identity).is_err());
        assert!(require_unambiguous_index_target(&f.index(), &original, &source).is_err());
        assert_eq!(fs::read(&alias).unwrap(), b"original recording fixture");
        assert_eq!(fs::read(&source).unwrap(), b"original recording fixture");
    }

    #[test]
    fn managed_manifest_does_not_adopt_replacement_media_at_its_original_path() {
        let f = Fixture::new();
        let manifest = f.managed_recording("recordings", "New");
        let source = f.0.join("recordings/New.mp4");
        let old = source.with_file_name("Moved.mp4");
        let identity = crate::recording_manifest::file_identity(&source).unwrap();
        let id = f.import("recordings").items[0].record_id.clone();
        fs::rename(&source, &old).unwrap();
        fs::write(&source, b"replacement").unwrap();

        let reopened = query(&f.index(), None, None).unwrap();
        assert_eq!(reopened.items[0].record_id, id);
        assert!(!reopened.items[0].source_master_available);
        assert!(!reopened.items[0].processing_available);
        assert!(crate::history::require_unambiguous_manifest(&manifest, &source, &identity).is_err());
        assert_eq!(fs::read(&old).unwrap(), b"original recording fixture");
        assert_eq!(fs::read(&source).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn aliased_management_folder_is_not_imported() {
        let f = Fixture::new();
        f.recording("recordings", "Legacy", "older", None);
        let foreign = f.managed_recording("outside", "New");
        let managed = f.0.join("recordings").join(crate::save_destination::MANAGEMENT_DIRECTORY);
        std::os::unix::fs::symlink(foreign.parent().unwrap(), &managed).unwrap();

        let result = f.import("recordings");
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].name, "Legacy");
        assert!(result.items[0].processing_available);
        assert!(!result.warnings.is_empty());
        assert!(parse_item(&managed.join("New.capture.json")).is_err());
    }

    #[test]
    fn concurrent_imports_do_not_lose_entries() {
        let f = Fixture::new();
        for n in 0..4 {
            f.recording(&format!("a{n}"), "Keep", "same-job", None);
        }
        let threads: Vec<_> = (0..4)
            .map(|n| {
                let root = f.index();
                let folder = f.0.join(format!("a{n}"));
                std::thread::spawn(move || {
                    query(&root, Some(folder.to_str().unwrap()), None).unwrap()
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(query(&f.index(), None, None).unwrap().items.len(), 4);
    }
    #[test]
    fn processing_refreshes_without_duplicate_or_media_modification() {
        let f = Fixture::new();
        let m = f.recording("a", "Keep", "job", None);
        let initial = f.import("a");
        let warning = AppMessage::known("transcription.empty", "fixture raw");
        crate::recording_manifest::append_processing_result(&m, "partial", &[], &[warning])
            .unwrap();
        let result = query(&f.index(), None, Some((&m, 0))).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].record_id, initial.items[0].record_id);
        assert_eq!(
            result.items[0].processing_status.as_deref(),
            Some("partial")
        );
        assert_eq!(result.items[0].warnings.len(), 1);
        assert_eq!(
            fs::read(m.with_file_name("Keep.mp4")).unwrap(),
            b"original recording fixture"
        );
    }
    #[test]
    fn catalogue_has_no_hundred_entry_storage_limit() {
        let f = Fixture::new();
        let m = f.recording("a", "Keep", "job", None);
        let item = parse_item(&m).unwrap();
        let mut index = Index {
            schema_version: 1,
            entries: vec![],
        };
        for n in 0..105 {
            let mut copy = item.clone();
            copy.manifest_path =
                f.0.join(format!("missing-{n}.capture.json"))
                    .to_string_lossy()
                    .into();
            upsert(&mut index, copy, None);
        }
        fs::create_dir(f.index()).unwrap();
        persist(&f.index(), &index, None, false).unwrap();
        assert_eq!(query(&f.index(), None, None).unwrap().items.len(), 105);
    }
}
