//! Common spatial contract. Native screen IDs, Retina and DPI conversion belong to the adapter.
use crate::message::AppMessage;
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::AppHandle;

pub static SELECTING: AtomicBool = AtomicBool::new(false);
struct SelectionFlag<'a>(&'a AtomicBool);
impl<'a> SelectionFlag<'a> {
    fn acquire(flag: &'a AtomicBool) -> Result<Self, AppMessage> {
        if flag.swap(true, Ordering::SeqCst) {
            return Err(AppMessage::known("capture.selection_failed", "A region selector is already open"));
        }
        Ok(Self(flag))
    }
}
impl Drop for SelectionFlag<'_> {
    fn drop(&mut self) { self.0.store(false, Ordering::SeqCst); }
}
pub static AREA_OPERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
// Only output sizes actually published after SCK/report/media validation in this app run.
// No static list derived from VideoToolbox or from another Mac is advertised as verified.
static VERIFIED_OUTPUTS: std::sync::Mutex<Vec<OutputSize>> = std::sync::Mutex::new(Vec::new());
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputSize {
    pub width: u64,
    pub height: u64,
}
pub fn remember_output(size: OutputSize) {
    if size.width < 16
        || size.height < 16
        || size.width > 32768
        || size.height > 32768
        || size.width % 2 != 0
        || size.height % 2 != 0
    {
        return;
    }
    if let Ok(mut outputs) = VERIFIED_OUTPUTS.lock() {
        if !outputs.contains(&size) {
            outputs.push(size);
        }
    }
}
pub fn output_verified(size: OutputSize) -> bool {
    VERIFIED_OUTPUTS
        .lock()
        .map(|v| v.contains(&size))
        .unwrap_or(false)
}
#[tauri::command]
pub fn capture_output_alternatives(width: u64, height: u64) -> Vec<OutputSize> {
    let mut values = VERIFIED_OUTPUTS
        .lock()
        .map(|v| {
            v.iter()
                .copied()
                .filter(|s| {
                    s.width <= width && s.height <= height && (s.width < width || s.height < height)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    values.sort_by_key(|s| std::cmp::Reverse(s.width * s.height));
    values
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub origin: String,
    pub unit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CaptureArea {
    pub mode: String,
    pub display_ref: String,
    pub snapshot_token: String,
    pub region: Option<Region>,
}
impl CaptureArea {
    pub fn validate(&self, width: f64, height: f64) -> Result<(), AppMessage> {
        let valid = self.mode == "display" && self.region.is_none()
            || self.mode == "region"
                && self.region.as_ref().is_some_and(|r| {
                    r.origin == "top-left"
                        && r.unit == "display-logical"
                        && [r.x, r.y, r.width, r.height].iter().all(|v| v.is_finite())
                        && r.x >= 0.0
                        && r.y >= 0.0
                        && r.width >= 16.0
                        && r.height >= 16.0
                        && r.x + r.width <= width
                        && r.y + r.height <= height
                });
        if valid && !self.display_ref.is_empty() && !self.snapshot_token.is_empty() {
            Ok(())
        } else {
            Err(AppMessage::known(
                "capture.region_invalid",
                "Invalid region, unit or display snapshot",
            ))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectionRequest {
    protocol_version: u8,
    request_id: String,
    display_id: u32,
    display_ref: String,
    snapshot_token: String,
    initial_mode: String,
    initial_region: Option<Region>,
    language: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectionResponse {
    protocol_version: u8,
    request_id: String,
    status: String,
    area: Option<CaptureArea>,
    raw: Option<String>,
}
fn decode_response(
    bytes: &[u8],
    request: &SelectionRequest,
) -> Result<SelectionResponse, AppMessage> {
    let response: SelectionResponse = serde_json::from_slice(bytes)
        .map_err(|e| AppMessage::known("capture.selection_failed", e.to_string()))?;
    if request.protocol_version != 2 || response.protocol_version != 2
        || response.request_id != request.request_id
        || !matches!(
            response.status.as_str(),
            "confirmed" | "cancelled" | "invalidated" | "failed"
        )
        || (response.status == "confirmed") != response.area.is_some()
        || response.area.as_ref().is_some_and(|a| {
            !matches!((a.mode.as_str(), a.region.is_some()), ("display", false) | ("region", true))
                || a.display_ref != request.display_ref
                || a.snapshot_token != request.snapshot_token
        })
    {
        return Err(AppMessage::known(
            "capture.selection_failed",
            "Mismatched selection response",
        ));
    }
    Ok(response)
}

#[derive(Clone, Copy)]
struct SelectionWindowState {
    visible: bool,
    minimized: bool,
    focused: bool,
}
// A private seam for off-screen tests; no second window/state service.
trait SelectionWindowControl {
    fn selection_state(&self) -> Result<SelectionWindowState, AppMessage>;
    fn selection_hide(&self) -> Result<(), AppMessage>;
    fn selection_show(&self) -> Result<(), AppMessage>;
    fn selection_focus(&self) -> Result<(), AppMessage>;
}
fn selection_window_error(error: tauri::Error) -> AppMessage {
    AppMessage::known("capture.selection_failed", format!("Selection window: {error}"))
}
impl SelectionWindowControl for tauri::WebviewWindow {
    fn selection_state(&self) -> Result<SelectionWindowState, AppMessage> {
        Ok(SelectionWindowState {
            visible: self.is_visible().map_err(selection_window_error)?,
            minimized: self.is_minimized().map_err(selection_window_error)?,
            focused: self.is_focused().map_err(selection_window_error)?,
        })
    }
    fn selection_hide(&self) -> Result<(), AppMessage> { self.hide().map_err(selection_window_error) }
    fn selection_show(&self) -> Result<(), AppMessage> { self.show().map_err(selection_window_error) }
    fn selection_focus(&self) -> Result<(), AppMessage> { self.set_focus().map_err(selection_window_error) }
}
struct SelectionWindows<W: SelectionWindowControl> {
    windows: Vec<(W, SelectionWindowState, bool)>,
    restored: bool,
}
impl<W: SelectionWindowControl> SelectionWindows<W> {
    fn hide(windows: impl IntoIterator<Item = W>) -> Result<Self, AppMessage> {
        // Snapshot every window BEFORE hiding any: hiding the key window can
        // change which remaining window is focused. Minimized/hidden stay so.
        let windows = windows.into_iter().map(|window| {
            let state = window.selection_state()?;
            Ok((window, state, false))
        }).collect::<Result<Vec<_>, AppMessage>>()?;
        let mut guard = Self { windows, restored: false };
        for (window, state, hidden) in &mut guard.windows {
            if state.visible && !state.minimized {
                // Include this window in rollback even if hide partially fails.
                *hidden = true;
                window.selection_hide()?;
            }
        }
        Ok(guard)
    }
    fn restore(&mut self) -> Result<(), AppMessage> {
        let mut error = None;
        // Attempt ALL windows even if one OS call fails; never recreate/close a
        // webview or change the history selection / React form state.
        for (window, _, hidden) in &mut self.windows {
            if *hidden {
                match window.selection_show() {
                    Ok(()) => *hidden = false,
                    Err(e) => { error.get_or_insert(e); }
                }
            }
        }
        for (window, state, hidden) in &self.windows {
            if state.visible && !state.minimized && state.focused && !hidden {
                if let Err(e) = window.selection_focus() { error.get_or_insert(e); }
            }
        }
        self.restored = error.is_none();
        error.map_or(Ok(()), Err)
    }
}
impl<W: SelectionWindowControl> Drop for SelectionWindows<W> {
    fn drop(&mut self) {
        if !self.restored {
            // Also runs on helper errors, malformed responses and unwinding.
            if let Err(e) = self.restore() { eprintln!("Selection window restore failed: {e}"); }
        }
    }
}
fn with_selection_windows_tracked<W: SelectionWindowControl, T>(
    windows: impl IntoIterator<Item = W>, operation: impl FnOnce() -> Result<T, AppMessage>,
    restored: impl FnOnce(bool),
) -> Result<T, AppMessage> {
    let mut guard = SelectionWindows::hide(windows)?;
    let result = operation();
    let restore_result = guard.restore();
    restored(restore_result.is_ok());
    match (result, restore_result) {
        (result, Ok(())) => result,
        (Ok(_), Err(e)) => Err(e),
        (Err(mut original), Err(restore)) => {
            original.raw = Some(format!("{}; window restoration: {restore}", original));
            Err(original)
        }
    }
}

#[cfg(test)]
fn with_selection_windows_hidden<W: SelectionWindowControl, T>(
    windows: impl IntoIterator<Item = W>, operation: impl FnOnce() -> Result<T, AppMessage>,
) -> Result<T, AppMessage> {
    with_selection_windows_tracked(windows, operation, |_| {})
}

#[tauri::command]
pub async fn select_capture_region(
    app: AppHandle,
    request: SelectionRequest,
) -> Result<SelectionResponse, AppMessage> {
    let reset = SelectionFlag::acquire(&SELECTING)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _reset = reset;
        let _operation = AREA_OPERATION.try_lock().map_err(|_| {
            AppMessage::known(
                "capture.selection_failed",
                "A capture operation is already preparing",
            )
        })?;
        if request.protocol_version != 2
            || request.request_id.is_empty()
            || !matches!(request.initial_mode.as_str(), "display" | "region")
            || !matches!(request.language.as_str(), "ja" | "en")
        {
            return Err(AppMessage::known(
                "capture.selection_failed",
                "Invalid selection request",
            ));
        }
        // Reject selection while a recording is preparing, running or finalizing.
        use tauri::Manager;
        let _lease=app.state::<crate::operation_gate::OperationGate>().acquire(crate::operation_gate::Owner::Selection)?;
        if app.state::<crate::capture::CaptureManager>().busy() {
            return Err(AppMessage::known(
                "capture.already_recording",
                "Capture is busy",
            ));
        }
        let helper = crate::capture_engine::helper_path(&app)?;
        // Only Tauri-owned product webviews are hidden. The Swift selector is
        // a separate helper process and keeps its own visible input surface.
        let manager = app.state::<crate::selection_session::SelectionManager>().inner().clone();
        let mut session = manager.begin(&request.request_id).map_err(|e| AppMessage::known("capture.selection_failed", e))?;
        let restored = std::cell::Cell::new(false);
        let result = with_selection_windows_tracked(app.webview_windows().into_values(), || {
        let mut command = Command::new(helper);
        command.arg("select-region");
        let payload = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        let output = crate::selection_session::run_helper(command, &payload, &session, Duration::from_secs(610))
            .map_err(|e| AppMessage::known("capture.selection_failed", e))?;
        if std::env::var("CAPTURE_SCRIBE_SELECTION_DIAGNOSTICS").as_deref() == Ok("1") {
            eprint!("{}", String::from_utf8_lossy(&output.diagnostic));
        }
        let mut response = decode_response(&output.bytes, &request)?;
        if output.cancel_won {
            response.status = "cancelled".into(); response.area = None;
            response.raw = Some("Parent cancellation accepted".into());
        }
        if let Some(area) = &response.area {
            let displays = crate::capture_engine::selected_engine(&app)?.list_displays()?;
            let display = displays
                .iter()
                .find(|d| d.id == request.display_id)
                .ok_or_else(|| {
                    AppMessage::known("capture.region_invalid", "Display disappeared")
                })?;
            if display.display_ref != area.display_ref
                || display.snapshot_token != area.snapshot_token
            {
                return Err(AppMessage::known(
                    "capture.region_invalid",
                    "Display changed after selection",
                ));
            }
            area.validate(display.width_points as f64, display.height_points as f64)?;
        }
        Ok(response)
        }, |ok| restored.set(ok));
        // Completion includes window restoration and release of the product gates.
        drop(_lease); drop(_operation);
        drop(_reset);
        let outcome = result.as_ref().map(|r| r.status.as_str()).unwrap_or("failed");
        session.finish(outcome, restored.get());
        result
    })
    .await
    .map_err(|e| AppMessage::from(e.to_string()))?;
    result
}

#[tauri::command]
pub fn selection_status(manager: tauri::State<'_, crate::selection_session::SelectionManager>) -> crate::selection_session::SelectionSnapshot {
    manager.snapshot()
}
#[tauri::command]
pub fn cancel_selection(manager: tauri::State<'_, crate::selection_session::SelectionManager>, request_id: String) -> Result<crate::selection_session::CancelReceipt, String> {
    manager.cancel(&request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone)]
    struct FakeWindow(std::sync::Arc<std::sync::Mutex<FakeWindowData>>);
    struct FakeWindowData {
        state: SelectionWindowState,
        calls: Vec<&'static str>,
        fail_snapshot: bool,
        fail_hide: bool,
        fail_show: bool,
    }
    impl FakeWindow {
        fn new(visible: bool, minimized: bool, focused: bool) -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(FakeWindowData {
                state: SelectionWindowState { visible, minimized, focused },
                calls: vec![], fail_snapshot: false, fail_hide: false, fail_show: false,
            })))
        }
        fn calls(&self) -> Vec<&'static str> { self.0.lock().unwrap().calls.clone() }
        fn visible(&self) -> bool { self.0.lock().unwrap().state.visible }
    }
    impl SelectionWindowControl for FakeWindow {
        fn selection_state(&self) -> Result<SelectionWindowState, AppMessage> {
            let mut w = self.0.lock().unwrap(); w.calls.push("snapshot");
            if w.fail_snapshot { return Err(AppMessage::known("capture.selection_failed", "snapshot")); }
            Ok(w.state)
        }
        fn selection_hide(&self) -> Result<(), AppMessage> {
            let mut w = self.0.lock().unwrap(); w.calls.push("hide");
            w.state.visible = false; w.state.focused = false;
            if w.fail_hide { return Err(AppMessage::known("capture.selection_failed", "hide")); }
            Ok(())
        }
        fn selection_show(&self) -> Result<(), AppMessage> {
            let mut w = self.0.lock().unwrap(); w.calls.push("show");
            if w.fail_show { return Err(AppMessage::known("capture.selection_failed", "show")); }
            w.state.visible = true; Ok(())
        }
        fn selection_focus(&self) -> Result<(), AppMessage> {
            let mut w = self.0.lock().unwrap(); w.calls.push("focus"); w.state.focused = true; Ok(())
        }
    }
    #[test]
    fn selection_flag_guard_releases_on_failure_and_cannot_clear_another_owner() {
        let flag = AtomicBool::new(false);
        {
            let _owner = SelectionFlag::acquire(&flag).unwrap();
            assert!(SelectionFlag::acquire(&flag).is_err());
            assert!(flag.load(Ordering::SeqCst));
        }
        assert!(!flag.load(Ordering::SeqCst));
        let _ = std::panic::catch_unwind(|| { let _owner = SelectionFlag::acquire(&flag).unwrap(); panic!("fixture"); });
        assert!(!flag.load(Ordering::SeqCst));
        assert!(SelectionFlag::acquire(&flag).is_ok());
    }

    #[test]
    fn shared_session_completion_follows_window_restoration_and_flag_cleanup() {
        let manager = crate::selection_session::SelectionManager::default();
        let mut session = manager.begin("gui-fixture").unwrap();
        let window = FakeWindow::new(true, false, true);
        let restored = std::cell::Cell::new(false);
        let response = with_selection_windows_tracked([window.clone()], || {
            assert!(!window.visible()); session.started(123);
            let receipt = manager.cancel("gui-fixture").unwrap();
            assert!(!receipt.completed); assert!(session.helper_finished());
            assert_eq!(manager.snapshot().phase, "restoring"); Ok("cancelled")
        }, |ok| restored.set(ok)).unwrap();
        assert!(window.visible()); session.finish(response, restored.get());
        assert_eq!(manager.snapshot().windows_restored, Some(true));
        assert_eq!(manager.snapshot().phase, "finished");
    }

    #[test]
    fn selection_hides_product_windows_and_restores_original_history_focus_for_all_responses() {
        for status in ["confirmed", "cancelled", "invalidated", "failed"] {
            let main = FakeWindow::new(true, false, false);
            let history = FakeWindow::new(true, false, true);
            let help = FakeWindow::new(true, false, false);
            let result = with_selection_windows_hidden([main.clone(), history.clone(), help.clone()], || {
                assert!(!main.visible() && !history.visible() && !help.visible());
                assert_eq!(history.calls(), vec!["snapshot", "hide"]);
                Ok(status)
            }).unwrap();
            assert_eq!(result, status);
            assert_eq!(main.calls(), vec!["snapshot", "hide", "show"]);
            assert_eq!(history.calls(), vec!["snapshot", "hide", "show", "focus"]);
            assert_eq!(help.calls(), vec!["snapshot", "hide", "show"]);
            assert!(main.visible() && history.visible() && help.visible());
        }
    }
    #[test]
    fn selection_keeps_hidden_and_minimized_windows_unchanged() {
        let hidden = FakeWindow::new(false, false, false);
        let minimized = FakeWindow::new(true, true, false);
        with_selection_windows_hidden([hidden.clone(), minimized.clone()], || Ok(())).unwrap();
        assert_eq!(hidden.calls(), vec!["snapshot"]);
        assert_eq!(minimized.calls(), vec!["snapshot"]);
        assert!(!hidden.visible());
        assert!(minimized.0.lock().unwrap().state.minimized);
    }
    #[test]
    fn selection_restores_on_spawn_protocol_timeout_errors_and_panic() {
        // The real helper paths all return through the same guarded closure.
        for raw in ["spawn", "write", "bad response", "nonzero exit", "timeout", "display invalidated"] {
            let w = FakeWindow::new(true, false, true);
            let result: Result<(), _> = with_selection_windows_hidden([w.clone()], || {
                Err(AppMessage::known("capture.selection_failed", raw))
            });
            assert_eq!(result.unwrap_err().raw.as_deref(), Some(raw));
            assert_eq!(w.calls(), vec!["snapshot", "hide", "show", "focus"]);
        }
        let w = FakeWindow::new(true, false, true);
        let result = std::panic::catch_unwind({ let w = w.clone(); move || {
            let _: Result<(), _> = with_selection_windows_hidden([w], || panic!("isolated unwind"));
        }});
        assert!(result.is_err()); assert!(w.visible());
        assert!(w.0.lock().unwrap().state.focused);
    }
    #[test]
    fn selection_rolls_back_partial_hide_and_never_starts_helper_if_snapshot_fails() {
        let a = FakeWindow::new(true, false, true); let b = FakeWindow::new(true, false, false);
        b.0.lock().unwrap().fail_hide = true;
        let result = with_selection_windows_hidden([a.clone(), b.clone()], || -> Result<(), AppMessage> {
            panic!("must not launch helper")
        });
        assert!(result.is_err()); assert!(a.visible() && b.visible());
        let c = FakeWindow::new(true, false, false); c.0.lock().unwrap().fail_snapshot = true;
        let a = FakeWindow::new(true, false, true);
        assert!(with_selection_windows_hidden([a.clone(), c], || Ok(())).is_err());
        assert_eq!(a.calls(), vec!["snapshot"]);
    }
    #[test]
    fn selection_reports_restore_failure_and_attempts_every_other_window() {
        let a = FakeWindow::new(true, false, false); let b = FakeWindow::new(true, false, true);
        a.0.lock().unwrap().fail_show = true;
        let error = with_selection_windows_hidden([a.clone(), b.clone()], || Ok(())).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("capture.selection_failed"));
        assert!(b.visible()); assert!(b.calls().contains(&"focus"));
        assert!(a.calls().iter().filter(|c| **c == "show").count() >= 2); // Drop retries.
    }
    #[test]
    fn alternatives_come_only_from_successful_sck_outputs_not_static_guesses() {
        let size = OutputSize {
            width: 640,
            height: 480,
        };
        remember_output(size);
        assert!(output_verified(size));
        assert!(!output_verified(OutputSize {
            width: 638,
            height: 478
        }));
        assert!(capture_output_alternatives(5120, 2880).contains(&size));
        assert!(!capture_output_alternatives(640, 480).contains(&size));
    }
    fn area() -> CaptureArea {
        CaptureArea {
            mode: "region".into(),
            display_ref: "runtime-ref".into(),
            snapshot_token: "s".into(),
            region: Some(Region {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 50.0,
                origin: "top-left".into(),
                unit: "display-logical".into(),
            }),
        }
    }
    #[test]
    fn regions_reject_nonfinite_outside_oversize_and_wrong_units() {
        let good = area();
        assert!(good.validate(100.0, 50.0).is_ok());
        for (x, w) in [(-1.0, 100.0), (0.0, 101.0), (f64::NAN, 100.0), (0.0, 0.0)] {
            let mut a = area();
            a.region.as_mut().unwrap().x = x;
            a.region.as_mut().unwrap().width = w;
            assert!(a.validate(100.0, 50.0).is_err());
        }
        let mut a = area();
        a.region.as_mut().unwrap().unit = "css-pixel".into();
        assert!(a.validate(100.0, 50.0).is_err());
    }
    #[test]
    fn selection_requires_matching_request_and_explicit_confirmation() {
        let r = SelectionRequest {
            protocol_version: 2,
            request_id: "r".into(),
            display_id: 1,
            display_ref: "runtime-ref".into(),
            snapshot_token: "s".into(),
            initial_mode: "display".into(),
            initial_region: None,
            language: "ja".into(),
        };
        let mut response = serde_json::json!({"protocolVersion":2,"requestId":"r","status":"cancelled","area":null,"raw":null});
        assert!(decode_response(response.to_string().as_bytes(), &r).is_ok());
        response["status"] = "confirmed".into();
        assert!(decode_response(response.to_string().as_bytes(), &r).is_err());
        response["area"] = serde_json::to_value(area()).unwrap();
        assert!(decode_response(response.to_string().as_bytes(), &r).is_ok());
        response["requestId"] = "other".into();
        assert!(decode_response(response.to_string().as_bytes(), &r).is_err());
    }
    #[test]
    fn selection_v2_accepts_explicit_full_display_but_rejects_mixed_versions_modes_and_snapshots() {
        let request = SelectionRequest { protocol_version:2, request_id:"r".into(), display_id:1,
            display_ref:"runtime-ref".into(), snapshot_token:"s".into(), initial_mode:"region".into(), initial_region:area().region, language:"en".into() };
        let mut full=area();full.mode="display".into();full.region=None;
        assert!(full.validate(100.0,50.0).is_ok());
        let good=serde_json::json!({"protocolVersion":2,"requestId":"r","status":"confirmed","area":full,"raw":null});
        assert!(decode_response(good.to_string().as_bytes(),&request).is_ok());
        for (key,value) in [("protocolVersion",serde_json::json!(1)),("requestId",serde_json::json!("old")),("status",serde_json::json!("cancelled"))] {
            let mut bad=good.clone();bad[key]=value;
            assert!(decode_response(bad.to_string().as_bytes(),&request).is_err());
        }
        for (key,value) in [("mode",serde_json::json!("region")),("mode",serde_json::json!("other")),("displayRef",serde_json::json!("other")),("snapshotToken",serde_json::json!("old")),("region",serde_json::to_value(area().region).unwrap())] {
            let mut bad=good.clone();bad["area"][key]=value;
            assert!(decode_response(bad.to_string().as_bytes(),&request).is_err());
        }
        let mut old=request.clone();old.protocol_version=1;
        assert!(decode_response(good.to_string().as_bytes(),&old).is_err());
        let mut missing=serde_json::to_value(&request).unwrap();missing.as_object_mut().unwrap().remove("initialMode");
        assert!(serde_json::from_value::<SelectionRequest>(missing).is_err());
    }
}
