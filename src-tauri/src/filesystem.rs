use crate::message::AppMessage;
use std::path::PathBuf;
use std::process::Command;

fn existing_path(value: &str) -> Result<PathBuf, AppMessage> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppMessage::known(
            "filesystem.path_required",
            "表示するパスが空です",
        ));
    }
    let path = PathBuf::from(value);
    if !path.exists() {
        return Err(AppMessage::known(
            "filesystem.not_found",
            format!("path not found: {}", path.display()),
        )
        .param("path", path.to_string_lossy().into_owned()));
    }
    Ok(path)
}

#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), AppMessage> {
    let path = existing_path(&path)?;

    #[cfg(target_os = "macos")]
    let status = {
        let mut command = Command::new("/usr/bin/open");
        if path.is_file() {
            command.arg("-R");
        }
        command.arg(&path).status()
    };

    #[cfg(target_os = "windows")]
    let status = {
        let mut command = Command::new("explorer.exe");
        if path.is_file() {
            command.arg(format!("/select,{}", path.display()));
        } else {
            command.arg(&path);
        }
        command.status()
    };

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let status = {
        let target = if path.is_dir() {
            path.as_path()
        } else {
            path.parent().unwrap_or(path.as_path())
        };
        Command::new("xdg-open").arg(target).status()
    };

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(AppMessage::known(
            "filesystem.reveal_failed",
            status.to_string(),
        )),
        Err(error) => Err(AppMessage::known(
            "filesystem.reveal_failed",
            error.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::existing_path;

    #[test]
    fn reveal_rejects_empty_and_missing_paths() {
        assert!(existing_path(" ").is_err());
        assert!(existing_path("/capture-scribe-path-that-does-not-exist").is_err());
    }
}
