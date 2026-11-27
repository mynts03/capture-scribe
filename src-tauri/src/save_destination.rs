use crate::message::AppMessage;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const MANAGEMENT_DIRECTORY: &str = ".capturescribe";

fn invalid_subfolder() -> AppMessage {
    AppMessage::known(
        "save.subfolder_invalid",
        "Use a relative subfolder without parent, empty, dot, or unsupported components",
    )
}

fn components(value: &str) -> Result<Vec<&str>, AppMessage> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    if value.contains(['\\', ':', '*', '?', '"', '<', '>', '|'])
        || value.chars().any(char::is_control)
    {
        return Err(invalid_subfolder());
    }
    let parts: Vec<_> = value.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.trim() != *p)
    {
        return Err(invalid_subfolder());
    }
    Ok(parts)
}

pub fn validated_stem(value: &str) -> Result<String, AppMessage> {
    let stem = value.trim().trim_end_matches(".mp4").trim();
    if stem.is_empty() {
        return Err(AppMessage::known(
            "capture.filename_required",
            "Enter a file name",
        ));
    }
    if stem.contains(['/', '\\', ':'])
        || stem.chars().any(char::is_control)
        || stem == "."
        || stem == ".."
    {
        return Err(AppMessage::known(
            "capture.filename_invalid",
            "Unsupported file name",
        ));
    }
    Ok(stem.to_owned())
}

fn unavailable(error: impl std::fmt::Display) -> AppMessage {
    AppMessage::known("capture.output_unavailable", error.to_string())
}

// Canonicalize the existing ancestor without creating anything during preview.
fn canonical_or_missing(path: &Path) -> Result<PathBuf, AppMessage> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::canonicalize(path).map_err(unavailable),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| unavailable(e))?;
            let name = path
                .file_name()
                .ok_or_else(|| unavailable("Invalid base folder"))?;
            Ok(canonical_or_missing(parent)?.join(name))
        }
        Err(e) => Err(unavailable(e)),
    }
}

pub fn resolve_directory(base: &str, subfolder: &str, create: bool) -> Result<PathBuf, AppMessage> {
    let parts = components(subfolder)?; // Invalid input must never create even the base.
    let base = PathBuf::from(base.trim());
    if base.as_os_str().is_empty() {
        return Err(AppMessage::known(
            "capture.output_required",
            "Enter a base folder",
        ));
    }
    if !base.is_absolute() {
        return Err(unavailable("The base folder must be absolute"));
    }
    if create {
        fs::create_dir_all(&base).map_err(unavailable)?;
    }
    let base = canonical_or_missing(&base)?;
    if base.exists() && !base.is_dir() {
        return Err(unavailable("The base is not a directory"));
    }
    #[cfg(target_os = "macos")]
    if create {
        create_subdirectories(&base, &parts)?;
    }
    let mut target = base.clone();
    for part in parts {
        target.push(part);
        let metadata = match fs::symlink_metadata(&target) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&target) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                    Err(e) => return Err(unavailable(e)),
                }
                Some(fs::symlink_metadata(&target).map_err(unavailable)?)
            }
            Err(e) => return Err(unavailable(e)),
        };
        if let Some(metadata) = metadata {
            // Reject internal as well as external links, including dangling links.
            if metadata.file_type().is_symlink() {
                return Err(AppMessage::known(
                    "save.subfolder_link",
                    "Subfolder contains a symbolic link",
                ));
            }
            if !metadata.is_dir() {
                return Err(unavailable("A subfolder component is not a directory"));
            }
            let canonical = fs::canonicalize(&target).map_err(unavailable)?;
            if !canonical.starts_with(&base) {
                return Err(invalid_subfolder());
            }
            target = canonical;
        }
    }
    Ok(target)
}

// mkdirat/openat bind each creation to the already-open parent, rather than
// traversing a possibly replaced path string. Never follow a child symlink.
#[cfg(target_os = "macos")]
fn create_subdirectories(base: &Path, parts: &[&str]) -> Result<(), AppMessage> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(base.as_os_str().as_bytes()).map_err(unavailable)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(name.as_ptr(), flags) };
    if fd < 0 {
        return Err(unavailable(std::io::Error::last_os_error()));
    }
    let mut parent = unsafe { OwnedFd::from_raw_fd(fd) };
    for part in parts {
        let name = CString::new(*part).map_err(unavailable)?;
        let mut fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            let made = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
            if made < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(unavailable(std::io::Error::last_os_error()));
            }
            fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(AppMessage::known(
                "save.subfolder_link",
                std::io::Error::last_os_error().to_string(),
            ));
        }
        parent = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DirectoryGuard {
    base: String,
    subfolder: String,
    pub directory: PathBuf,
    metadata: fs::Metadata,
}

