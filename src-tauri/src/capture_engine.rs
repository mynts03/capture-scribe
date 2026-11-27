use crate::message::AppMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourcePixelSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayInfo {
    pub id: u32,
    pub width_points: i64,
    pub height_points: i64,
    pub is_main: bool,
    pub display_ref: String,
    pub snapshot_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point_pixel_scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_pixels: Option<SourcePixelSize>,
}

impl DisplayInfo {
    fn from_helper(value: Value) -> Result<Self, &'static str> {
        let scale = value["pointPixelScale"].as_f64().filter(|s| s.is_finite() && *s > 0.0);
        let pixels = serde_json::from_value::<SourcePixelSize>(value["sourcePixels"].clone())
            .ok().filter(|size| size.width > 0 && size.height > 0);
        let (point_pixel_scale, source_pixels) = match scale.zip(pixels) {
            Some((scale, pixels)) => (Some(scale), Some(pixels)),
            None => (None, None),
        };
        Ok(Self {
            id: value["id"].as_u64().ok_or("display id missing")? as u32,
            width_points: value["widthPoints"].as_i64().ok_or("display width missing")?,
            height_points: value["heightPoints"].as_i64().ok_or("display height missing")?,
            is_main: value["isMain"].as_bool().unwrap_or(false),
            display_ref: value["displayRef"].as_str().ok_or("display reference missing")?.into(),
            snapshot_token: value["snapshotToken"].as_str().ok_or("display snapshot missing")?.into(),
            point_pixel_scale,
            source_pixels,
        })
    }
}

pub struct CaptureLaunch<'a> {
    pub output_path: &'a Path,
    pub report_path: &'a Path,
    pub display_id: u32,
    pub system_audio: bool,
    pub microphone: bool,
    pub max_duration_seconds: u64,
    pub area: Option<&'a crate::capture_area::CaptureArea>,
    pub approved_output: Option<crate::capture_area::OutputSize>,
}

pub struct CaptureProcess {
    pub child: Child,
    pub ready: Receiver<Result<u128, AppMessage>>,
    pub failure: Arc<Mutex<Option<AppMessage>>>,
    pub events: thread::JoinHandle<()>,
}

/// Boundary between the product lifecycle and an OS-specific recording path.
///
/// The initial implementation is an out-of-process Swift A2 adapter. A future
/// B implementation, Windows adapter, or in-process native module implements
/// this contract without changing recording validation or artifact promotion.
pub trait CaptureEngine {
    fn id(&self) -> &'static str;
    fn list_displays(&self) -> Result<Vec<DisplayInfo>, AppMessage>;
    fn start(
        &self,
        app: &AppHandle,
        launch: CaptureLaunch<'_>,
    ) -> Result<CaptureProcess, AppMessage>;
}

pub fn selected_engine(app: &AppHandle) -> Result<Box<dyn CaptureEngine>, AppMessage> {
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(SwiftA2Engine {
            helper: helper_path(app)?,
        }));
    }
    #[allow(unreachable_code)]
    Err("no recording engine is implemented for this operating system".into())
}

struct SwiftA2Engine {
    helper: PathBuf,
}

fn display_list_command(helper: &Path) -> Command {
    let mut command = Command::new(helper);
    // Startup/focus discovery must never request TCC approval automatically.
    command.args(["list", "--no-permission-prompt"]);
    command
}

