//! The macOS Help item opens a standalone, informational webview.
use crate::{capture_area::SELECTING, history_window::HistoryWindowState};
use std::sync::atomic::Ordering;
use tauri::{
    ipc::{Invoke, InvokeError},
    menu::{Menu, MenuItem, HELP_SUBMENU_ID},
    AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Wry,
};

pub(crate) const MENU_ID: &str = "capture-automation-help";
pub(crate) const WINDOW_LABEL: &str = "automation-help";

#[derive(serde::Deserialize)]
struct MenuCopy {
    #[serde(rename = "automationHelp.menu")]
    menu: String,
    #[serde(rename = "automationHelp.title")]
    title: String,
}

fn copy(language: &str) -> serde_json::Result<MenuCopy> {
    let catalog = match language {
        "en" => include_str!("../../src/locales/en.json"),
        _ => include_str!("../../src/locales/ja.json"),
    };
    serde_json::from_str(catalog)
}

fn menu_label(copy: &MenuCopy) -> String {
    // The native menu library treats a single ampersand as a mnemonic.
    copy.menu.replace('&', "&&")
}

fn language_script(language: &str) -> String {
    let language = if language == "en" { "en" } else { "ja" };
    format!(
        r#"window.captureScribeHelpLanguage = "{language}";
        window.dispatchEvent(new Event("capturescribe:help-language"));"#
    )
}

pub(crate) fn add_menu_item(app: &AppHandle, menu: &Menu<Wry>) {
    let Some(help) = menu.get(HELP_SUBMENU_ID) else {
        return;
    };
    let Some(help) = help.as_submenu() else {
        return;
    };
    let Ok(copy) = copy("ja") else {
        return;
    };
    if let Ok(item) = MenuItem::with_id(app, MENU_ID, menu_label(&copy), true, None::<&str>) {
        let _ = help.append(&item);
    }
}

pub(crate) fn refresh_language(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let language = handle.state::<HistoryWindowState>().current_language();
        let Ok(copy) = copy(&language) else {
            return;
        };
        if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
            let _ = window.set_title(&copy.title);
            // Send only the locale, without exposing broadcast history events to Help.
            let _ = window.eval(language_script(&language));
        }
        let Some(help) = handle.menu().and_then(|menu| menu.get(HELP_SUBMENU_ID)) else {
            return;
        };
        if let Some(item) = help.as_submenu().and_then(|menu| menu.get(MENU_ID)) {
            if let Some(item) = item.as_menuitem() {
                let _ = item.set_text(menu_label(&copy));
            }
        }
    });
}

pub(crate) fn open(app: &AppHandle) {
    if SELECTING.load(Ordering::SeqCst) {
        return;
    }
    let window = match app.get_webview_window(WINDOW_LABEL) {
        Some(window) => window,
        None => {
            let language = app.state::<HistoryWindowState>().current_language();
            let Ok(menu_copy) = copy(&language) else {
                return;
            };
            let Ok(window) = WebviewWindowBuilder::new(
                app,
                WINDOW_LABEL,
                WebviewUrl::App("index.html?window=automation-help".into()),
            )
            .title(menu_copy.title)
            .inner_size(540.0, 510.0)
            .min_inner_size(360.0, 340.0)
            .visible(false)
            .focused(false)
            .initialization_script(language_script(&language))
            .on_page_load(|window: WebviewWindow, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    // Catch a language change made while the frontend was loading.
                    let language = window.state::<HistoryWindowState>().current_language();
                    if let Ok(copy) = copy(&language) {
                        let _ = window.set_title(&copy.title);
                    }
                    let _ = window.eval(language_script(&language));
                }
            })
            .build() else {
                return;
            };
            window
        }
    };
    // Creating the webview can overlap with the selector taking its window snapshot.
    // If selection has started, keep a newly created window hidden.
    if !SELECTING.load(Ordering::SeqCst) {
        let _ = window
            .show()
            .and_then(|_| window.unminimize())
            .and_then(|_| window.set_focus());
    }
}

pub(crate) fn handle_invoke(invoke: Invoke<Wry>) -> bool {
    // Local application commands are otherwise exposed by Tauri's default
    // invoke handler. Help gets its own close action and no plugin capability.
    if invoke.message.command() == "close_automation_help" {
        let result = invoke.message.webview().window().close();
        invoke
            .resolver
            .respond(result.map_err(InvokeError::from_error));
    } else {
        invoke
            .resolver
            .reject("Command unavailable in Automation Help");
    }
    true
}
