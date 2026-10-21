//! Opens Picard's window on its Clean or Settings tab.
//!
//! The window is created when asked for and destroyed when closed, so Picard
//! holds no webview while it sits in the menu bar. The URL hash picks the first
//! tab; an open window is told to switch tabs with the `show-tab` event.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime, Theme, WebviewUrl, WebviewWindowBuilder};

use crate::config::{self, Appearance};
use crate::state::Shared;

/// Must match the window label granted in capabilities/default.json.
const WINDOW_LABEL: &str = "main";

/// Must match the event name App.svelte listens for.
const SHOW_TAB_EVENT: &str = "show-tab";

/// Must match the event name `onHistoryChanged` in api.ts listens for.
const HISTORY_CHANGED_EVENT: &str = "history-changed";

/// Must match the event name `onPreviewChanged` in api.ts listens for.
const PREVIEW_CHANGED_EVENT: &str = "preview-changed";

/// Must match the event name `onUpdateStatus` in api.ts listens for.
const UPDATE_STATUS_EVENT: &str = "update-status";

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
    // The config read can block (a stalled network mount, say), and this runs
    // on the event thread, so the title bar catches up once the read is done.
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let appearance = configured_appearance(&handle);
        if appearance != Appearance::System
            && let Err(e) = set_appearance(&handle, appearance)
        {
            log::warn!("could not apply the appearance to the title bar: {e}");
        }
    });
    // A menu bar app is never the active app, so the new window would open
    // behind whatever the user is working in without an explicit focus.
    window.set_focus()
}

/// The native theme for `appearance`. `None` lets macOS decide.
pub fn theme(appearance: Appearance) -> Option<Theme> {
    match appearance {
        Appearance::System => None,
        Appearance::Light => Some(Theme::Light),
        Appearance::Dark => Some(Theme::Dark),
    }
}

/// Applies `appearance` to the title bar of the open window, if there is one.
pub fn set_appearance<R: Runtime>(app: &AppHandle<R>, appearance: Appearance) -> tauri::Result<()> {
    match app.get_webview_window(WINDOW_LABEL) {
        Some(window) => window.set_theme(theme(appearance)),
        None => Ok(()),
    }
}

/// Tells the open window that `history.jsonl` changed, so the Clean tab can
/// reload its total and its "Recently removed" list. Does nothing when no
/// window is open, which is most of the time for a menu bar app.
pub fn history_changed<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    if app.get_webview_window(WINDOW_LABEL).is_none() {
        return Ok(());
    }
    app.emit_to(WINDOW_LABEL, HISTORY_CHANGED_EVENT, ())
}

/// Tells the open window that the stored previews changed: a background scan
/// stored one, or a background clean dropped one. The Clean tab compares the
/// store with its list. Does nothing when no window is open.
pub fn preview_changed<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    if app.get_webview_window(WINDOW_LABEL).is_none() {
        return Ok(());
    }
    app.emit_to(WINDOW_LABEL, PREVIEW_CHANGED_EVENT, ())
}

/// Tells the open window how the update is going, so the About tab can follow
/// a background download. Does nothing when no window is open.
pub fn update_status_changed<R: Runtime, S: Serialize + Clone>(
    app: &AppHandle<R>,
    status: &S,
) -> tauri::Result<()> {
    if app.get_webview_window(WINDOW_LABEL).is_none() {
        return Ok(());
    }
    app.emit_to(WINDOW_LABEL, UPDATE_STATUS_EVENT, status.clone())
}

/// The appearance the config asks for.
///
/// The window must still work when the config cannot be read, because the
/// window is where the user fixes it. So a bad config gives the macOS
/// appearance, and the reason goes to the log.
fn configured_appearance<R: Runtime>(app: &AppHandle<R>) -> Appearance {
    let Some(shared) = app.try_state::<Arc<Shared>>() else {
        log::warn!("window opens with the macOS appearance: Picard is still starting");
        return Appearance::System;
    };
    match config::load(&shared.config_path, &shared.home) {
        Ok(cfg) => cfg.appearance,
        Err(e) => {
            log::warn!("window opens with the macOS appearance: {e:#}");
            Appearance::System
        }
    }
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
    let mut child = std::process::Command::new("/usr/bin/open")
        .arg(&log)
        .spawn()
        .map_err(|e| format!("could not open {}: {e}", log.display()))?;
    // This runs on the tray's event thread, so the wait that reaps `open`
    // happens elsewhere. Without it each click leaves a zombie process.
    std::thread::spawn(move || {
        if let Err(e) = child.wait() {
            log::warn!("waiting for open to exit: {e}");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_forces_light_or_dark_and_leaves_system_to_macos() {
        assert_eq!(theme(Appearance::System), None);
        assert_eq!(theme(Appearance::Light), Some(Theme::Light));
        assert_eq!(theme(Appearance::Dark), Some(Theme::Dark));
    }
}