impl CaptureEngine for SwiftA2Engine {
    fn id(&self) -> &'static str {
        "macos-swift-a2-v1"
    }

    fn list_displays(&self) -> Result<Vec<DisplayInfo>, AppMessage> {
        let output = display_list_command(&self.helper)
            .output()
            .map_err(|error| format!("could not start capture helper: {error}"))?;
        if !output.status.success() {
            let failure = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    let event: Value = serde_json::from_str(line).ok()?;
                    if event["event"] == "capture_error" {
                        serde_json::from_value(event["message"].clone()).ok()
                    } else {
                        None
                    }
                });
            return Err(failure.unwrap_or_else(|| {
                String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .to_string()
                    .into()
            }));
        }
        let text = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
        let json_start = text
            .find('[')
            .ok_or_else(|| format!("display list was not JSON: {text}"))?;
        let values: Vec<Value> =
            serde_json::from_str(&text[json_start..]).map_err(|error| error.to_string())?;
        values
            .into_iter()
            .map(DisplayInfo::from_helper)
            .collect::<Result<Vec<_>, &str>>()
            .map_err(AppMessage::from)
    }

    fn start(
        &self,
        app: &AppHandle,
        launch: CaptureLaunch<'_>,
    ) -> Result<CaptureProcess, AppMessage> {
        let mut command = Command::new(&self.helper);
        command
            .args(["record", "--output"])
            .arg(launch.output_path)
            .arg("--report")
            .arg(launch.report_path)
            .arg("--display-id")
            .arg(launch.display_id.to_string())
            .arg("--duration")
            .arg(launch.max_duration_seconds.clamp(60, 86_400).to_string())
            .arg(if launch.system_audio {
                "--system-audio"
            } else {
                "--no-system-audio"
            })
            .arg(if launch.microphone {
                "--microphone"
            } else {
                "--no-microphone"
            })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(area) = launch.area {
            command
                .arg("--area")
                .arg(serde_json::to_string(area).map_err(|e| e.to_string())?);
        }
        if let Some(size) = launch.approved_output {
            command
                .arg("--approved-output")
                .arg(serde_json::to_string(&size).map_err(|e| e.to_string())?);
        }

        let mut child = command
            .spawn()
            .map_err(|error| AppMessage::known("capture.helper_failed", error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "capture helper stdout is unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "capture helper stderr is unavailable".to_string())?;
        let (ready_sender, ready_receiver) = mpsc::channel();
        let failure = Arc::new(Mutex::new(None));
        let event_failure = failure.clone();
        let event_app = app.clone();
        let events = thread::spawn(move || {
            let mut ready_sender = Some(ready_sender);
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if value.get("event").and_then(Value::as_str) == Some("recording_ready") {
                        if let Some(sender) = ready_sender.take() {
                            let value = value["startedAtUnixMs"].as_u64().filter(|v| *v > 0)
                                .map(|v| v as u128)
                                .ok_or_else(|| AppMessage::known("capture.start_timing_invalid", "Capture helper did not provide a valid recording-start time"));
                            let _ = sender.send(value);
                        }
                    }
                    if value.get("event").and_then(Value::as_str) == Some("capture_error") {
                        if let Ok(message) =
                            serde_json::from_value::<AppMessage>(value["message"].clone())
                        {
                            if let Ok(mut failure) = event_failure.lock() {
                                *failure = Some(message.clone());
                            }
                            if let Some(sender) = ready_sender.take() {
                                let _ = sender.send(Err(message));
                            }
                        }
                    }
                    let _ = event_app.emit("capture-helper-event", value);
                }
            }
        });
        let log_app = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = log_app.emit("capture-helper-log", line);
            }
        });

        Ok(CaptureProcess {
            child,
            ready: ready_receiver,
            failure,
            events,
        })
    }
}

#[cfg(test)]
mod display_discovery_tests {
    use super::*;
    #[test]
    fn helper_display_metadata_reports_only_available_source_geometry() {
        let common = serde_json::json!({"id":7, "widthPoints":801, "heightPoints":600,
            "isMain":true, "displayRef":"macos-sck:test", "snapshotToken":"opaque"});
        for (scale, width, height) in [(1.0, 801, 600), (2.0, 1602, 1200)] {
            let mut raw = common.clone();
            raw["pointPixelScale"] = serde_json::json!(scale);
            raw["sourcePixels"] = serde_json::json!({"width":width, "height":height});
            let display = DisplayInfo::from_helper(raw.clone()).unwrap();
            assert_eq!(serde_json::to_value(display).unwrap(), raw);
        }
        for (scale, size) in [
            (serde_json::Value::Null, serde_json::json!({"width":801, "height":600})),
            (serde_json::json!(1), serde_json::Value::Null),
            (serde_json::json!(0), serde_json::json!({"width":801, "height":600})),
            (serde_json::json!(1), serde_json::json!({"width":0, "height":600})),
        ] {
            let mut raw = common.clone();
            raw["pointPixelScale"] = scale;
            raw["sourcePixels"] = size;
            assert_eq!(serde_json::to_value(DisplayInfo::from_helper(raw).unwrap()).unwrap(), common);
        }
        assert_eq!(serde_json::to_value(DisplayInfo::from_helper(common.clone()).unwrap()).unwrap(), common);
    }

    #[test]
    fn display_discovery_is_explicitly_non_prompting_without_running_helper() {
        let command = display_list_command(Path::new("fixture-helper-not-executed"));
        assert_eq!(command.get_program(), "fixture-helper-not-executed");
        assert_eq!(command.get_args().collect::<Vec<_>>(), vec!["list", "--no-permission-prompt"]);
    }
}

pub(crate) fn helper_path(app: &AppHandle) -> Result<PathBuf, AppMessage> {
    if let Ok(value) = std::env::var("CAPTURE_SCRIBE_CAPTURE_HELPER") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "CAPTURE_SCRIBE_CAPTURE_HELPER does not exist: {}",
            path.display()
        )
        .into());
    }

    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let sidecar = directory.join("capture-scribe-capture");
            if sidecar.is_file() {
                return Ok(sidecar);
            }
        }
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        let bundled = resource_dir.join("bin/capture-scribe-capture");
        if bundled.is_file() {
            return Ok(bundled);
        }
    }

    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../native/macos-capture-helper/.build/release/capture-scribe-capture");
    if development.is_file() {
        return Ok(development);
    }
    Err(format!(
        "macOS capture helper is not built. Run: swift build -c release --package-path native/macos-capture-helper (expected {})",
        development.display()
    ).into())
}
