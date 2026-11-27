pub mod control;
pub mod capture_target;
mod control_permissions;
pub mod selection_session;
mod capture;
pub use capture::CaptureManager;
mod capture_failures;
mod capture_area;
mod capture_engine;
mod capture_process;
mod filesystem;
mod history;
mod history_window;
mod history_index;
mod message;
mod postprocess;
mod operation_gate;
mod owned_worker;
mod processing_manager;
mod app_exit;
mod processing_runs;
mod recording_manifest;
mod recording_names;
mod save_destination;

#[cfg(target_os = "macos")]
mod automation_help;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let gate=operation_gate::OperationGate::default();
    let selection = selection_session::SelectionManager::default();
    let capture = capture::CaptureManager::default();
    let targets = capture_target::TargetControl::new(capture_target::CaptureTargetManager::default(), gate.clone(), selection.clone(), capture.clone());
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(capture)
        .manage(targets)
        .manage(history_window::HistoryWindowState::default())
        .manage(processing_manager::ProcessingManager::new(gate.clone()))
        .manage(app_exit::ExitManager::new(gate.clone()))
        .manage(gate)
        .manage(selection)
        .setup(|app| {
            use tauri::{Manager, Emitter};
            let handle=app.handle().clone();
            app.state::<capture_target::TargetControl>().bind_notifications(move |revision| {
                let _=handle.emit("temporary-capture-target-changed",serde_json::json!({"revision":revision}));
            });
            // Disabled unless a fresh, explicit, user-private directory is supplied.
            if let Some(directory) = std::env::var_os("CAPTURE_SCRIBE_CONTROL_DIR") {
                let server = control::ControlServer::start(
                    std::path::Path::new(&directory), &std::env::current_exe()?,
                    app.state::<selection_session::SelectionManager>().inner().clone(),
                    app.state::<capture_target::TargetControl>().inner().clone(),
                    app.state::<capture::CaptureManager>().inner().clone(),
                )?;
                app.manage(std::sync::Mutex::new(server));
            }
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{Menu, MenuItem};
                // The native predefined Quit uses NSApplication.terminate:
                // and bypasses Tauri's prevent_exit. Preserve the default
                // menu, replacing only its final app-menu Quit action.
                let menu = Menu::default(app.handle())?;
                let items = menu.items()?;
                let app_menu = items
                    .first()
                    .and_then(|item| item.as_submenu())
                    .ok_or("default macOS app menu is unavailable")?;
                let app_items = app_menu.items()?;
                let quit = app_items
                    .last()
                    .and_then(|item| item.as_predefined_menuitem())
                    .ok_or("default macOS Quit menu item is unavailable")?;
                let safe_quit = MenuItem::with_id(
                    app,
                    "capture-safe-quit",
                    quit.text()?,
                    true,
                    Some("CmdOrCtrl+Q"),
                )?;
                app_menu.remove(quit)?;
                app_menu.append(&safe_quit)?;
                app.set_menu(menu)?;
                if let Some(menu) = app.menu() {
                    automation_help::add_menu_item(app.handle(), &menu);
                }
            }
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "capture-safe-quit" {
                // Goes through ExitRequested and the same capture finalizer.
                app.exit(0);
            }
            #[cfg(target_os = "macos")]
            if event.id().as_ref() == automation_help::MENU_ID {
                automation_help::open(app);
            }
        })
        .invoke_handler({
            let commands: fn(tauri::ipc::Invoke<tauri::Wry>) -> bool = tauri::generate_handler![
                capture::default_output_directory,
                capture::list_displays,
                capture_target::temporary_capture_target_status,
                capture_target::clear_temporary_capture_target,
                capture_area::select_capture_region,
                capture_area::selection_status,
                capture_area::cancel_selection,
                capture_area::capture_output_alternatives,
                capture::capture_status,
                capture::last_capture_result,
                capture::last_capture_failure,
                capture::start_capture,
                capture::stop_capture,
                filesystem::reveal_path,
                save_destination::preview_save_directory,
                save_destination::validate_file_name,
                history_index::list_recording_history,
                history_window::history_window_snapshot,
                history_window::set_history_selection,
                history_window::set_history_language,
                history_window::open_recording_history,
                history_window::open_processing_settings,
                postprocess::process_recording,
                processing_manager::processing_snapshot,
                processing_manager::stop_processing,
                app_exit::app_exit_snapshot,
                app_exit::choose_app_exit,
                postprocess::transcription_languages,
            ];
            move |invoke: tauri::ipc::Invoke<tauri::Wry>| {
                #[cfg(target_os = "macos")]
                if invoke.message.webview().label() == automation_help::WINDOW_LABEL {
                    return automation_help::handle_invoke(invoke);
                }
                commands(invoke)
            }
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                #[cfg(not(target_os="macos"))]
                use tauri::Manager;
                // Closing History or Help only closes that auxiliary window.
                if window.label()!="main" {return;}
                #[cfg(target_os="macos")]
                { // macOS red close button hides the main window, not the app.
                    api.prevent_close();
                    let _=window.hide();
                }
                #[cfg(not(target_os="macos"))]
                if !window.state::<app_exit::ExitManager>().request(window.app_handle().clone()) {
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building CaptureScribe")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                use tauri::Manager;
                app.state::<selection_session::SelectionManager>().shutdown();
                app.state::<capture_target::TargetControl>().shutdown();
                if let Some(server) = app.try_state::<std::sync::Mutex<control::ControlServer>>() {
                    if let Ok(mut server) = server.lock() { server.shutdown(); }
                }
            }
            #[cfg(target_os="macos")]
            if let tauri::RunEvent::Reopen {has_visible_windows:false,..}=&event {
                use tauri::Manager;
                // Dock reopen must not reveal the temporarily hidden product
                // over the separate range selector. Its scope restores focus.
                if !capture_area::SELECTING.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Some(window)=app.get_webview_window("main"){let _=window.show();let _=window.set_focus();}
                }
            }
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                use tauri::Manager;
                if !app.state::<app_exit::ExitManager>().request(app.clone()) {
                    api.prevent_exit();
                }
            }
        });
}
