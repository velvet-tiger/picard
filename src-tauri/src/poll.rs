//! The poll loop: reads config, checks volumes, sends alerts, updates the menu.
//!
//! It wakes every `poll_minutes`, and also as soon as the config file changes or
//! a drive is mounted or unmounted under `/Volumes`. File-system watchers run
//! on their own thread and send `Wake` messages into the loop. When
//! `daily_scan` is set it also wakes at that time and runs the daily scan.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::{Action, ActionType, NotificationExt};

use crate::windows::{self, Tab};
use tokio::sync::mpsc;

use crate::config::{self, Config, Mode};
use crate::disk::{self, DiskSpace};
use crate::env::ToolEnv;
use crate::monitor::{self, State, States};
use crate::runner::{self, CleanRequest, VolumeRun};
use crate::state::Shared;
use crate::tray::{MenuView, Tray};
use crate::volume::{self, UnavailableRoot, UuidCache, Volume};

/// The notification action type registered in `register_clean_action`.
const LOW_SPACE_ACTIONS: &str = "picard-low-space";
const CLEAN_ACTION: &str = "clean";
/// What the plugin reports for a click on the notification itself.
const TAP_ACTION: &str = "tap";

/// Adds a Clean… button to Picard's notifications. The button, or a click on
/// the notification itself, opens the Preview window, so nothing is deleted
/// without the user seeing the list first.
pub fn register_clean_action<R: Runtime>(app: &AppHandle<R>) -> Result<()> {
    let notifications = app.notification();
    let clean = Action::builder(CLEAN_ACTION, "Clean…").build();
    notifications
        .register_action_types(vec![
            ActionType::builder(LOW_SPACE_ACTIONS)
                .actions(vec![clean])
                .build(),
        ])
        .context("registering notification actions")?;
    let handle = app.clone();
    notifications
        .on_action(move |performed| {
            if matches!(performed.action_id(), CLEAN_ACTION | TAP_ACTION)
                && let Err(e) = windows::open(&handle, Tab::Clean)
            {
                log::error!("could not open the Clean window from a notification: {e}");
            }
        })
        .context("listening for notification actions")
}

/// Why the loop woke up early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    ConfigChanged,
    VolumesChanged,
}

const VOLUMES_DIR: &str = "/Volumes";

// Editors and FSEvents emit several events per save or mount. Waiting briefly
// and draining the queue turns a burst into one check.
const DEBOUNCE: Duration = Duration::from_millis(500);

// Used only when there is no valid config yet, so a fixed config error still
// gets retried even if no file event arrives.
const FALLBACK_POLL: Duration = Duration::from_secs(5 * 60);

// Added to the wait for the daily scan so the loop wakes just after the
// scheduled minute starts rather than a moment before it.
const DAILY_SCAN_SLACK: Duration = Duration::from_secs(1);

const DAILY_STAMP_FORMAT: &str = "%Y-%m-%d";

/// Reads the date of the last daily scan. A missing file means it has never
/// run. An unreadable or malformed one is logged and treated the same way,
/// which can at worst repeat one scan; refusing to scan would be worse.
pub(crate) fn read_daily_stamp(path: &Path) -> Option<NaiveDate> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!(
                "reading {} failed, treating the daily scan as not run: {e}",
                path.display()
            );
            return None;
        }
    };
    match NaiveDate::parse_from_str(text.trim(), DAILY_STAMP_FORMAT) {
        Ok(d) => Some(d),
        Err(e) => {
            log::warn!(
                "{} holds {text:?}, not a date; treating the daily scan as not run: {e}",
                path.display()
            );
            None
        }
    }
}

/// Records the date of the daily scan, so a restart later that day does not repeat it.
pub(crate) fn write_daily_stamp(path: &Path, date: NaiveDate) -> Result<()> {
    std::fs::write(path, format!("{}\n", date.format(DAILY_STAMP_FORMAT)))
        .with_context(|| format!("writing {}", path.display()))
}

/// What started a cleanup. It decides how much is removed and what is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// A volume fell below its threshold: clean until it is back above it.
    LowSpace,
    /// The daily scan in auto mode: run every task in full.
    Daily,
}

impl Trigger {
    fn request(self) -> CleanRequest {
        match self {
            Trigger::LowSpace => CleanRequest::UntilTarget,
            Trigger::Daily => CleanRequest::Everything,
        }
    }

    /// The notification for a finished run, or `None` if it is not worth one.
    fn report(
        self,
        name: &str,
        run: &Result<VolumeRun, String>,
        after: Option<DiskSpace>,
    ) -> Option<(String, String)> {
        match self {
            Trigger::LowSpace => Some(runner::outcome_message(name, run, after)),
            Trigger::Daily => runner::daily_outcome_message(name, run, after),
        }
    }
}

