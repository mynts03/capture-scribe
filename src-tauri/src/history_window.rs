//! App-lifetime catalogue/selection only; no settings writer, worker or queue.
use crate::{history::RecordingHistoryResult, message::AppMessage};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryWindowSnapshot {
    pub session_id: String,
    pub revision: u64,
    pub selection_revision: u64,
    pub selected_ids: Vec<String>,
    pub selection_explicit: bool,
    pub language: String,
    pub catalog: Option<RecordingHistoryResult>,
    pub focus_process_revision: u64,
}
struct Inner { snapshot: HistoryWindowSnapshot, refresh_generation: u64 }
#[derive(Clone)]
pub struct HistoryWindowState { inner: Arc<Mutex<Inner>>, opening: Arc<Mutex<()>> }
impl Default for HistoryWindowState {
    fn default() -> Self {
        Self { inner: Arc::new(Mutex::new(Inner {
            snapshot: HistoryWindowSnapshot { session_id: crate::history_index::token(), revision: 0,
                selection_revision: 0, selected_ids: vec![], selection_explicit: false,
                language: "ja".into(), catalog: None, focus_process_revision: 0 }, refresh_generation: 0,
        })), opening: Arc::new(Mutex::new(())) }
    }
}
fn problem(raw: impl Into<String>) -> AppMessage { AppMessage::known("history.selection_changed", raw) }
pub fn selectable(item: &crate::history::RecordingHistoryItem) -> bool {
    item.processing_available && item.source_master_available && !item.snapshot_stale && item.manifest_state == "available"
}
impl HistoryWindowState {
    pub fn snapshot(&self) -> HistoryWindowSnapshot { self.inner.lock().unwrap().snapshot.clone() }
    #[cfg(target_os = "macos")]
    pub(crate) fn current_language(&self) -> String {
        self.inner.lock().unwrap().snapshot.language.clone()
    }
    pub fn begin_refresh(&self) -> u64 { let mut s = self.inner.lock().unwrap(); s.refresh_generation += 1; s.refresh_generation }
    pub fn install_catalog(&self, generation: u64, catalog: RecordingHistoryResult) -> bool {
        let mut s = self.inner.lock().unwrap();
        if generation != s.refresh_generation { return false; }
        // Offline/missing selections remain selected; never silently substitute another recording.
        s.snapshot.catalog = Some(catalog); s.snapshot.revision += 1; true
    }
    fn select(&self, ids: Vec<String>, selected: bool, replace: bool) -> Result<HistoryWindowSnapshot, AppMessage> {
        if ids.len() > 10_000 { return Err(problem("Too many selection changes")); }
        let mut s = self.inner.lock().unwrap();
        if selected && ids.iter().any(|id| !s.snapshot.catalog.as_ref().is_some_and(|c| c.items.iter().any(|item| &item.record_id == id && selectable(item)))) {
            return Err(problem("A selected recording is no longer available for processing"));
        }
        if replace { s.snapshot.selected_ids.clear(); }
        for id in ids {
            if selected { if !s.snapshot.selected_ids.contains(&id) { s.snapshot.selected_ids.push(id); } }
            else { s.snapshot.selected_ids.retain(|old| old != &id); }
        }
        s.snapshot.selection_explicit = true; s.snapshot.selection_revision += 1; s.snapshot.revision += 1;
        Ok(s.snapshot.clone())
    }
    pub fn check_target(&self, revision: u64, manifest: &str) -> Result<(), AppMessage> {
        let s = self.inner.lock().unwrap();
        if s.snapshot.selection_revision != revision { return Err(problem("Selection changed before processing was accepted")); }
        if s.snapshot.selection_explicit && (s.snapshot.selected_ids.len() != 1 || !s.snapshot.catalog.as_ref().is_some_and(|c| c.items.iter().any(|item|
            s.snapshot.selected_ids.contains(&item.record_id) && item.manifest_path == manifest && selectable(item)))) {
            return Err(problem("Exactly one available selected recording is required"));
        }
        Ok(())
    }
    fn language(&self, language: String) -> Result<HistoryWindowSnapshot, AppMessage> {
        if !matches!(language.as_str(), "ja" | "en") { return Err(problem("Unsupported display language")); }
        let mut s = self.inner.lock().unwrap();
        if s.snapshot.language != language { s.snapshot.language = language; s.snapshot.revision += 1; }
        Ok(s.snapshot.clone())
    }
    pub fn publish(&self, app: &AppHandle) { let _ = app.emit("history-window-state", self.snapshot()); }
}
fn trusted(window: &WebviewWindow) -> Result<(), AppMessage> {
    if matches!(window.label(), "main" | "history") { Ok(()) } else { Err(problem("Unsupported window")) }
}
#[tauri::command]
pub fn history_window_snapshot(window: WebviewWindow, state: tauri::State<'_, HistoryWindowState>) -> Result<HistoryWindowSnapshot, AppMessage> {
    trusted(&window)?; Ok(state.snapshot())
}
#[tauri::command]
pub fn set_history_selection(app: AppHandle, window: WebviewWindow, state: tauri::State<'_, HistoryWindowState>, record_ids: Vec<String>, selected: bool, replace: Option<bool>) -> Result<HistoryWindowSnapshot, AppMessage> {
    trusted(&window)?; let result = state.select(record_ids, selected, replace.unwrap_or(false))?; state.publish(&app); Ok(result)
}
#[tauri::command]
pub fn set_history_language(app: AppHandle, window: WebviewWindow, state: tauri::State<'_, HistoryWindowState>, language: String) -> Result<HistoryWindowSnapshot, AppMessage> {
    trusted(&window)?;
    let result = state.language(language)?;
    state.publish(&app);
    #[cfg(target_os = "macos")]
    crate::automation_help::refresh_language(&app);
    Ok(result)
}
#[tauri::command]
pub async fn open_recording_history(app: AppHandle, window: WebviewWindow) -> Result<(), AppMessage> {
    trusted(&window)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<HistoryWindowState>(); let _opening = state.opening.lock().unwrap();
        let history = match app.get_webview_window("history") {
            Some(window) => window,
            None => WebviewWindowBuilder::new(&app, "history", WebviewUrl::App("index.html?window=history".into()))
                .title("CaptureScribe").inner_size(900.0, 720.0).min_inner_size(600.0, 650.0)
                .build().map_err(|e| problem(e.to_string()))?,
        };
        history.show().and_then(|_| history.unminimize()).and_then(|_| history.set_focus()).map_err(|e| problem(e.to_string()))
    }).await.map_err(|e| problem(e.to_string()))?
}
#[tauri::command]
pub fn open_processing_settings(app: AppHandle, window: WebviewWindow, state: tauri::State<'_, HistoryWindowState>) -> Result<(), AppMessage> {
    trusted(&window)?;
    let main = app.get_webview_window("main").ok_or_else(|| problem("Main window unavailable"))?;
    main.show().and_then(|_| main.unminimize()).and_then(|_| main.set_focus()).map_err(|e| problem(e.to_string()))?;
    { let mut s = state.inner.lock().unwrap(); s.snapshot.focus_process_revision += 1; s.snapshot.revision += 1; }
    state.publish(&app); Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog() -> RecordingHistoryResult {
        let item = |id: &str, available: bool| crate::history::RecordingHistoryItem {
            record_id: id.into(), job_id: id.into(), recorded_at_unix_ms: Some(1), manifest_state: "available".into(),
            snapshot_stale: false, last_confirmed_unix_ms: 1, artifacts: vec![], processing_available: available,
            manifest_path: format!("/fixture/{id}.json"), name: id.into(), source_master_path: format!("/fixture/{id}.mp4"),
            source_master_available: available, duration_seconds: 1.0, processing_status: Some("failed".into()),
            processing_runs: vec![], transcription: None, warning_count: 0, warnings: vec![], transcript_count: 0,
            user_output_count: 0, modified_at_unix_ms: 1,
        };
        RecordingHistoryResult { items: vec![item("a", true), item("b", true), item("missing", false)], warnings: vec![], capture_failures: vec![], processing_run_updates: vec![] }
    }
    #[test]
    fn availability_not_result_controls_selection_and_hidden_ids_survive() {
        let state = HistoryWindowState::default(); let ticket = state.begin_refresh(); state.install_catalog(ticket, catalog());
        state.select(vec!["a".into(), "b".into()], true, false).unwrap();
        assert!(state.select(vec!["missing".into()], true, false).is_err());
        assert!(state.select(vec!["invented".into()], true, false).is_err());
        let s = state.select(vec!["a".into()], false, false).unwrap(); assert_eq!(s.selected_ids, vec!["b"]);
        assert!(state.check_target(s.selection_revision, "/fixture/a.json").is_err());
        assert!(state.check_target(s.selection_revision, "/fixture/b.json").is_ok());
        assert!(state.check_target(s.selection_revision - 1, "/fixture/b.json").is_err());
        let s = state.select(vec!["a".into()], true, false).unwrap();
        assert!(state.check_target(s.selection_revision, "/fixture/a.json").is_err());
        state.select(vec!["a".into()], false, false).unwrap();
        let ticket = state.begin_refresh(); let mut c = catalog(); c.items.clear(); state.install_catalog(ticket, c);
        assert_eq!(state.snapshot().selected_ids, vec!["b"]);
        assert!(state.check_target(state.snapshot().selection_revision, "/fixture/b.json").is_err());
    }
    #[test]
    fn late_catalogues_cannot_undo_newer_data_or_selection() {
        let state = HistoryWindowState::default(); let old = state.begin_refresh(); let new = state.begin_refresh();
        assert!(state.install_catalog(new, catalog())); state.select(vec!["a".into()], true, false).unwrap();
        assert!(!state.install_catalog(old, RecordingHistoryResult { items: vec![], ..catalog() }));
        assert_eq!(state.snapshot().selected_ids, vec!["a"]); assert_eq!(state.snapshot().catalog.unwrap().items.len(), 3);
        assert!(state.language("invalid".into()).is_err()); assert_eq!(state.language("en".into()).unwrap().language, "en");
    }
}
