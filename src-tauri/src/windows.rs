//! Opens Picard's window on its Clean or Settings tab.
//!
//! The window is created when asked for and destroyed when closed, so Picard
//! holds no webview while it sits in the menu bar. The URL hash picks the first
//! tab; an open window is told to switch tabs with the `show-tab` event.

use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

/// Must match the window label granted in capabilities/default.json.
const WINDOW_LABEL: &str = "main";

/// Must match the event name App.svelte listens for.
const SHOW_TAB_EVENT: &str = "show-tab";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Clean,
    Settings,
}

impl Tab {
    /// Must match the tab ids in App.svelte.
    fn id(self) -> &'static str {
        match self {
            Tab::Clean => "clean",
            Tab::Settings => "settings",
        }
    }
}

/// Shows the window on `tab`, creating it if needed, and brings it to the front.
pub fn open<R: Runtime>(app: &AppHandle<R>, tab: Tab) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(WINDOW_LABEL) {
        existing.emit_to(WINDOW_LABEL, SHOW_TAB_EVENT, tab.id())?;
        existing.unminimize()?;
        existing.show()?;
        return existing.set_focus();
    }
    let url = WebviewUrl::App(format!("index.html#/{}", tab.id()).into());
    let window = WebviewWindowBuilder::new(app, WINDOW_LABEL, url)
        .title("Picard")
        .inner_size(760.0, 720.0)
        .min_inner_size(480.0, 400.0)
        .center()
        .build()?;
    // A menu bar app is never the active app, so the new window would open
    // behind whatever the user is working in without an explicit focus.
    window.set_focus()
}

/// Opens Picard's log file in the user's default viewer for `.log` files.
pub fn open_log<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let name = app
        .config()
        .product_name
        .clone()
        .unwrap_or_else(|| "Picard".to_string());
    let log = dir.join(format!("{name}.log"));
    if !log.is_file() {
        return Err(format!("no log file yet at {}", log.display()));
    }
    std::process::Command::new("/usr/bin/open")
        .arg(&log)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open {}: {e}", log.display()))
}
