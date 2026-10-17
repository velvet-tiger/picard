//! Menu bar icon and menu.
//!
//! The menu shows one disabled status line per volume, then Quit. The poll loop
//! calls `Tray::show` after every check. The menu is rebuilt each time because it
//! is small and the set of volumes changes as drives come and go.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Runtime};

use crate::disk::{self, DiskSpace};
use crate::volume::{UnavailableRoot, Volume};
use crate::windows::{self, Tab};

const TRAY_ID: &str = "picard";
const MENU_QUIT: &str = "quit";
const MENU_CLEAN: &str = "clean";
const MENU_SETTINGS: &str = "settings";
const MENU_LOG: &str = "open_log";

// Template images (black on transparent) that macOS tints for the menu bar.
// Drawn by icons/make_tray_icons.py.
const ICON_OK: &[u8] = include_bytes!("../icons/tray-ok.png");
const ICON_LOW: &[u8] = include_bytes!("../icons/tray-low.png");
const ICON_CLEANING: &[u8] = include_bytes!("../icons/tray-cleaning.png");

/// What the menu bar icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Ok,
    Low,
    Cleaning,
}

impl TrayState {
    /// Cleaning wins over low, and low over ok: what Picard is doing matters
    /// more than what it last measured.
    pub fn from_flags(cleaning: bool, low: bool) -> Self {
        if cleaning {
            TrayState::Cleaning
        } else if low {
            TrayState::Low
        } else {
            TrayState::Ok
        }
    }

    fn icon(self) -> &'static [u8] {
        match self {
            TrayState::Ok => ICON_OK,
            TrayState::Low => ICON_LOW,
            TrayState::Cleaning => ICON_CLEANING,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            TrayState::Ok => "Picard",
            TrayState::Low => "Picard: disk space is low",
            TrayState::Cleaning => "Picard: cleaning",
        }
    }
}

/// Sets the menu bar icon to match `state`. Callable from any thread that has
/// an `AppHandle`, so window commands can show cleaning too.
pub fn set_state<R: Runtime>(app: &AppHandle<R>, state: TrayState) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        log::warn!("tray icon not found; cannot show {state:?}");
        return;
    };
    let result = Image::from_bytes(state.icon()).and_then(|img| {
        tray.set_icon(Some(img))?;
        tray.set_icon_as_template(true)?;
        tray.set_tooltip(Some(state.tooltip()))
    });
    if let Err(e) = result {
        log::error!("could not set tray icon to {state:?}: {e}");
    }
}

/// What the menu should say, built by the poll loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuView {
    pub readings: Vec<(Volume, DiskSpace)>,
    pub unavailable: Vec<UnavailableRoot>,
    pub config_error: bool,
}

/// Handle to the tray icon, used to replace its menu.
pub struct Tray<R: Runtime> {
    app: AppHandle<R>,
    icon: TrayIcon<R>,
}

impl<R: Runtime> Tray<R> {
    /// Creates the tray icon with a placeholder menu until the first check.
    pub fn install(app: &AppHandle<R>) -> tauri::Result<Self> {
        let menu = build_menu(app, &["Checking disks…".to_string()])?;
        let icon = TrayIconBuilder::with_id(TRAY_ID)
            .icon(Image::from_bytes(ICON_OK)?)
            .icon_as_template(true)
            .tooltip("Picard")
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(|app, event| {
                let opened = match event.id().as_ref() {
                    MENU_CLEAN => windows::open(app, Tab::Clean).map_err(|e| e.to_string()),
                    MENU_SETTINGS => windows::open(app, Tab::Settings).map_err(|e| e.to_string()),
                    MENU_LOG => windows::open_log(app),
                    MENU_QUIT => {
                        app.exit(0);
                        Ok(())
                    }
                    _ => Ok(()),
                };
                if let Err(e) = opened {
                    log::error!("menu item {:?} failed: {e}", event.id().as_ref());
                }
            })
            .build(app)?;
        Ok(Self {
            app: app.clone(),
            icon,
        })
    }

    /// Replaces the menu with one built from `view`.
    pub fn show(&self, view: &MenuView) -> tauri::Result<()> {
        let menu = build_menu(&self.app, &status_lines(view))?;
        self.icon.set_menu(Some(menu))
    }
}

fn build_menu<R: Runtime>(app: &AppHandle<R>, lines: &[String]) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    for line in lines {
        menu.append(&MenuItem::new(app, line, false, None::<&str>)?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for (id, label) in [
        (MENU_CLEAN, "Clean…"),
        (MENU_SETTINGS, "Settings…"),
        (MENU_LOG, "Open Log"),
    ] {
        menu.append(&MenuItem::with_id(app, id, label, true, None::<&str>)?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        MENU_QUIT,
        "Quit Picard",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

/// The disabled status lines at the top of the menu.
///
/// Mounted volumes come first, then roots that cannot be checked. Several
/// roots on one unplugged drive share a name, so each name is listed once.
pub fn status_lines(view: &MenuView) -> Vec<String> {
    let mut lines = Vec::new();
    if view.config_error {
        lines.push("Config error, see log".to_string());
    }
    for (volume, space) in &view.readings {
        lines.push(format!("{}: {}", volume.name, disk::format_free(*space)));
    }
    let mut seen = Vec::new();
    for root in &view.unavailable {
        if !seen.contains(&&root.name) {
            seen.push(&root.name);
            lines.push(format!("{}: {}", root.name, root.reason));
        }
    }
    if lines.is_empty() {
        lines.push("No roots configured".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn volume(name: &str) -> Volume {
        Volume {
            dev: 1,
            mount: PathBuf::from("/"),
            name: name.to_string(),
            roots: vec![],
            uuid: None,
        }
    }

    fn unavailable(name: &str, root: &str) -> UnavailableRoot {
        UnavailableRoot {
            root: PathBuf::from(root),
            name: name.to_string(),
            reason: "not mounted".to_string(),
        }
    }

    #[test]
    fn lists_volumes_then_unavailable_roots_once_each() {
        let view = MenuView {
            readings: vec![(
                volume("Startup disk"),
                DiskSpace {
                    free_bytes: 19_000_000_000,
                    total_bytes: 994_000_000_000,
                },
            )],
            unavailable: vec![
                unavailable("Projects", "/Volumes/Projects/a"),
                unavailable("Projects", "/Volumes/Projects/b"),
            ],
            config_error: false,
        };
        assert_eq!(
            status_lines(&view),
            vec!["Startup disk: 19 GB free (2%)", "Projects: not mounted"]
        );
    }

    #[test]
    fn cleaning_beats_low_beats_ok() {
        assert_eq!(TrayState::from_flags(true, true), TrayState::Cleaning);
        assert_eq!(TrayState::from_flags(false, true), TrayState::Low);
        assert_eq!(TrayState::from_flags(false, false), TrayState::Ok);
    }

    #[test]
    fn config_error_comes_first() {
        let view = MenuView {
            config_error: true,
            ..MenuView::default()
        };
        assert_eq!(status_lines(&view), vec!["Config error, see log"]);
    }

    #[test]
    fn empty_view_says_so() {
        assert_eq!(
            status_lines(&MenuView::default()),
            vec!["No roots configured"]
        );
    }
}
