//! A small, recording-specific reservation and non-overwriting publication unit.
use crate::message::AppMessage;
use crate::recording_manifest::{file_identity, write_initial_manifest_for_source};
use crate::save_destination::{DirectoryGuard, MANAGEMENT_DIRECTORY};
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct NameReservation {
    pub directory: PathBuf,
    pub final_path: PathBuf,
    management: DirectoryGuard,
}

fn failure(error: impl std::fmt::Display, recovery: &Path) -> AppMessage {
    AppMessage::known("capture.finalize_failed", error.to_string())
        .param("path", recovery.to_string_lossy().into_owned())
}

// A leftover report/manifest or a derived output belongs to the same name
// family, even when the source MP4 was removed. No unrelated file is deleted.
fn family_occupied(directory: &Path, management: &Path, stem: &str) -> std::io::Result<bool> {
    let mut paths = vec![directory.join(format!("{stem}.mp4"))];
    for parent in [directory, management] {
        paths.extend([".run.json", ".capture.json", ".capture.json.processing.lock"]
            .map(|suffix| parent.join(format!("{stem}{suffix}"))));
    }
    for path in paths {
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    let prefixes =
        ["_processed", "_part", "_文字起こし"].map(|s| format!("{stem}{s}").to_lowercase());
    for entry in fs::read_dir(directory)? {
        let name = entry?.file_name().to_string_lossy().to_lowercase();
        if (name.ends_with(".mp4") || name.ends_with(".txt"))
            && prefixes.iter().any(|prefix| name.starts_with(prefix))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn reserve_name(directory: &Path, requested: &str) -> Result<NameReservation, AppMessage> {
    let base = directory.to_str().ok_or_else(|| failure("Invalid save path", directory))?;
    let management = DirectoryGuard::prepare(base, MANAGEMENT_DIRECTORY)?;
    reserve_in(directory, requested, &management)
}

fn reserve_in(
    directory: &Path,
    requested: &str,
    management: &DirectoryGuard,
) -> Result<NameReservation, AppMessage> {
    management.verify()?;
    let mut number = 1u64;
    loop {
        // Treat a user-entered suffix as literal text; never strip guessed numbering.
        let stem = if number == 1 {
            requested.to_owned()
        } else {
            format!("{requested} ({number})")
        };
        if !family_occupied(directory, &management.directory, &stem).map_err(|e| failure(e, directory))? {
            let reservation = directory.join(format!(".{stem}.capture-reservation"));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&reservation) {
                Ok(()) => {
                    let final_path = directory.join(format!("{stem}.mp4"));
                    // This directory also contains the working media. A crash/error
                    // leaves an intelligible recovery location, not an empty MP4.
                    let note = serde_json::json!({
                        "requestedStem": requested, "plannedSourceMaster": final_path,
                        "workingVideo": "source.mp4", "workingReport": "report.json",
                        "note": "Incomplete work: retain files; do not automatically reuse this reservation."
                    });
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(reservation.join("reservation.json"))
                        .map_err(|e| failure(e, &reservation))?;
                    file.write_all(note.to_string().as_bytes())
                        .and_then(|_| file.sync_all())
                        .map_err(|e| failure(e, &reservation))?;
                    // Prove this filesystem can place metadata before starting the capture helper.
                    // The probe uses a unique name in our reservation family and never replaces a file.
                    let probe = format!(
                        "{}-probe-{}",
                        reservation.file_name().unwrap().to_string_lossy(),
                        std::process::id()
                    );
                    link_management_file(
                        management,
                        &reservation.join("reservation.json"),
                        OsStr::new(&probe),
                        true,
                    ).map_err(|e| failure(e, &reservation))?;
                    return Ok(NameReservation {
                        directory: reservation,
                        final_path,
                        management: management.clone(),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => (),
                Err(error) => return Err(failure(error, directory)),
            }
        }
        number = number
            .checked_add(1)
            .ok_or_else(|| failure("Recording name space exhausted", directory))?;
    }
}

// The verified open directory prevents a replaced path or hidden symlink from
// redirecting publication. The source remains in our private reservation.
#[cfg(target_os = "macos")]
fn link_management_file(
    management: &DirectoryGuard,
    source: &Path,
    name: &OsStr,
    probe: bool,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let directory = management.open_verified().map_err(|e| std::io::Error::other(e.to_string()))?;
    let source = CString::new(source.as_os_str().as_bytes())?;
    let name = CString::new(name.as_bytes())?;
    if unsafe { libc::linkat(libc::AT_FDCWD, source.as_ptr(), directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if probe && unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn link_management_file(
    management: &DirectoryGuard,
    source: &Path,
    name: &OsStr,
    probe: bool,
) -> std::io::Result<()> {
    management.verify().map_err(|e| std::io::Error::other(e.to_string()))?;
    let target = management.directory.join(name);
    fs::hard_link(source, &target)?;
    if probe {
        fs::remove_file(target)?;
    }
    Ok(())
}

// Keep all working originals until the ENTIRE bundle is published. hard_link
// creates each destination atomically or refuses a collision, including links.
// Manifest is last: ordinary history never sees an incomplete recording bundle.
fn publish_bundle(
    video: &Path,
    report: &Path,
    manifest: &Path,
    reservation: &NameReservation,
) -> std::io::Result<()> {
    let management = &reservation.management;
    management.verify().map_err(|e| std::io::Error::other(e.to_string()))?;
    fs::hard_link(video, &reservation.final_path)?;
    link_management_file(management, report,
        reservation.final_path.with_extension("run.json").file_name().unwrap(), false)?;
    link_management_file(management, manifest,
        reservation.final_path.with_extension("capture.json").file_name().unwrap(), false)
}

pub fn finalize_recording(
    destination: &DirectoryGuard,
    initial: &NameReservation,
    requested: &str,
    job_id: &str,
    engine_id: &str,
    duration: f64,
) -> Result<(PathBuf, PathBuf), AppMessage> {
    let video = initial.directory.join("source.mp4");
    let report = initial.directory.join("report.json");
    let mut names = vec![initial.clone()];
    let mut prepared = Vec::new();
    loop {
        destination.verify()?;
        let current = names.last().unwrap();
        current.management.verify()?;
        let stem = current.final_path.file_stem().unwrap().to_string_lossy();
        let management = &initial.management;
        if family_occupied(&destination.directory, &management.directory, &stem)
            .map_err(|e| failure(e, &initial.directory))?
        {
            names.push(reserve_in(&destination.directory, requested, management)?);
            continue;
        }
        let manifest_work = initial
            .directory
            .join(format!("manifest-{}.json", names.len()));
        let metadata_base = management.directory.join(current.final_path.file_name().unwrap());
        let manifest_path = metadata_base.with_extension("capture.json");
        let report_path = metadata_base.with_extension("run.json");
        fs::write(
            current.directory.join("reservation.json"),
            serde_json::json!({
                "requestedStem": requested, "phase": "finalizing",
                "workingVideo": video, "workingReport": report,
                "preparedManifest": manifest_work, "plannedSourceMaster": current.final_path,
                "plannedReport": report_path,
                "plannedManifest": manifest_path,
                "note": "Retain all partial public files and working originals on failure."
            })
            .to_string(),
        )
        .map_err(|e| failure(e, &initial.directory))?;
        write_initial_manifest_for_source(
            job_id,
            engine_id,
            &current.final_path,
            &report_path,
            file_identity(&video)?,
            duration,
            &manifest_work,
        )
        .map_err(|e| failure(e, &initial.directory))?;
        prepared.push(manifest_work.clone());
        match publish_bundle(&video, &report, &manifest_work, current) {
            Ok(()) => {
                destination.verify()?;
                management.verify()?;
                let final_path = current.final_path.clone();
                // Only our exact temporary aliases are removed, after all three
                // public links exist. A cleanup failure leaves safe recovery data.
                for path in [video.clone(), report.clone()].into_iter().chain(prepared) {
                    let _ = fs::remove_file(path);
                }
                for name in names {
                    let _ = fs::remove_file(name.directory.join("reservation.json"));
                    let _ = fs::remove_dir(name.directory); // Never recursive.
                }
                return Ok((final_path, manifest_path));
            }
            Err(error) => {
                // A collision at the FIRST publish leaves the working video with
                // no public alias of ours: safely reserve a new name and retry.
                // Any partial publish is retained and reported, never rolled back.
                let partial = same_file(&video, &current.final_path);
                if error.kind() == ErrorKind::AlreadyExists && !partial {
                    names.push(reserve_in(&destination.directory, requested, management)?);
                    continue;
                }
                return Err(failure(format!("{error}; intended output: {}; partial public files and working originals retained", current.final_path.display()), &initial.directory));
            }
        }
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(left), fs::symlink_metadata(right)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        true
    } // Other capture engines remain unimplemented.
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("capturescribe-name-{}", crate::history_index::token()));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn destination(&self) -> DirectoryGuard {
            DirectoryGuard::prepare(self.0.to_str().unwrap(), "course/lesson01").unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn record(name: &NameReservation) {
        fs::write(name.directory.join("source.mp4"), b"source recording").unwrap();
        fs::write(name.directory.join("report.json"), b"capture report").unwrap();
    }
    fn finish(
        destination: &DirectoryGuard,
        name: &NameReservation,
        requested: &str,
    ) -> (PathBuf, PathBuf) {
        finalize_recording(destination, name, requested, "job", "test-engine", 2.0).unwrap()
    }
    #[test]
    fn sequential_recordings_keep_one_family_and_manifest_references() {
        let temp = Temp::new();
        let d = temp.destination();
        for (index, stem) in ["Lecture", "Lecture (2)", "Lecture (3)"].iter().enumerate() {
            let n = reserve_name(&d.directory, "Lecture").unwrap();
            record(&n);
            assert_eq!(n.final_path.file_stem().unwrap(), *stem);
            let (video, manifest) = finish(&d, &n, "Lecture");
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
            assert_eq!(value["schemaVersion"], 2);
            assert_eq!(value["artifacts"][0]["path"], video.to_str().unwrap());
            assert_eq!(
                value["sourceTimeline"]["mappingReport"],
                manifest.parent().unwrap().join(video.file_name().unwrap()).with_extension("run.json").to_str().unwrap()
            );
            assert!(!n.directory.exists());
            assert_eq!(
                fs::read(d.directory.join("Lecture.mp4")).unwrap(),
                b"source recording"
            );
            assert_eq!(manifest.parent().unwrap(), d.directory.join(MANAGEMENT_DIRECTORY));
            assert_eq!(value["artifacts"][0]["physicalIdentity"], file_identity(&video).unwrap());
            assert_eq!(fs::read_dir(&d.directory).unwrap().count(), index + 2);
            assert_eq!(fs::read_dir(manifest.parent().unwrap()).unwrap().count(), (index + 1) * 2);
        }
    }
    #[test]
    fn related_or_dangling_entries_reserve_the_whole_family() {
        for existing in [
            "Lecture.run.json",
            "Lecture.capture.json",
            "Lecture_processed.mp4",
            "Lecture_文字起こし (2).txt",
            "Lecture_part1 (2)_文字起こし.txt",
        ] {
            let temp = Temp::new();
            fs::write(temp.0.join(existing), b"existing").unwrap();
            let n = reserve_name(&temp.0, "Lecture").unwrap();
            assert_eq!(n.final_path.file_name().unwrap(), "Lecture (2).mp4");
            assert_eq!(fs::read(temp.0.join(existing)).unwrap(), b"existing");
        }
        #[cfg(unix)]
        {
            let temp = Temp::new();
            std::os::unix::fs::symlink(temp.0.join("missing"), temp.0.join("Lecture.mp4")).unwrap();
            assert_eq!(
                reserve_name(&temp.0, "Lecture")
                    .unwrap()
                    .final_path
                    .file_name()
                    .unwrap(),
                "Lecture (2).mp4"
            );
        }
    }
    #[test]
    fn gaps_literals_and_stale_reservations_are_preserved() {
        let temp = Temp::new();
        fs::write(temp.0.join("Lecture.mp4"), b"one").unwrap();
        fs::write(temp.0.join("Lecture (2).mp4"), b"two").unwrap();
        assert_eq!(
            reserve_name(&temp.0, "Lecture")
                .unwrap()
                .final_path
                .file_name()
                .unwrap(),
            "Lecture (3).mp4"
        );
        assert_eq!(
            reserve_name(&temp.0, "Lecture (2)")
                .unwrap()
                .final_path
                .file_name()
                .unwrap(),
            "Lecture (2) (2).mp4"
        );
        let n = reserve_name(&temp.0, "Stale").unwrap();
        assert_eq!(
            reserve_name(&temp.0, "Stale")
                .unwrap()
                .final_path
                .file_name()
                .unwrap(),
            "Stale (2).mp4"
        );
        assert!(n.directory.exists());
    }
    #[test]
    fn simultaneous_reservations_are_exclusive() {
        let temp = Temp::new();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let p = temp.0.clone();
                std::thread::spawn(move || reserve_name(&p, "Concurrent").unwrap().final_path)
            })
            .collect();
        let names: std::collections::HashSet<_> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(names.len(), 4);
    }
    #[test]
    fn collision_after_start_reallocates_before_any_publish() {
        let temp = Temp::new();
        let d = temp.destination();
        let n = reserve_name(&d.directory, "Lecture").unwrap();
        record(&n);
        fs::write(
            n.final_path.with_extension("capture.json"),
            b"late manifest",
        )
        .unwrap();
        let (video, _) = finish(&d, &n, "Lecture");
        assert_eq!(video.file_name().unwrap(), "Lecture (2).mp4");
        assert_eq!(
            fs::read(n.final_path.with_extension("capture.json")).unwrap(),
            b"late manifest"
        );
        assert!(!n.final_path.exists());
    }
    #[test]
    fn last_moment_and_partial_collisions_keep_working_and_existing_files() {
        let temp = Temp::new();
        let n = reserve_name(&temp.0, "Race").unwrap();
        record(&n);
        let manifest = n.directory.join("prepared.json");
        fs::write(&manifest, b"manifest").unwrap();
        fs::write(&n.final_path, b"external video").unwrap();
        assert!(publish_bundle(
            &n.directory.join("source.mp4"),
            &n.directory.join("report.json"),
            &manifest,
            &n
        )
        .is_err());
        assert_eq!(fs::read(&n.final_path).unwrap(), b"external video");
        fs::remove_file(&n.final_path).unwrap(); // Synthetic test-owned fixture only.
        let metadata = n.management.directory.join(n.final_path.file_name().unwrap());
        fs::write(metadata.with_extension("run.json"), b"external report").unwrap();
        assert!(publish_bundle(
            &n.directory.join("source.mp4"),
            &n.directory.join("report.json"),
            &manifest,
            &n
        )
        .is_err());
        assert_eq!(
            fs::read(metadata.with_extension("run.json")).unwrap(),
            b"external report"
        );
        assert_eq!(fs::read(&n.final_path).unwrap(), b"source recording");
        assert!(n.directory.join("source.mp4").exists());
        assert!(manifest.exists());
        assert!(!metadata.with_extension("capture.json").exists());
    }
    #[test]
    fn hidden_metadata_collisions_reserve_another_name() {
        let temp = Temp::new();
        let root = temp.destination();
        let management = root.directory.join(MANAGEMENT_DIRECTORY);
        fs::create_dir(&management).unwrap();
        let lock = management.join("Lecture.capture.json.processing.lock");
        fs::write(&lock, b"old lock").unwrap();
        let name = reserve_name(&root.directory, "Lecture").unwrap();
        assert_eq!(name.final_path.file_name().unwrap(), "Lecture (2).mp4");
        assert_eq!(fs::read(lock).unwrap(), b"old lock");
    }

    #[test]
    fn replaced_management_directory_preserves_working_source() {
        let temp = Temp::new();
        let root = temp.destination();
        let name = reserve_name(&root.directory, "Lecture").unwrap();
        record(&name);
        let management = root.directory.join(MANAGEMENT_DIRECTORY);
        fs::rename(&management, root.directory.join("old-management")).unwrap();
        fs::create_dir(&management).unwrap();
        assert!(finalize_recording(&root, &name, "Lecture", "job", "engine", 1.0).is_err());
        assert_eq!(fs::read(name.directory.join("source.mp4")).unwrap(), b"source recording");
        assert!(!name.final_path.exists());
        assert_eq!(fs::read_dir(management).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn invalid_management_path_stops_before_reservation() {
        let temp = Temp::new();
        let root = temp.destination();
        let management = root.directory.join(MANAGEMENT_DIRECTORY);
        std::os::unix::fs::symlink(&temp.0, &management).unwrap();
        assert!(reserve_name(&root.directory, "Lecture").is_err());
        assert_eq!(fs::read_dir(&root.directory).unwrap().count(), 1);
        fs::remove_file(&management).unwrap();
        fs::write(&management, b"existing file").unwrap();
        assert!(reserve_name(&root.directory, "Lecture").is_err());
        assert_eq!(fs::read(management).unwrap(), b"existing file");
    }

    #[test]
    fn manifest_preparation_failure_never_discards_source() {
        let temp = Temp::new();
        let d = temp.destination();
        let n = reserve_name(&d.directory, "Failure").unwrap();
        record(&n);
        fs::write(n.directory.join("manifest-1.json"), b"keep existing").unwrap();
        assert!(finalize_recording(&d, &n, "Failure", "job", "engine", 2.0).is_err());
        assert_eq!(
            fs::read(n.directory.join("manifest-1.json")).unwrap(),
            b"keep existing"
        );
        assert_eq!(
            fs::read(n.directory.join("source.mp4")).unwrap(),
            b"source recording"
        );
        assert!(!n.final_path.exists());
    }

    #[test]
    fn manifest_publish_failure_keeps_all_partial_and_working_files() {
        let temp = Temp::new();
        let n = reserve_name(&temp.0, "Manifest race").unwrap();
        record(&n);
        let prepared = n.directory.join("prepared.json");
        fs::write(&prepared, b"our prepared manifest").unwrap();
        let metadata = n.management.directory.join(n.final_path.file_name().unwrap());
        let existing = metadata.with_extension("capture.json");
        fs::write(&existing, b"external manifest").unwrap();
        assert!(publish_bundle(
            &n.directory.join("source.mp4"),
            &n.directory.join("report.json"),
            &prepared,
            &n
        )
        .is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"external manifest");
        assert_eq!(fs::read(&n.final_path).unwrap(), b"source recording");
        assert_eq!(
            fs::read(metadata.with_extension("run.json")).unwrap(),
            b"capture report"
        );
        assert_eq!(fs::read(&prepared).unwrap(), b"our prepared manifest");
        assert!(n.directory.join("source.mp4").exists());
        assert!(n.directory.join("report.json").exists());
    }
}
