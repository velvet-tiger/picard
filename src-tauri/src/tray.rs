//! Menu bar icon and menu.
//!
//! The menu holds a disabled status line ("112 GB free (23%)"), a separator and
//! Quit. The status line is refreshed on a fixed timer.

use std::path::PathBuf;
use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Runtime};

use crate::disk;

const TRAY_ID: &str = "picard";
const MENU_QUIT: &str = "quit";

// TODO(M2): replace with `poll_minutes` from config.toml once config.rs exists.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

// TODO(M7): placeholder monochrome icon. Replace with the final ok/low/cleaning set.
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray-template.png");

/// Creates the tray icon and starts the timer that refreshes its status line.
///
/// `watched` is the path whose volume is reported. In M1 that is the home folder.
pub fn install<R: Runtime>(app: &AppHandle<R>, watched: PathBuf) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Reading disk…", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit Picard", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &PredefinedMenuItem::separator(app)?, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(TRAY_ICON)?)
        .icon_as_template(true)
        .tooltip("Picard")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            if event.id() == MENU_QUIT {
                app.exit(0);
            }
        })
        .build(app)?;

    tauri::async_runtime::spawn(refresh_status(status, watched));
    Ok(())
}

/// Updates the status line now and then on every tick, for the life of the app.
async fn refresh_status<R: Runtime>(status: MenuItem<R>, watched: PathBuf) {
    let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
    loop {
        ticker.tick().await;
        let text = status_text(&watched);
        if let Err(e) = status.set_text(&text) {
            log::error!("could not update tray status line to {text:?}: {e}");
        }
    }
}

/// Reads the volume and formats the status line. A failed read is shown in the
/// menu and logged, so a broken reading is never mistaken for a real number.
fn status_text(watched: &std::path::Path) -> String {
    match disk::query(watched) {
        Ok(space) => {
            let text = disk::format_free(space);
            log::info!("{}: {text}", watched.display());
            text
        }
        Err(e) => {
            log::error!("free-space query failed: {e}");
            "Free space unavailable".to_string()
        }
    }
}
