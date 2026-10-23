//! Menu bar icon and menu.
//!
//! The menu shows one disabled status line per volume, then Quit. The poll loop
//! calls `Tray::show` after every check. When only the numbers changed, the
//! status lines are edited in place, so a menu that is already open shows them.
//! macOS shows the menu the moment the icon is clicked, before Picard hears of
//! the click, so a rebuilt menu would only appear at the next open. The menu is
//! rebuilt when lines come or go, as drives are mounted and unmounted.
//!
//! Pointing at the icon, and pressing it, ask the poll loop for a fresh check.

use std::sync::Arc;

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_notification::NotificationExt;

use crate::disk::{self, DiskSpace};
use crate::poll;
use crate::state::Shared;
use crate::updater::Updates;
use crate::volume::{UnavailableRoot, Volume};
use crate::windows::{self, Tab};

const TRAY_ID: &str = "picard";
const MENU_QUIT: &str = "quit";
const MENU_CLEAN: &str = "clean";
const MENU_SETTINGS: &str = "settings";
const MENU_LOG: &str = "open_log";
const MENU_UPDATE: &str = "restart_to_update";

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
    /// The version that is installed and waiting for a restart.
    pub update_ready: Option<String>,
}

/// Handle to the tray icon, used to update its menu.
pub struct Tray<R: Runtime> {
    app: AppHandle<R>,
    icon: TrayIcon<R>,
    shown: ShownMenu<MenuItem<R>>,
}

/// A status line in the menu whose text can change: a `MenuItem` in the
/// app, a recorder in tests.
trait LineItem {
    fn set_text(&self, text: &str) -> tauri::Result<()>;
}

impl<R: Runtime> LineItem for MenuItem<R> {
    fn set_text(&self, text: &str) -> tauri::Result<()> {
        MenuItem::set_text(self, text)
    }
}

/// What the current menu holds, so `Tray::show` can edit it in place.
struct ShownMenu<I> {
    /// The status line items, in menu order, one per entry in `lines`.
    items: Vec<I>,
    lines: Vec<String>,
    update_ready: Option<String>,
}

/// Whether `ShownMenu::update_in_place` could show the new lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InPlace {
    Done,
    NeedsRebuild,
}

impl<I: LineItem> ShownMenu<I> {
    /// Sets the text of each status line that differs from `lines`.
    /// Returns `NeedsRebuild`, and changes nothing, when a line or the
    /// restart item comes or goes. Each line is recorded once it is set, so
    /// after a failure the next call retries only the lines still out of date.
    fn update_in_place(
        &mut self,
        lines: &[String],
        update_ready: Option<&str>,
    ) -> tauri::Result<InPlace> {
        if !fits_in_place(
            &self.lines,
            self.update_ready.as_deref(),
            lines,
            update_ready,
        ) {
            return Ok(InPlace::NeedsRebuild);
        }
        for ((item, shown), line) in self.items.iter().zip(self.lines.iter_mut()).zip(lines) {
            if shown != line {
                item.set_text(line)?;
                shown.clone_from(line);
            }
        }
        Ok(InPlace::Done)
    }
}

impl<R: Runtime> Tray<R> {
    /// Creates the tray icon with a placeholder menu until the first check.
    pub fn install(app: &AppHandle<R>) -> tauri::Result<Self> {
        let lines = vec!["Checking disks…".to_string()];
        let (menu, items) = build_menu(app, &lines, None)?;
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
                    MENU_UPDATE => restart_to_update(app),
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
            .on_tray_icon_event(|tray, event| {
                if comes_before_menu(&event) {
                    poll::request_refresh(tray.app_handle());
                }
            })
            .build(app)?;
        Ok(Self {
            app: app.clone(),
            icon,
            shown: ShownMenu {
                items,
                lines,
                update_ready: None,
            },
        })
    }

    /// Shows `view` in the menu: in place when only the text of the status
    /// lines changed, otherwise in a new menu.
    pub fn show(&mut self, view: &MenuView) -> tauri::Result<()> {
        let lines = status_lines(view);
        let update_ready = view.update_ready.as_deref();
        if self.shown.update_in_place(&lines, update_ready)? == InPlace::Done {
            return Ok(());
        }
        let (menu, items) = build_menu(&self.app, &lines, update_ready)?;
        self.icon.set_menu(Some(menu))?;
        self.shown = ShownMenu {
            items,
            lines,
            update_ready: view.update_ready.clone(),
        };
        Ok(())
    }
}