impl DirectoryGuard {
    pub fn prepare(base: &str, subfolder: &str) -> Result<Self, AppMessage> {
        let directory = resolve_directory(base, subfolder, true)?;
        let metadata = fs::metadata(&directory).map_err(unavailable)?;
        Ok(Self {
            base: base.into(),
            subfolder: subfolder.into(),
            directory,
            metadata,
        })
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn open_verified(&self) -> Result<fs::File, AppMessage> {
        use std::os::unix::fs::OpenOptionsExt;
        self.verify()?;
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.directory)
            .map_err(unavailable)?;
        self.check_identity(&self.directory, &directory.metadata().map_err(unavailable)?)?;
        Ok(directory)
    }

    pub fn verify(&self) -> Result<(), AppMessage> {
        let directory = resolve_directory(&self.base, &self.subfolder, false)?;
        let metadata = fs::metadata(&directory).map_err(unavailable)?;
        self.check_identity(&directory, &metadata)
    }

    fn check_identity(&self, directory: &Path, metadata: &fs::Metadata) -> Result<(), AppMessage> {
        let same = directory == self.directory;
        #[cfg(unix)]
        let same = {
            use std::os::unix::fs::MetadataExt;
            same && metadata.dev() == self.metadata.dev() && metadata.ino() == self.metadata.ino()
        };
        #[cfg(not(unix))]
        let same = same && metadata.is_dir() && self.metadata.is_dir();
        if !same {
            return Err(AppMessage::known(
                "save.destination_changed",
                "Save directory changed during recording; source files are retained",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveDirectory {
    directory: String,
    exists: bool,
}

#[tauri::command]
pub fn preview_save_directory(
    output_directory: String,
    subfolder: String,
) -> Result<SaveDirectory, AppMessage> {
    let path = resolve_directory(&output_directory, &subfolder, false)?;
    Ok(SaveDirectory {
        exists: path.is_dir(),
        directory: path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub fn validate_file_name(file_name: String) -> Result<String, AppMessage> {
    validated_stem(&file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "capturescribe-save-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        fs::canonicalize(p).unwrap()
    }
    #[test]
    fn preview_is_read_only_and_start_creates_nested_folders() {
        let root = root();
        let base = root.join("base");
        let b = base.to_str().unwrap();
        assert_eq!(
            resolve_directory(b, "course/lesson01", false).unwrap(),
            base.join("course/lesson01")
        );
        assert!(!base.exists());
        assert_eq!(resolve_directory(b, "", true).unwrap(), base);
        let target = resolve_directory(b, "course/lesson01", true).unwrap();
        assert!(target.is_dir());
        fs::write(target.join("keep.mp4"), b"original").unwrap();
        resolve_directory(b, "course/lesson01", true).unwrap();
        assert_eq!(fs::read(target.join("keep.mp4")).unwrap(), b"original");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invalid_paths_do_not_create_base() {
        let root = root();
        let base = root.join("not-created");
        for value in [
            "/outside",
            "../outside",
            "a/../b",
            ".",
            "a/./b",
            "a//b",
            "a/",
            " a",
            "a ",
            "C:/x",
            "a\\b",
            "\\\\server\\share",
            "a\n",
            "a\0b",
        ] {
            assert!(
                resolve_directory(base.to_str().unwrap(), value, true).is_err(),
                "{value:?}"
            );
            assert!(!base.exists());
        }
        assert!(resolve_directory("relative", "", false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn all_subfolder_links_and_replacement_after_preview_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = root();
        let base = root.join("base");
        let outside = root.join("outside");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(base.join("real")).unwrap();
        symlink(&outside, base.join("escape")).unwrap();
        symlink(base.join("real"), base.join("inside")).unwrap();
        symlink(root.join("missing"), base.join("dangling")).unwrap();
        for sub in ["escape/new", "inside/new", "dangling/new"] {
            assert!(resolve_directory(base.to_str().unwrap(), sub, true).is_err());
        }
        resolve_directory(base.to_str().unwrap(), "later/new", false).unwrap();
        symlink(&outside, base.join("later")).unwrap();
        assert!(resolve_directory(base.to_str().unwrap(), "later/new", true).is_err());
        assert!(!outside.join("new").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn filename_rules_are_shared_with_clipboard_validation() {
        assert_eq!(validated_stem(" Lecture.mp4.mp4 ").unwrap(), "Lecture");
        for name in [
            "", " ", ".", "..", "../x", "a/b", "a\\b", "a:b", "a\nline", "a\0b",
        ] {
            assert!(validated_stem(name).is_err());
        }
    }
    #[test]
    fn directory_guard_rejects_replaced_directory() {
        let root = root();
        let base = root.join("base");
        let guard = DirectoryGuard::prepare(base.to_str().unwrap(), "nested").unwrap();
        guard.verify().unwrap();
        fs::rename(base.join("nested"), base.join("moved")).unwrap();
        fs::create_dir(base.join("nested")).unwrap();
        assert!(guard.verify().is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