/// Writes the default config if none exists. Returns true if it wrote one.
pub fn ensure_config_file(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let dir = path
        .parent()
        .with_context(|| format!("config path {} has no parent folder", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(path, config::DEFAULT_TOML)
        .with_context(|| format!("writing default config to {}", path.display()))?;
    Ok(true)
}

/// Starts watching the config folder and `/Volumes`.
///
/// The returned watcher must be kept alive; dropping it stops the events.
/// The config folder is watched rather than the file, because many editors
/// save by replacing the file, which would end a watch on the file itself.
pub fn start_watchers(config_path: &Path, tx: mpsc::Sender<Wake>) -> Result<RecommendedWatcher> {
    let config_name = config_path.file_name().map(|n| n.to_os_string());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                log::warn!("file watcher error: {e}");
                return;
            }
        };
        for path in &event.paths {
            let wake = if path.starts_with(VOLUMES_DIR) {
                Wake::VolumesChanged
            } else if path.file_name().map(|n| n.to_os_string()) == config_name {
                Wake::ConfigChanged
            } else {
                continue;
            };
            // A full queue already holds a pending wake, so dropping this one loses nothing.
            let _ = tx.try_send(wake);
        }
    })
    .context("creating file watcher")?;

    let config_dir = config_path
        .parent()
        .with_context(|| format!("config path {} has no parent folder", config_path.display()))?;
    watcher
        .watch(config_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", config_dir.display()))?;
    watcher
        .watch(Path::new(VOLUMES_DIR), RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {VOLUMES_DIR}"))?;
    Ok(watcher)
}

/// Everything one check found.
pub(crate) struct Scan {
    pub(crate) readings: Vec<(Volume, DiskSpace)>,
    pub(crate) unavailable: Vec<UnavailableRoot>,
}

/// Maps roots to volumes and reads each volume's free space. Blocking I/O.
pub(crate) fn scan(roots: &[PathBuf], uuids: &UuidCache) -> Scan {
    let mut map = volume::map_roots(roots, volume::locate);
    volume::attach_uuids(&mut map.volumes, uuids);
    let mut unavailable = map.unavailable;
    let mut readings = Vec::new();
    for v in map.volumes {
        match disk::query(&v.mount) {
            Ok(space) => readings.push((v, space)),
            // The drive can be unplugged between mapping and reading it.
            Err(e) => {
                log::warn!("reading {} failed: {e}", v.mount.display());
                unavailable.extend(v.roots.iter().map(|root| UnavailableRoot {
                    root: root.clone(),
                    name: v.name.clone(),
                    reason: "not readable".to_string(),
                }));
            }
        }
    }
    Scan {
        readings,
        unavailable,
    }
}

/// The loop's state. Owned by the loop task, never shared.
pub struct Poller<R: Runtime> {
    app: AppHandle<R>,
    tray: Tray<R>,
    shared: Arc<Shared>,
    config: Option<Config>,
    config_error: bool,
    states: States,
    /// Local date of the last daily scan. Read from disk at start, then kept
    /// here, so a failed stamp write cannot make the scan repeat.
    last_daily_scan: Option<NaiveDate>,
}

impl<R: Runtime> Poller<R> {
    pub fn new(app: AppHandle<R>, tray: Tray<R>, shared: Arc<Shared>) -> Self {
        Self {
            app,
            tray,
            shared,
            config: None,
            config_error: false,
            states: States::new(),
            last_daily_scan: None,
        }
    }

    /// Runs until the app exits or every watcher sender is dropped.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Wake>) {
        let home = self.shared.home.clone();
        let shell = std::env::var_os("SHELL");
        match tauri::async_runtime::spawn_blocking(move || ToolEnv::detect(&home, shell)).await {
            Ok(env) => self.shared.set_tools(env),
            Err(e) => log::error!("resolving tool PATH failed; cleanup tasks are off: {e}"),
        }
        self.reload_config();
        self.last_daily_scan = read_daily_stamp(&self.shared.paths.daily_scan);
        loop {
            self.check().await;
            self.daily_scan_if_due().await;
            let wait = self.next_wait();
            match tokio::time::timeout(wait, rx.recv()).await {
                Err(_elapsed) => {}
                Ok(None) => {
                    log::error!("file watcher channel closed; poll loop stopping");
                    return;
                }
                Ok(Some(first)) => {
                    tokio::time::sleep(DEBOUNCE).await;
                    let mut config_changed = first == Wake::ConfigChanged;
                    while let Ok(next) = rx.try_recv() {
                        config_changed |= next == Wake::ConfigChanged;
                    }
                    log::info!("woken early: config changed = {config_changed}");
                    if config_changed {
                        self.reload_config();
                    }
                }
            }
        }
    }

    /// How long to sleep: until the next poll or the next daily scan, whichever is sooner.
    fn next_wait(&self) -> Duration {
        let Some(config) = &self.config else {
            return FALLBACK_POLL;
        };
        let poll = Duration::from_secs(config.poll_minutes.saturating_mul(60));
        match config.daily_scan {
            Some(at) => {
                let now = Local::now().naive_local();
                let daily = monitor::until_daily_scan(at, now, self.last_daily_scan);
                poll.min(daily + DAILY_SCAN_SLACK)
            }
            None => poll,
        }
    }

    /// Runs the daily scan if it is due, following the mode: auto cleans every
    /// volume in full, ask notifies about what could be cleaned, off does nothing.
    async fn daily_scan_if_due(&mut self) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let Some(at) = config.daily_scan else {
            return;
        };
        let today = Local::now().naive_local();
        if !monitor::daily_scan_due(at, today, self.last_daily_scan) {
            return;
        }
        // Recorded before the scan runs, so a scan that fails or panics waits
        // until tomorrow instead of retrying on every poll. Off mode records it
        // too; otherwise the scan would stay due and the loop would never sleep.
        self.last_daily_scan = Some(today.date());
        if let Err(e) = write_daily_stamp(&self.shared.paths.daily_scan, today.date()) {
            log::error!("could not record the daily scan; a restart today may repeat it: {e:#}");
        }
        log::info!("daily scan: mode {:?}", config.mode);
        if config.mode == Mode::Off {
            return;
        }
        let Some(found) = self.scan_volumes(&config).await else {
            return;
        };
        let mut cleaned = false;
        for (volume, space) in found.readings {
            if config.mode == Mode::Auto {
                cleaned |= self.clean(&config, volume, Trigger::Daily).await;
                continue;
            }
            // A low volume has already had its low-space alert; the cooldown
            // decides when it gets the next one.
            if matches!(self.states.get(&volume.mount), Some(State::Fired { .. })) {
                log::info!(
                    "daily scan: {} already alerted as low, skipping",
                    volume.name
                );
                continue;
            }
            let name = volume.name.clone();
            let cleanable = self.cleanable(&config, volume).await;
            match monitor::daily_scan_message(&name, space, cleanable) {
                Some((title, body)) => self.notify(&title, &body),
                None => log::info!("daily scan: nothing to offer on {name}"),
            }
        }
        // Refresh the menu with the space the cleanup freed.
        if cleaned {
            self.check().await;
        }
    }

    /// Re-reads the config file. A bad file keeps the last good config running.
    fn reload_config(&mut self) {
        let loaded = config::load(&self.shared.config_path, &self.shared.home);
        match loaded {
            Ok(c) => {
                log::info!(
                    "config loaded: mode {:?}, {} root(s)",
                    c.mode,
                    c.roots.len()
                );
                let old_mode = self.config.as_ref().map(|old| old.mode);
                self.states = monitor::states_after_reload(
                    std::mem::take(&mut self.states),
                    old_mode,
                    c.mode,
                );
                self.config = Some(c);
                self.config_error = false;
            }
            Err(e) => {
                let keeping = if self.config.is_some() {
                    "keeping the previous config"
                } else {
                    "not monitoring until it is fixed"
                };
                log::error!("config error, {keeping}: {e:#}");
                self.config_error = true;
            }
        }
    }

    /// One check: scan, decide, alert (or clean, in auto mode), redraw.
    async fn check(&mut self) {
        let Some(config) = self.config.clone() else {
            self.show(MenuView {
                config_error: true,
                ..MenuView::default()
            });
            return;
        };
        let Some(mut found) = self.scan_volumes(&config).await else {
            return;
        };
        let (states, alerts) =
            monitor::evaluate(&self.states, &found.readings, &config, Instant::now());
        self.states = states;
        let mut cleaned = false;
        for alert in &alerts {
            if config.mode == Mode::Auto {
                let volume = found.readings.iter().find(|(v, _)| v.mount == alert.mount);
                if let Some((volume, _)) = volume {
                    self.clean(&config, volume.clone(), Trigger::LowSpace).await;
                    cleaned = true;
                    continue;
                }
            }
            let volume = found.readings.iter().find(|(v, _)| v.mount == alert.mount);
            let cleanable = match volume {
                Some((v, _)) => self.cleanable(&config, v.clone()).await,
                None => None,
            };
            let (title, body) = alert.message(cleanable);
            self.notify(&title, &body);
        }
        // Show the space after cleaning, not the reading that triggered it.
        if cleaned && let Some(after) = self.scan_volumes(&config).await {
            found = after;
        }
        let low = found
            .readings
            .iter()
            .any(|(v, space)| monitor::is_low(&config, v, *space));
        self.shared.set_low(&self.app, low);
        self.show(MenuView {
            readings: found.readings,
            unavailable: found.unavailable,
            config_error: self.config_error,
        });
    }

    async fn scan_volumes(&self, config: &Config) -> Option<Scan> {
        let roots = config.roots.clone();
        let uuids = Arc::clone(&self.shared.uuids);
        match tauri::async_runtime::spawn_blocking(move || scan(&roots, &uuids)).await {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("disk scan task failed: {e}");
                None
            }
        }
    }

    /// How much a preview scan finds on `volume`, for the ask-mode estimate.
    /// `None` if the scan cannot run yet or fails; the reason is logged.
    async fn cleanable(&self, config: &Config, volume: Volume) -> Option<u64> {
        let tools = self.shared.tools()?;
        let config = config.clone();
        let sizes = Arc::clone(&self.shared.sizes);
        let scanned = tauri::async_runtime::spawn_blocking(move || {
            runner::preview_volume(&config, &volume, &sizes, &tools)
        })
        .await;
        match scanned {
            Ok(Ok(scans)) => Some(
                scans
                    .iter()
                    .filter_map(|s| s.result.as_ref().ok())
                    .flatten()
                    .map(|c| c.bytes)
                    .sum(),
            ),
            Ok(Err(e)) => {
                log::warn!("estimate for the low-space alert failed: {e:#}");
                None
            }
            Err(e) => {
                log::warn!("estimate task failed: {e}");
                None
            }
        }
    }

    /// Runs the cleanup tasks on one volume and reports the result.
    /// Returns true if the run removed anything.
    async fn clean(&self, config: &Config, volume: Volume, trigger: Trigger) -> bool {
        log::info!("auto mode ({trigger:?}): cleaning {}", volume.name);
        let Some(tools) = self.shared.tools() else {
            let title = match trigger {
                Trigger::LowSpace => format!("{} is low", volume.name),
                Trigger::Daily => format!("Daily scan of {} failed", volume.name),
            };
            self.notify(
                &title,
                "Cleanup is off because tool paths could not be resolved. See the log.",
            );
            return false;
        };
        let _cleaning = self.shared.start_cleaning(&self.app);
        let config = config.clone();
        let sizes = Arc::clone(&self.shared.sizes);
        let paths = self.shared.paths.clone();
        let name = volume.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            let run =
                runner::clean_volume(&config, &volume, &sizes, &paths, &tools, trigger.request());
            let after = disk::query(&volume.mount).ok();
            (run, after)
        })
        .await;
        let (run, after) = match outcome {
            Ok((run, after)) => (run.map_err(|e| format!("{e:#}")), after),
            Err(e) => {
                log::error!("cleanup task for {name} panicked or was cancelled: {e}");
                (Err(e.to_string()), None)
            }
        };
        if let Ok(r) = &run {
            log::info!(
                "auto mode: {name}: removed {}, failed {}, stopped {:?}",
                r.removed.len(),
                r.failed.len(),
                r.stopped
            );
        }
        match trigger.report(&name, &run, after) {
            Some((title, body)) => self.notify(&title, &body),
            None => log::info!("auto mode ({trigger:?}): nothing removed on {name}"),
        }
        run.is_ok_and(|r| !r.removed.is_empty())
    }

    fn notify(&self, title: &str, body: &str) {
        log::info!("alert: {title}: {body}");
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .action_type_id(LOW_SPACE_ACTIONS)
            .show()
        {
            log::error!("could not show notification {title:?}: {e}");
        }
    }

    fn show(&self, view: MenuView) {
        log::info!("status: {}", crate::tray::status_lines(&view).join("; "));
        if let Err(e) = self.tray.show(&view) {
            log::error!("could not update tray menu: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_config_file_writes_once_and_keeps_edits() {
        let dir = std::env::temp_dir().join(format!("picard-config-test-{}", std::process::id()));
        let path = dir.join("nested/config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(ensure_config_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            config::DEFAULT_TOML
        );

        std::fs::write(&path, "edited").unwrap();
        assert!(!ensure_config_file(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn daily_stamp_round_trips_and_tolerates_junk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily-scan.txt");
        assert_eq!(read_daily_stamp(&path), None);
        let date = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        write_daily_stamp(&path, date).unwrap();
        assert_eq!(read_daily_stamp(&path), Some(date));
        std::fs::write(&path, "yesterday").unwrap();
        assert_eq!(read_daily_stamp(&path), None);
    }

    #[test]
    fn scan_reports_missing_root_as_not_mounted() {
        let s = scan(
            &[
                std::env::temp_dir(),
                PathBuf::from("/Volumes/picard-no-such-drive/x"),
            ],
            &UuidCache::default(),
        );
        assert_eq!(s.readings.len(), 1);
        assert_eq!(s.unavailable.len(), 1);
        assert_eq!(s.unavailable[0].name, "picard-no-such-drive");
    }
}
