//! Opens the Settings and Preview windows on demand.
//!
//! Windows are created when asked for and destroyed when closed, so Picard
//! holds no webview while it sits in the menu bar. Both load the same frontend,
//! which picks its view from the URL hash.

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Settings,
    Preview,
}

impl Window {
    /// Must match the window labels granted in capabilities/default.json.
    pub fn label(self) -> &'static str {
        match self {
            Window::Settings => "settings",
            Window::Preview => "preview",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Window::Settings => "Picard Settings",
            Window::Preview => "Picard: Clean",
        }
    }

    fn size(self) -> (f64, f64) {
        match self {
            Window::Settings => (620.0, 720.0),
            Window::Preview => (760.0, 640.0),
        }
    }
}

/// Shows the window, creating it if needed, and brings it to the front.
pub fn open<R: Runtime>(app: &AppHandle<R>, which: Window) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(which.label()) {
        existing.unminimize()?;
        existing.show()?;
        return existing.set_focus();
    }
    let (w, h) = which.size();
    let url = WebviewUrl::App(format!("index.html#/{}", which.label()).into());
    let window = WebviewWindowBuilder::new(app, which.label(), url)
        .title(which.title())
        .inner_size(w, h)
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
