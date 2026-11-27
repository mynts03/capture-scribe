//! A small immutable write-ahead outbox for one-recording processing runs.
//! This is not a queue/manager: it never starts, retries or stops a worker.
use crate::{
    message::AppMessage,
    recording_manifest::{self as manifest, ProcessingRun, ProcessingSettings, PublishedArtifact},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT: &str = "capturescribe.processing-run";
pub(crate) fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
pub(crate) fn problem(detail: impl ToString) -> AppMessage {
    AppMessage::known("processing.record_unavailable", detail.to_string())
}
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 160 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestIdentity {
    pub manifest_path: String,
    pub job_id: String,
    pub source_master_path: String,
    pub source_physical_identity: String,
}
impl ManifestIdentity {
    pub fn read(path: &Path) -> Result<Self, AppMessage> {
        if !fs::symlink_metadata(path).map_err(problem)?.is_file() {
            return Err(problem("Manifest must be a regular file"));
        }
        if let Some(parent) = path.parent().filter(|p| p.ends_with(crate::save_destination::MANAGEMENT_DIRECTORY)) {
            if !fs::symlink_metadata(parent).map_err(problem)?.is_dir() {
                return Err(problem("Management folder must be a regular directory"));
            }
        }
        let path = fs::canonicalize(path).map_err(problem)?;
        let v = manifest::read_supported(&path)?;
        let identity = Self::from_value(&path, &v)?;
        identity.require_current_target()?;
        Ok(identity)
    }
    fn from_value(path: &Path, v: &Value) -> Result<Self, AppMessage> {
        let artifacts = v["artifacts"].as_array().ok_or("Missing artifacts")?;
        let mut sources = artifacts.iter().filter(|a| a["role"] == "source_master");
        let source = sources.next().ok_or("Missing validated Source Master")?;
        if source["state"] != "validated" || sources.next().is_some() {
            return Err(problem("Source Master must be a single validated artifact"));
        }
        let s = Self {
            manifest_path: path.to_string_lossy().into_owned(),
            job_id: v["jobId"].as_str().unwrap_or("").into(),
            source_master_path: source["path"].as_str().unwrap_or("").into(),
            source_physical_identity: source["physicalIdentity"].as_str().unwrap_or("").into(),
        };
        if s.job_id.is_empty()
            || !Path::new(&s.source_master_path).is_absolute()
            || s.source_physical_identity.is_empty()
        {
            return Err(problem("Missing recording identity"));
        }
        Ok(s)
    }
    fn require_current_target(&self) -> Result<(), AppMessage> {
        crate::history::require_unambiguous_manifest(
            Path::new(&self.manifest_path),
            Path::new(&self.source_master_path),
            &self.source_physical_identity,
        )
    }
    fn matches(&self, v: &Value) -> Result<(), AppMessage> {
        if &Self::from_value(Path::new(&self.manifest_path), v)? != self {
            return Err(problem("Recording identity changed; no run applied"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    format: String,
    schema_version: u32,
    identity: ManifestIdentity,
    run: ProcessingRun,
}
fn decode(bytes: &[u8]) -> Result<Snapshot, AppMessage> {
    let v: Value = serde_json::from_slice(bytes).map_err(problem)?;
    if v["format"] != FORMAT || v["schemaVersion"] != 1 {
        return Err(problem("Unsupported processing outbox; original retained"));
    }
    let s: Snapshot = serde_json::from_value(v).map_err(problem)?;
    if s.run.run_schema_version != Some(1)
        || !s.run.run_id.as_deref().is_some_and(valid_id)
        || s.run.job_id.as_deref() != Some(&s.identity.job_id)
        || s.run.manifest_path.as_deref() != Some(&s.identity.manifest_path)
        || !Path::new(&s.identity.manifest_path).is_absolute()
        || !Path::new(&s.identity.source_master_path).is_absolute()
        || !matches!(
            s.run.status.as_str(),
            "accepted"
                | "running"
                | "completed"
                | "partial"
                | "failed"
                | "cancelled"
                | "not-executed"
                | "interrupted"
                | "unknown"
        )
    {
        return Err(problem("Invalid outbox identity/status"));
    }
    Ok(s)
}
fn read(path: &Path) -> Result<Snapshot, AppMessage> {
    if !fs::symlink_metadata(path).map_err(problem)?.is_file() {
        return Err(problem("Not a regular outbox snapshot"));
    }
    decode(&fs::read(path).map_err(problem)?)
}
// Exclusive immutable phase publication; retries with the same bytes are safe.
fn publish(path: &Path, snapshot: &Snapshot) -> Result<(), AppMessage> {
    let bytes = serde_json::to_vec_pretty(snapshot).map_err(problem)?;
    decode(&bytes)?;
    if fs::symlink_metadata(path).is_ok() {
        return if read(path)? == *snapshot {
            Ok(())
        } else {
            Err(problem("Conflicting immutable run snapshot"))
        };
    }
    let temp = path.with_extension(format!("{}.tmp", crate::history_index::token()));
    let result = (|| {
        let mut file = crate::history_index::private_file(&temp).map_err(problem)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(problem)?;
        read(&temp)?;
        match fs::hard_link(&temp, path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && read(path)? == *snapshot => {
                Ok(())
            }
            Err(e) => Err(problem(e)),
        }
    })();
    let _ = fs::remove_file(&temp); // Only our own unique unpublished alias.
    if let Ok(dir) = fs::File::open(path.parent().unwrap()) {
        let _ = dir.sync_all();
    }
    result
}
fn lock_file(dir: &Path) -> Result<fs::File, AppMessage> {
    let path = dir.join("live.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return Err(problem("Invalid run lock"));
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(problem)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub run: ProcessingRun,
    pub manifest_path: String,
    pub manifest_applied: bool,
    pub outbox_saved: bool,
    pub active: bool,
    pub record_error: Option<AppMessage>,
    pub artifacts: Vec<RunArtifact>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunArtifact {
    pub path: String,
    pub role: String,
    pub availability: String,
}
pub fn observed_artifacts(run: &ProcessingRun) -> Vec<RunArtifact> {
    run.published_artifacts
        .iter()
        .map(|a| {
            let availability = match fs::symlink_metadata(&a.path) {
                Ok(m) if m.is_file() => match manifest::file_identity(Path::new(&a.path)) {
                    Ok(id) if id == a.file_identity => "available",
                    Ok(_) => "unavailable",
                    Err(_) => "unknown",
                },
                Ok(_) => "unavailable",
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
                Err(_) => "unknown",
            }
            .into();
            RunArtifact {
                path: a.path.clone(),
                role: a.role.clone(),
                availability,
            }
        })
        .collect()
}
pub struct Execution {
    snapshot: Snapshot,
    pub directory: PathBuf,
    _live: fs::File,
}
impl Execution {
    pub fn accept(
        root: &Path,
        identity: ManifestIdentity,
        record_id: Option<String>,
        settings: ProcessingSettings,
    ) -> Result<Self, AppMessage> {
        let id = format!("run-{}", crate::history_index::token());
        let dir = root.join("processing-outbox").join(&id);
        fs::create_dir_all(dir.parent().unwrap()).map_err(problem)?;
        if !fs::symlink_metadata(dir.parent().unwrap())
            .map_err(problem)?
            .is_dir()
        {
            return Err(problem("Invalid outbox directory"));
        }
        fs::create_dir(&dir).map_err(problem)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [dir.parent().unwrap(), dir.as_path()] {
                fs::set_permissions(p, fs::Permissions::from_mode(0o700)).map_err(problem)?;
            }
        }
        let live = lock_file(&dir)?;
        live.lock().map_err(problem)?;
        let run = ProcessingRun {
            run_schema_version: Some(1),
            run_id: Some(id),
            record_id,
            job_id: Some(identity.job_id.clone()),
            manifest_path: Some(identity.manifest_path.clone()),
            accepted_at_unix_ms: Some(now()),
            settings: Some(settings),
            status: "accepted".into(),
            ..Default::default()
        };
        let snapshot = Snapshot {
            format: FORMAT.into(),
            schema_version: 1,
            identity,
            run,
        };
        publish(&dir.join("accepted.json"), &snapshot)?;
        Ok(Self {
            snapshot,
            directory: dir,
            _live: live,
        })
    }
    pub fn run(&self) -> &ProcessingRun {
        &self.snapshot.run
    }
    pub fn identity(&self) -> &ManifestIdentity {
        &self.snapshot.identity
    }
    pub fn start(&mut self) -> Result<(), AppMessage> {
        if ManifestIdentity::read(Path::new(&self.snapshot.identity.manifest_path))?
            != self.snapshot.identity
        {
            return Err(problem("Recording changed before launch"));
        }
        // This timestamp means execution/launch attempt, not successful child
        // startup. Persist before spawn so no unrecorded worker is launched.
        let mut next = self.snapshot.clone();
        next.run.status = "running".into();
        next.run.started_at_unix_ms = Some(now());
        publish(&self.directory.join("started.json"), &next)?;
        self.snapshot = next;
        Ok(())
    }
    pub fn finish(&mut self, mut run: ProcessingRun) -> RunView {
        run.finished_at_unix_ms = if matches!(run.status.as_str(), "unknown" | "interrupted") {
            None
        } else {
            Some(now())
        };
        let mut next = self.snapshot.clone();
        next.run = run;
        let saved = publish(&self.directory.join("terminal.json"), &next);
        let applied = apply(&next);
        let record_error = match (saved.as_ref().err(), applied.as_ref().err()) {
            (Some(a), Some(b)) => Some(problem(
                serde_json::to_string(&serde_json::json!({"outbox":a,"manifest":b}))
                    .unwrap_or_else(|_| format!("{a}; {b}")),
            )),
            (a, b) => a.or(b).cloned(),
        };
        self.snapshot = next;
        RunView {
            artifacts: observed_artifacts(&self.snapshot.run),
            run: self.snapshot.run.clone(),
            manifest_path: self.snapshot.identity.manifest_path.clone(),
            manifest_applied: applied.is_ok(),
            outbox_saved: saved.is_ok(),
            active: false,
            record_error,
        }
    }
}

fn apply(snapshot: &Snapshot) -> Result<(), AppMessage> {
    let path = Path::new(&snapshot.identity.manifest_path);
    snapshot.identity.require_current_target()?;
    manifest::with_manifest(path, || {
        let mut v = manifest::read_supported(path)?;
        snapshot.identity.matches(&v)?;
        let before = v.clone();
        manifest::add_run(&mut v, &snapshot.run)?;
        if before == v {
            return Ok(());
        }
        manifest::write_manifest_value(path, &v)
    })
    .map_err(problem)
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    format: String,
    schema_version: u32,
    run_id: String,
    job_id: String,
    manifest_path: String,
    source_master: String,
    artifact: PublishedArtifact,
}

pub fn receipts(
    dir: &Path,
    identity: &ManifestIdentity,
    run_id: &str,
) -> (Vec<PublishedArtifact>, Vec<AppMessage>) {
    let mut artifacts = Vec::new();
    let mut warnings = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return (vec![], vec![problem(e)]),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warnings.push(problem(e));
                continue;
            }
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("published-")
            || entry.path().extension().and_then(|s| s.to_str()) != Some("json")
        {
            continue;
        }
        let parsed = (|| {
            if !entry.file_type().map_err(problem)?.is_file() {
                return Err(problem("Invalid publication receipt"));
            }
            let r: Receipt = serde_json::from_slice(&fs::read(entry.path()).map_err(problem)?)
                .map_err(problem)?;
            if r.format != "capturescribe.publication"
                || r.schema_version != 1
                || r.run_id != run_id
                || r.job_id != identity.job_id
                || r.manifest_path != identity.manifest_path
                || r.source_master != identity.source_master_path
                || !matches!(r.artifact.role.as_str(), "user_output" | "transcript")
                || !Path::new(&r.artifact.path).is_absolute()
                || Path::new(&r.artifact.path).parent()
                    != Path::new(&identity.source_master_path).parent()
                || r.artifact.path == identity.source_master_path
                || r.artifact.validation != "publication-only"
            {
                return Err(problem("Receipt identity mismatch; not attributed"));
            }
            Ok(r.artifact)
        })();
        match parsed {
            Ok(a) => {
                if !artifacts
                    .iter()
                    .any(|old: &PublishedArtifact| old.path == a.path)
                {
                    artifacts.push(a);
                }
            }
            Err(e) => warnings.push(e),
        }
    }
    artifacts.sort_by(|a, b| a.path.cmp(&b.path));
    (artifacts, warnings)
}

/// Read/reconcile only registered runs; never execute a worker, traverse other
/// directories, rewrite a corrupt record or guess terminal times after a crash.
pub fn reconcile(root: &Path) -> (Vec<RunView>, Vec<AppMessage>) {
    let mut views = Vec::new();
    let mut warnings = Vec::new();
    let entries = match fs::read_dir(root.join("processing-outbox")) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (views, warnings),
        Err(e) => return (views, vec![problem(e)]),
    };
    for entry in entries {
        let read_one = (|| {
            let entry = entry.map_err(problem)?;
            let dir = entry.path();
            if !entry.file_type().map_err(problem)?.is_dir()
                || !valid_id(&entry.file_name().to_string_lossy())
            {
                return Err(problem("Invalid outbox entry"));
            }
            let live = lock_file(&dir)?;
            let active = match live.try_lock() {
                Ok(()) => false,
                Err(std::fs::TryLockError::WouldBlock) => true,
                Err(e) => return Err(problem(e)),
            };
            let accepted = read(&dir.join("accepted.json"))?;
            if accepted.run.run_id.as_deref() != entry.file_name().to_str()
                || accepted.run.status != "accepted"
            {
                return Err(problem("Outbox directory/run mismatch"));
            }
            let mut snapshot = accepted.clone();
            for phase in ["started.json", "terminal.json"] {
                let path = dir.join(phase);
                if fs::symlink_metadata(&path).is_ok() {
                    let next = read(&path)?;
                    if (phase == "started.json" && next.run.status != "running")
                        || (phase == "terminal.json" && !next.run.terminal())
                    {
                        return Err(problem("Invalid run phase status"));
                    }
                    if next.identity != accepted.identity
                        || next.run.run_id != accepted.run.run_id
                        || next.run.settings != accepted.run.settings
                        || next.run.accepted_at_unix_ms != accepted.run.accepted_at_unix_ms
                    {
                        return Err(problem("Outbox phase mismatch"));
                    }
                    snapshot = next;
                }
            }
            let mut applied = false;
            let mut record_error = None;
            if snapshot.run.terminal() && !active {
                match apply(&snapshot) {
                    Ok(()) => applied = true,
                    Err(e) => record_error = Some(e),
                }
            } else if !active {
                // Manifest may have won when terminal outbox publication failed.
                let existing =
                    manifest::read_supported(Path::new(&snapshot.identity.manifest_path))
                        .ok()
                        .filter(|v| snapshot.identity.matches(v).is_ok())
                        .and_then(|v| {
                            v["processingRuns"]
                                .as_array()
                                .and_then(|rs| {
                                    rs.iter().find(|r| {
                                        r["runId"] == snapshot.run.run_id.as_deref().unwrap_or("")
                                    })
                                })
                                .cloned()
                        })
                        .and_then(|r| serde_json::from_value::<ProcessingRun>(r).ok())
                        .filter(|r| r.terminal());
                if let Some(run) = existing {
                    snapshot.run = run;
                    applied = true;
                } else {
                    snapshot.run.status = "unknown".into();
                    let (a, w) = receipts(
                        &dir,
                        &snapshot.identity,
                        snapshot.run.run_id.as_deref().unwrap(),
                    );
                    snapshot.run.artifact_paths = a.iter().map(|a| a.path.clone()).collect();
                    snapshot.run.published_artifacts = a;
                    warnings.extend(w);
                }
            }
            Ok(RunView {
                artifacts: observed_artifacts(&snapshot.run),
                run: snapshot.run,
                manifest_path: snapshot.identity.manifest_path,
                manifest_applied: applied,
                outbox_saved: true,
                active,
                record_error,
            })
        })();
        match read_one {
            Ok(v) => views.push(v),
            Err(e) => warnings.push(e),
        }
    }
    views.sort_by(|a, b| b.run.accepted_at_unix_ms.cmp(&a.run.accepted_at_unix_ms));
    (views, warnings)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub struct Fixture {
        pub root: PathBuf,
        pub source: PathBuf,
        pub path: PathBuf,
        pub history: PathBuf,
    }
    impl Fixture {
        pub fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "capturescribe-runs-{}",
                crate::history_index::token()
            ));
            fs::create_dir(&root).unwrap();
            let source = root.join("Synthetic.mp4");
            fs::write(&source, b"isolated source").unwrap();
            let report = root.join("Synthetic.run.json");
            fs::write(&report, b"{}").unwrap();
            let path = root.join("Synthetic.capture.json");
            manifest::write_initial_manifest("fixture-job", "test", &source, &report, 12.0, &path)
                .unwrap();
            let history = root.join("history");
            Self {
                root,
                source,
                path,
                history,
            }
        }
        pub fn settings() -> ProcessingSettings {
            ProcessingSettings {
                trim_trailing_silence: false,
                split_long_video: true,
                split_threshold_minutes: 10,
                split_segment_minutes: 7,
                transcribe: true,
                transcription_locale: Some("en-US".into()),
            }
        }
        pub fn execution(&self) -> Execution {
            Execution::accept(
                &self.history,
                ManifestIdentity::read(&self.path).unwrap(),
                Some("fixture-record".into()),
                Self::settings(),
            )
            .unwrap()
        }
        pub fn value(&self) -> Value {
            serde_json::from_slice(&fs::read(&self.path).unwrap()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn phases_settings_all_terminal_states_and_restart_reconcile_are_idempotent() {
        let f = Fixture::new();
        let original = fs::read(&f.source).unwrap();
        for status in [
            "completed",
            "partial",
            "failed",
            "cancelled",
            "not-executed",
            "interrupted",
            "unknown",
        ] {
            let mut e = f.execution();
            e.start().unwrap();
            let mut run = e.run().clone();
            run.status = status.into();
            run.error = Some(
                AppMessage::known("processing.worker_failed", "fixture").param("test", "retained"),
            );
            let id = run.run_id.clone();
            let view = e.finish(run);
            assert!(view.outbox_saved && view.manifest_applied);
            drop(e);
            let before = fs::read(&f.path).unwrap();
            let (views, w) = reconcile(&f.history);
            assert!(w.is_empty());
            let v = views.iter().find(|v| v.run.run_id == id).unwrap();
            assert_eq!(v.run.settings, Some(Fixture::settings()));
            assert!(v.manifest_applied);
            assert_eq!(fs::read(&f.path).unwrap(), before);
        }
        assert_eq!(f.value()["processingRuns"].as_array().unwrap().len(), 7);
        assert_eq!(fs::read(&f.source).unwrap(), original);
    }
    #[test]
    fn manifest_failure_retains_terminal_outbox_and_safe_reconciliation_checks_identity() {
        let f = Fixture::new();
        let original = fs::read(&f.path).unwrap();
        let mut e = f.execution();
        e.start().unwrap();
        let mut changed = f.value();
        changed["jobId"] = serde_json::json!("different-job");
        fs::write(&f.path, serde_json::to_vec(&changed).unwrap()).unwrap();
        let mut run = e.run().clone();
        run.status = "completed".into();
        let view = e.finish(run);
        assert!(view.outbox_saved && !view.manifest_applied);
        drop(e);
        let before = fs::read(&f.path).unwrap();
        let (v, _) = reconcile(&f.history);
        assert!(!v[0].manifest_applied);
        assert_eq!(fs::read(&f.path).unwrap(), before);
        fs::write(&f.path, original).unwrap();
        let (v, w) = reconcile(&f.history);
        assert!(w.is_empty());
        assert!(v[0].manifest_applied);
        assert_eq!(f.value()["processingRuns"].as_array().unwrap().len(), 1);
        let before = fs::read(&f.path).unwrap();
        reconcile(&f.history);
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
    #[test]
    fn active_and_crashed_runs_do_not_become_completed_or_get_invented_finish_times() {
        let f = Fixture::new();
        let mut e = f.execution();
        e.start().unwrap();
        let (v, w) = reconcile(&f.history);
        assert!(w.is_empty());
        assert!(v[0].active);
        assert_eq!(v[0].run.status, "running");
        drop(e);
        let (v, w) = reconcile(&f.history);
        assert!(w.is_empty());
        assert!(!v[0].active);
        assert_eq!(v[0].run.status, "unknown");
        assert!(v[0].run.finished_at_unix_ms.is_none());
        assert!(f.value()["processingRuns"]
            .as_array()
            .is_none_or(|a| a.is_empty()));
    }
    #[test]
    fn manifest_only_terminal_survives_outbox_write_failure() {
        let f = Fixture::new();
        let mut e = f.execution();
        e.start().unwrap();
        fs::write(e.directory.join("terminal.json"), b"corrupt retained").unwrap();
        let mut run = e.run().clone();
        run.status = "partial".into();
        let view = e.finish(run);
        assert!(!view.outbox_saved && view.manifest_applied);
        assert!(view.record_error.is_some());
        assert_eq!(
            fs::read(e.directory.join("terminal.json")).unwrap(),
            b"corrupt retained"
        );
        // A corrupt phase is reported, never silently repaired by a new run.
        drop(e);
        let (_, w) = reconcile(&f.history);
        assert_eq!(w.len(), 1);
        assert_eq!(f.value()["processingRuns"][0]["status"], "partial");
    }
    #[test]
    fn missing_terminal_with_manifest_result_is_read_without_reexecution() {
        let f = Fixture::new();
        let mut e = f.execution();
        e.start().unwrap();
        let mut s = e.snapshot.clone();
        s.run.status = "completed".into();
        s.run.finished_at_unix_ms = Some(now());
        apply(&s).unwrap();
        drop(e);
        // Concurrent process-spawn tests can briefly inherit the flock FD
        // between fork and exec (CLOEXEC). Wait for actual lock release;
        // never make the production reader treat an occupied lock as idle.
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
        let (v,w)=loop{let result=reconcile(&f.history);if result.0.first().is_none_or(|v|!v.active)||std::time::Instant::now()>=deadline{break result;}std::thread::sleep(std::time::Duration::from_millis(10));};
        assert!(w.is_empty());
        assert!(v[0].manifest_applied);
        assert_eq!(v[0].run.status, "completed");
    }
    #[test]
    fn concurrent_runs_merge_and_same_id_conflict_keeps_previous() {
        let f = Fixture::new();
        let executions: Vec<_> = (0..8).map(|_| f.execution()).collect();
        let handles: Vec<_> = executions
            .into_iter()
            .map(|mut e| {
                std::thread::spawn(move || {
                    e.start().unwrap();
                    let mut r = e.run().clone();
                    r.status = "completed".into();
                    assert!(e.finish(r).manifest_applied);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(f.value()["processingRuns"].as_array().unwrap().len(), 8);
        let mut e = f.execution();
        e.start().unwrap();
        let mut r = e.run().clone();
        r.status = "failed".into();
        e.finish(r);
        let before = fs::read(&f.path).unwrap();
        let mut conflicting = e.snapshot.clone();
        conflicting.run.status = "completed".into();
        assert!(apply(&conflicting).is_err());
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
    #[test]
    fn legacy_runs_extensions_and_unsupported_or_broken_data_are_preserved() {
        let f = Fixture::new();
        let mut v = f.value();
        v["vendorExtension"] = serde_json::json!({"keep":true});
        v["processingRuns"] = serde_json::json!([{"status":"completed","artifactPaths":[],"warnings":[],"legacyExtra":"keep"}]);
        fs::write(&f.path, serde_json::to_vec(&v).unwrap()).unwrap();
        let mut e = f.execution();
        let mut r = e.run().clone();
        r.status = "not-executed".into();
        assert!(e.finish(r).manifest_applied);
        drop(e);
        assert_eq!(f.value()["vendorExtension"], v["vendorExtension"]);
        assert_eq!(f.value()["processingRuns"][0], v["processingRuns"][0]);
        let old: ProcessingRun = serde_json::from_value(v["processingRuns"][0].clone()).unwrap();
        assert!(
            old.run_id.is_none()
                && old.settings.is_none()
                && old.started_at_unix_ms.is_none()
                && old.finished_at_unix_ms.is_none()
        );
        for bytes in [b"broken".to_vec(),br#"{"schemaVersion":999,"artifacts":[]}"#.to_vec(),br#"{"schemaVersion":2,"artifacts":[],"processingRuns":[{"status":"failed","runSchemaVersion":999}]}"#.to_vec()]{fs::write(&f.path,&bytes).unwrap();assert!(ManifestIdentity::read(&f.path).is_err());assert_eq!(fs::read(&f.path).unwrap(),bytes);}
        let dir = f.history.join("processing-outbox").join("run-future");
        fs::create_dir(&dir).unwrap();
        let bytes = br#"{"format":"capturescribe.processing-run","schemaVersion":999}"#;
        fs::write(dir.join("accepted.json"), bytes).unwrap();
        let (_, w) = reconcile(&f.history);
        assert!(!w.is_empty());
        assert_eq!(fs::read(dir.join("accepted.json")).unwrap(), bytes);
    }
    #[test]
    fn accepted_or_start_record_failure_never_requires_a_worker() {
        let f = Fixture::new();
        fs::write(&f.history, b"blocked").unwrap();
        assert!(Execution::accept(
            &f.history,
            ManifestIdentity::read(&f.path).unwrap(),
            None,
            Fixture::settings()
        )
        .is_err());
        assert_eq!(fs::read(&f.history).unwrap(), b"blocked");
        let h = f.root.join("other-history");
        let mut e = Execution::accept(
            &h,
            ManifestIdentity::read(&f.path).unwrap(),
            None,
            Fixture::settings(),
        )
        .unwrap();
        fs::write(e.directory.join("started.json"), b"partial write preserved").unwrap();
        assert!(e.start().is_err());
        assert_eq!(e.run().status, "accepted");
    }
}