/// True for the tray events that come just before the menu opens: the
/// pointer reaching the icon, and a button going down on it. Hovering gives
/// the check a head start, so the menu usually opens on fresh numbers.
fn comes_before_menu(event: &TrayIconEvent) -> bool {
    matches!(
        event,
        TrayIconEvent::Enter { .. }
            | TrayIconEvent::Click {
                button_state: MouseButtonState::Down,
                ..
            }
    )
}

/// True if a menu that shows `old_lines` and offers `old_update` can show
/// `lines` and `update` by changing the text of its status lines. A line
/// that comes or goes, or a change to the restart item, needs a new menu.
fn fits_in_place(
    old_lines: &[String],
    old_update: Option<&str>,
    lines: &[String],
    update: Option<&str>,
) -> bool {
    old_lines.len() == lines.len() && old_update == update
}

/// Builds the menu. Also returns the status line items, in order, so
/// `Tray::show` can change their text later.
fn build_menu<R: Runtime>(
    app: &AppHandle<R>,
    lines: &[String],
    update_ready: Option<&str>,
) -> tauri::Result<(Menu<R>, Vec<MenuItem<R>>)> {
    let menu = Menu::new(app)?;
    let mut items = Vec::with_capacity(lines.len());
    for line in lines {
        let item = MenuItem::new(app, line, false, None::<&str>)?;
        menu.append(&item)?;
        items.push(item);
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
    if let Some(version) = update_ready {
        menu.append(&MenuItem::with_id(
            app,
            MENU_UPDATE,
            update_label(version),
            true,
            None::<&str>,
        )?)?;
    }
    menu.append(&MenuItem::with_id(
        app,
        MENU_QUIT,
        "Quit Picard",
        true,
        None::<&str>,
    )?)?;
    Ok((menu, items))
}

/// The menu item that restarts Picard into an installed update.
fn update_label(version: &str) -> String {
    format!("Restart to Finish Updating ({version})")
}

/// Restarts into the installed update. If it cannot, for example because a
/// cleanup is running, a notification says why: a menu click that does
/// nothing would look broken.
fn restart_to_update<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let restarted = match (
        app.try_state::<Arc<Updates>>(),
        app.try_state::<Arc<Shared>>(),
    ) {
        (Some(updates), Some(shared)) => updates.restart(app, &shared),
        _ => Err("Picard is still starting. Try again in a moment.".to_string()),
    };
    if let Err(reason) = &restarted
        && let Err(e) = app
            .notification()
            .builder()
            .title("Picard could not restart")
            .body(reason)
            .show()
    {
        log::error!("could not show the restart notification: {e}");
    }
    restarted
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
    use std::cell::{Cell, RefCell};
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
            error: None,
        }
    }

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| t.to_string()).collect()
    }

    /// Records every `set_text` call, and fails them while `fail` is set.
    #[derive(Default)]
    struct FakeLine {
        calls: RefCell<Vec<String>>,
        fail: Cell<bool>,
    }

    impl LineItem for FakeLine {
        fn set_text(&self, text: &str) -> tauri::Result<()> {
            self.calls.borrow_mut().push(text.to_string());
            if self.fail.get() {
                return Err(tauri::Error::FailedToReceiveMessage);
            }
            Ok(())
        }
    }

    fn shown(texts: &[&str], update_ready: Option<&str>) -> ShownMenu<FakeLine> {
        ShownMenu {
            items: texts.iter().map(|_| FakeLine::default()).collect(),
            lines: lines(texts),
            update_ready: update_ready.map(str::to_string),
        }
    }

    fn calls(menu: &ShownMenu<FakeLine>) -> Vec<Vec<String>> {
        menu.items
            .iter()
            .map(|i| i.calls.borrow().clone())
            .collect()
    }

    const DISK: &str = "Startup disk: 19 GB free (2%)";
    const PROJECTS: &str = "Projects: not mounted";

    #[test]
    fn unchanged_lines_are_left_alone() {
        let mut menu = shown(&[DISK, PROJECTS], None);
        let same = lines(&[DISK, PROJECTS]);
        assert_eq!(menu.update_in_place(&same, None).unwrap(), InPlace::Done);
        assert_eq!(calls(&menu), vec![Vec::<String>::new(), vec![]]);
    }

    #[test]
    fn one_changed_line_is_set_once() {
        let mut menu = shown(&[DISK, PROJECTS], None);
        let new = lines(&["Startup disk: 21 GB free (2%)", PROJECTS]);
        assert_eq!(menu.update_in_place(&new, None).unwrap(), InPlace::Done);
        assert_eq!(
            calls(&menu),
            vec![vec!["Startup disk: 21 GB free (2%)".to_string()], vec![]]
        );
        assert_eq!(menu.lines, new);
    }

    #[test]
    fn a_new_line_or_restart_item_asks_for_a_rebuild() {
        let mut menu = shown(&[DISK], None);
        let more = lines(&[DISK, PROJECTS]);
        assert_eq!(
            menu.update_in_place(&more, None).unwrap(),
            InPlace::NeedsRebuild
        );
        assert_eq!(
            menu.update_in_place(&lines(&[DISK]), Some("0.7.0"))
                .unwrap(),
            InPlace::NeedsRebuild
        );
        assert_eq!(calls(&menu), vec![Vec::<String>::new()]);
        assert_eq!(menu.lines, lines(&[DISK]), "nothing recorded");
    }

    #[test]
    fn a_failed_line_is_retried_alone() {
        let mut menu = shown(&[DISK, PROJECTS], None);
        let new = lines(&[
            "Startup disk: 21 GB free (2%)",
            "Projects: 8.0 GB free (2%)",
        ]);
        menu.items[1].fail.set(true);
        assert!(menu.update_in_place(&new, None).is_err());
        assert_eq!(
            menu.lines,
            lines(&["Startup disk: 21 GB free (2%)", PROJECTS])
        );

        menu.items[1].fail.set(false);
        assert_eq!(menu.update_in_place(&new, None).unwrap(), InPlace::Done);
        assert_eq!(
            calls(&menu),
            vec![
                vec!["Startup disk: 21 GB free (2%)".to_string()],
                vec![
                    "Projects: 8.0 GB free (2%)".to_string(),
                    "Projects: 8.0 GB free (2%)".to_string()
                ],
            ],
            "the first line is not set again"
        );
        assert_eq!(menu.lines, new);
    }

    #[test]
    fn new_numbers_on_the_same_lines_fit_in_place() {
        let old = lines(&["Startup disk: 19 GB free (2%)", "Projects: not mounted"]);
        let new = lines(&["Startup disk: 21 GB free (2%)", "Projects: not mounted"]);
        assert!(fits_in_place(&old, None, &new, None));
        assert!(fits_in_place(&old, Some("0.7.0"), &new, Some("0.7.0")));
        // The placeholder from `install` is replaced in place by the first check.
        assert!(fits_in_place(
            &lines(&["Checking disks…"]),
            None,
            &lines(&["Startup disk: 19 GB free (2%)"]),
            None
        ));
    }

    #[test]
    fn a_line_or_restart_item_that_comes_or_goes_needs_a_new_menu() {
        let one = lines(&["Startup disk: 19 GB free (2%)"]);
        let two = lines(&[
            "Startup disk: 19 GB free (2%)",
            "Projects: 8.0 GB free (2%)",
        ]);
        assert!(!fits_in_place(&one, None, &two, None));
        assert!(!fits_in_place(&two, None, &one, None));
        assert!(!fits_in_place(&one, None, &one, Some("0.7.0")));
        assert!(!fits_in_place(&one, Some("0.7.0"), &one, None));
        assert!(!fits_in_place(&one, Some("0.7.0"), &one, Some("0.7.1")));
    }

    fn click(button_state: MouseButtonState) -> TrayIconEvent {
        TrayIconEvent::Click {
            id: TRAY_ID.into(),
            position: tauri::PhysicalPosition::new(0.0, 0.0),
            rect: tauri::Rect::default(),
            button: tauri::tray::MouseButton::Left,
            button_state,
        }
    }

    #[test]
    fn hovering_and_pressing_come_before_the_menu() {
        let enter = TrayIconEvent::Enter {
            id: TRAY_ID.into(),
            position: tauri::PhysicalPosition::new(0.0, 0.0),
            rect: tauri::Rect::default(),
        };
        let leave = TrayIconEvent::Leave {
            id: TRAY_ID.into(),
            position: tauri::PhysicalPosition::new(0.0, 0.0),
            rect: tauri::Rect::default(),
        };
        assert!(comes_before_menu(&enter));
        assert!(comes_before_menu(&click(MouseButtonState::Down)));
        assert!(!comes_before_menu(&click(MouseButtonState::Up)));
        assert!(!comes_before_menu(&leave));
    }

    #[test]
    fn the_restart_item_names_the_installed_version() {
        assert_eq!(update_label("0.4.0"), "Restart to Finish Updating (0.4.0)");
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
            update_ready: None,
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
