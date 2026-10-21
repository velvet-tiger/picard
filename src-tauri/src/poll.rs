//! The poll loop: reads config, checks volumes, sends alerts, updates the menu.
//!
//! It wakes every `poll_minutes`, and also as soon as the config file changes or
//! a drive is mounted or unmounted under `/Volumes`. File-system watchers run
//! on their own thread and send `Wake` messages into the loop. When
//! `daily_scan` is set it also wakes at that time and runs the daily scan.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::{Action, ActionType, NotificationExt};

use crate::windows::{self, Tab};
use tokio::sync::mpsc;

use crate::config::{self, Config, Mode};
use crate::disk::{self, DiskSpace};
use crate::env::ToolEnv;
use crate::monitor::{self, DailyAction, States};
use crate::runner::{self, CleanRequest, LockBusy, TaskScan, VolumeRun};
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

const DAILY_STAMP_FORMAT: &str = "%Y-%m-%d";

/// What `daily-scan.txt` records: the local date of the last daily scan
/// (`None` before the first one), and the `daily_scan` time in force when it
/// was written (`None` when off). The time lets the first config load after a
/// start tell whether the setting changed while Picard was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DailyStamp {
    pub(crate) date: Option<NaiveDate>,
    pub(crate) at: Option<NaiveTime>,
}

/// Stands in for the date in the stamp before the first daily scan has run.
const NO_DATE: &str = "-";

impl DailyStamp {
    /// One line such as `2026-10-20 03:00`, `- 03:00` before the first scan,
    /// or `2026-10-20` when the scan is off.
    fn to_line(self) -> String {
        let date = self.date.map_or_else(
            || NO_DATE.to_string(),
            |d| d.format(DAILY_STAMP_FORMAT).to_string(),
        );
        match self.at {
            Some(at) => format!("{date} {}", at.format(config::DAILY_SCAN_FORMAT)),
            None => date.to_string(),
        }
    }

    /// Parses `to_line`'s output. `None` for anything else.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        let date = match parts.next()? {
            NO_DATE => None,
            d => Some(NaiveDate::parse_from_str(d, DAILY_STAMP_FORMAT).ok()?),
        };
        let at = match parts.next() {
            Some(t) => Some(NaiveTime::parse_from_str(t, config::DAILY_SCAN_FORMAT).ok()?),
            None => None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self { date, at })
    }
}

/// Reads the daily-scan stamp. A missing file means the scan has never run.
///
/// An unreadable or malformed file is logged and treated the same way. The
/// first config load then counts the scan as being set up now, so if its time
/// has passed, today's scan is skipped. Skipping one scan is safer than
/// starting a full cleanup the user did not expect.
pub(crate) fn read_daily_stamp(path: &Path) -> Option<DailyStamp> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!(
                "reading {} failed, treating the daily scan as never run: {e}",
                path.display()
            );
            return None;
        }
    };
    let stamp = DailyStamp::parse(&text);
    if stamp.is_none() {
        log::warn!(
            "{} holds {text:?}, not a daily-scan stamp; treating the daily scan as never run",
            path.display()
        );
    }
    stamp
}

/// Writes the daily-scan stamp, so a restart later that day does not repeat the scan.
///
/// Opened with `O_NOFOLLOW` and mode 600, so a symlink planted in its place
/// cannot redirect the write to another file.
pub(crate) fn write_daily_stamp(path: &Path, stamp: DailyStamp) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{}", stamp.to_line()).with_context(|| format!("writing {}", path.display()))
}

/// How one cleanup ended, as far as the poll loop cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleaned {
    Removed,
    NothingRemoved,
    /// Another cleanup held the run lock, so this one did not start.
    LockBusy,
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

/// What a file event on one path means.
#[derive(Debug, PartialEq, Eq)]
enum Watched {
    Wake(Wake),
    /// `history.jsonl` changed: some clean, here or in the CLI, removed something.
    History,
}

/// Sorts one event path. Paths are matched on their file name, not in full,
/// because FSEvents can report a path through a different spelling of the
/// same folder (`/private/var` for `/var`, say).
fn classify(
    path: &Path,
    config_name: Option<&OsStr>,
    history_name: Option<&OsStr>,
) -> Option<Watched> {
    if path.starts_with(VOLUMES_DIR) {
        return Some(Watched::Wake(Wake::VolumesChanged));
    }
    let name = path.file_name();
    if name.is_some() && name == config_name {
        Some(Watched::Wake(Wake::ConfigChanged))
    } else if name.is_some() && name == history_name {
        Some(Watched::History)
    } else {
        None
    }
}

/// Starts watching the config folder and `/Volumes`.
///
/// The returned watcher must be kept alive; dropping it stops the events.
/// The config folder is watched rather than the file, because many editors
/// save by replacing the file, which would end a watch on the file itself.
/// `history.jsonl` lives in the same folder, so a change to it calls
/// `on_history` on the watcher's thread, at most once per event. The CLI runs
/// in its own process, and this is the only way the app learns it cleaned.
pub fn start_watchers(
    config_path: &Path,
    tx: mpsc::Sender<Wake>,
    on_history: impl Fn() + Send + 'static,
) -> Result<RecommendedWatcher> {
    let config_name = config_path.file_name().map(OsStr::to_os_string);
    let history_name = runner::DataPaths::beside(config_path)
        .history
        .file_name()
        .map(OsStr::to_os_string);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                log::warn!("file watcher error: {e}");
                return;
            }
        };
        let mut history_changed = false;
        for path in &event.paths {
            match classify(path, config_name.as_deref(), history_name.as_deref()) {
                // A full queue already holds a pending wake, so dropping this one loses nothing.
                Some(Watched::Wake(wake)) => {
                    let _ = tx.try_send(wake);
                }
                Some(Watched::History) => history_changed = true,
                None => {}
            }
        }
        if history_changed {
            on_history();
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
    /// The `daily_scan` time the stamp on disk was written with.
    recorded_daily_time: Option<NaiveTime>,
    /// Volumes whose daily clean found the run lock busy, with the scan's
    /// date. Retried at each check that same day.
    daily_retry: Option<(NaiveDate, Vec<PathBuf>)>,
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
            recorded_daily_time: None,
            daily_retry: None,
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
        // Read before the first config load, which uses it to tell a first
        // ever start from a restart.
        let stamp = read_daily_stamp(&self.shared.paths.daily_scan);
        self.last_daily_scan = stamp.and_then(|s| s.date);
        self.recorded_daily_time = stamp.and_then(|s| s.at);
        self.reload_config();
        loop {
            self.check().await;
            self.daily_scan_if_due(Local::now().naive_local()).await;
            let wait = self.next_wait(Local::now().naive_local());
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
    fn next_wait(&self, now: NaiveDateTime) -> Duration {
        let Some(config) = &self.config else {
            return FALLBACK_POLL;
        };
        let poll = Duration::from_secs(config.poll_minutes.saturating_mul(60));
        monitor::next_wait(poll, config.daily_scan, now, self.last_daily_scan)
    }

    /// Records `date` as the day of the last daily scan (`None` if none has
    /// run yet), under the `daily_scan` time `at`, in memory and on disk.
    fn record_daily_scan(&mut self, date: Option<NaiveDate>, at: Option<NaiveTime>) {
        self.last_daily_scan = date;
        self.recorded_daily_time = at;
        if let Err(e) = write_daily_stamp(&self.shared.paths.daily_scan, DailyStamp { date, at }) {
            log::error!("could not record the daily scan; a restart today may repeat it: {e:#}");
        }
    }

    /// Runs the daily scan if it is due, following the mode: auto cleans every
    /// volume in full, ask notifies about what could be cleaned, off does nothing.
    /// When it is not due, retries any clean that found the run lock busy.
    async fn daily_scan_if_due(&mut self, now: NaiveDateTime) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let Some(at) = config.daily_scan else {
            return;
        };
        if !monitor::daily_scan_due(at, now, self.last_daily_scan) {
            self.retry_daily_cleans(&config, now.date()).await;
            return;
        }
        // Recorded before the scan runs, so a scan that fails or panics waits
        // until tomorrow instead of retrying on every poll. Off mode records it
        // too; otherwise the scan would stay due and the loop would never sleep.
        self.record_daily_scan(Some(now.date()), Some(at));
        self.daily_retry = None;
        log::info!("daily scan: mode {:?}", config.mode);
        if config.mode == Mode::Off {
            return;
        }
        let Some(found) = self.scan_volumes(&config).await else {
            return;
        };
        let mut to_clean = Vec::new();
        for (volume, space) in found.readings {
            let state = self.states.get(&volume.mount).copied();
            match monitor::daily_action(config.mode, state) {
                DailyAction::Clean => to_clean.push(volume),
                DailyAction::Skip => log::info!("daily scan: skipping {}", volume.name),
                DailyAction::Offer => {
                    let name = volume.name.clone();
                    let cleanable = self.cleanable(&config, volume).await;
                    match monitor::daily_scan_message(&name, space, cleanable) {
                        Some((title, body)) => self.notify(&title, &body),
                        None => log::info!("daily scan: nothing to offer on {name}"),
                    }
                }
            }
        }
        self.clean_daily(&config, now.date(), to_clean).await;
    }

    /// Retries the daily cleans that found the run lock busy, if they are
    /// still for today and the mode is still auto.
    async fn retry_daily_cleans(&mut self, config: &Config, today: NaiveDate) {
        let Some((date, mounts)) = self.daily_retry.take() else {
            return;
        };
        if date != today || config.mode != Mode::Auto {
            log::info!("daily scan: dropping {} pending retry(s)", mounts.len());
            return;
        }
        let Some(found) = self.scan_volumes(config).await else {
            self.daily_retry = Some((date, mounts));
            return;
        };
        // A volume that has since been unplugged is dropped.
        let volumes = found
            .readings
            .into_iter()
            .map(|(v, _)| v)
            .filter(|v| mounts.contains(&v.mount))
            .collect();
        self.clean_daily(config, date, volumes).await;
    }

    /// Cleans each volume in full for the daily scan. Volumes whose clean
    /// found the run lock busy are kept for a retry at the next check.
    ///
    /// A volume that `check` just cleaned for low space is cleaned again here,
    /// in full, and can produce a second notification. That only happens when
    /// a drive is low at the scheduled time, and the second run is still useful.
    async fn clean_daily(&mut self, config: &Config, date: NaiveDate, volumes: Vec<Volume>) {
        let mut busy = Vec::new();
        let mut cleaned = false;
        for volume in volumes {
            let mount = volume.mount.clone();
            match self.clean(config, volume, Trigger::Daily).await {
                Cleaned::Removed => cleaned = true,
                Cleaned::NothingRemoved => {}
                Cleaned::LockBusy => busy.push(mount),
            }
        }
        if !busy.is_empty() {
            log::info!(
                "daily scan: another cleanup is running; retrying {} volume(s) at the next check",
                busy.len()
            );
            self.daily_retry = Some((date, busy));
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
                // On the first load there is no previous config, so the time
                // in the stamp stands in for it. That catches a change made
                // while Picard was closed. No stamp counts as being set up now.
                let old_daily = match &self.config {
                    Some(old) => old.daily_scan,
                    None => self.recorded_daily_time,
                };
                // A retry belongs to the schedule it came from. Without this, a
                // pending retry could start a full clean right after a save.
                if old_daily != c.daily_scan && self.daily_retry.take().is_some() {
                    log::info!("daily scan setting changed; dropping pending retries");
                }
                let last = monitor::last_scan_after_change(
                    old_daily,
                    c.daily_scan,
                    Local::now().naive_local(),
                    self.last_daily_scan,
                );
                if last != self.last_daily_scan {
                    log::info!("daily scan set after its time today; the first one runs tomorrow");
                }
                // Keeps the stamp's time in step with the config, even before
                // the first scan, so the next start compares against the
                // setting actually in force.
                if last != self.last_daily_scan || c.daily_scan != self.recorded_daily_time {
                    self.record_daily_scan(last, c.daily_scan);
                }
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
        let name = volume.name.clone();
        let Some(tools) = self.shared.tools() else {
            log::warn!("cleanup estimate for {name} skipped: tool paths are not resolved");
            return None;
        };
        let config = config.clone();
        let sizes = Arc::clone(&self.shared.sizes);
        let scanned = tauri::async_runtime::spawn_blocking(move || {
            runner::preview_volume(&config, &volume, &sizes, &tools)
        })
        .await;
        match scanned {
            Ok(Ok(scans)) => Some(scans.iter().map(TaskScan::cleanable_bytes).sum()),
            Ok(Err(e)) => {
                log::warn!("cleanup estimate for {name} failed: {e:#}");
                None
            }
            Err(e) => {
                log::warn!("cleanup estimate task for {name} failed: {e}");
                None
            }
        }
    }

    /// Runs the cleanup tasks on one volume and reports the result.
    async fn clean(&self, config: &Config, volume: Volume, trigger: Trigger) -> Cleaned {
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
            return Cleaned::NothingRemoved;
        };
        let _cleaning = self.shared.start_cleaning(&self.app);
        let config = config.clone();
        let sizes = Arc::clone(&self.shared.sizes);
        let paths = self.shared.paths.clone();
        let (config_path, home) = (self.shared.config_path.clone(), self.shared.home.clone());
        let name = volume.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            let run = runner::clean_volume(
                &config,
                &volume,
                &sizes,
                &paths,
                &tools,
                trigger.request(),
                &runner::ignore_from_file(&config_path, &home),
            );
            let after = disk::query(&volume.mount).ok();
            (run, after)
        })
        .await;
        let (run, after) = match outcome {
            // The daily scan retries later. A low-space clean reports it, as
            // before, and the cooldown decides when it tries again.
            Ok((Err(e), _))
                if trigger == Trigger::Daily && e.downcast_ref::<LockBusy>().is_some() =>
            {
                return Cleaned::LockBusy;
            }
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
        if run.is_ok_and(|r| !r.removed.is_empty()) {
            Cleaned::Removed
        } else {
            Cleaned::NothingRemoved
        }
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
    fn classify_sorts_config_history_and_volume_events() {
        let config = Some(OsStr::new("config.toml"));
        let history = Some(OsStr::new("history.jsonl"));
        let sort = |p: &str| classify(Path::new(p), config, history);

        assert_eq!(
            sort("/Users/a/.config/picard/config.toml"),
            Some(Watched::Wake(Wake::ConfigChanged))
        );
        // FSEvents can name the same folder another way; only the file name counts.
        assert_eq!(
            sort("/private/var/u/picard/history.jsonl"),
            Some(Watched::History)
        );
        assert_eq!(
            sort("/Volumes/Work"),
            Some(Watched::Wake(Wake::VolumesChanged))
        );
        assert_eq!(sort("/Users/a/.config/picard/run.lock"), None);
        assert_eq!(sort("/Users/a/.config/picard/daily-scan.txt"), None);
        assert_eq!(sort("/"), None);
    }

    #[test]
    fn classify_matches_nothing_when_a_name_is_missing() {
        assert_eq!(classify(Path::new("/"), None, None), None);
        assert_eq!(classify(Path::new("/a/history.jsonl"), None, None), None);
    }

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

    // Runs the real watcher, so it proves FSEvents reports an append to a file
    // that already exists. That is how every clean after the first one writes.
    #[test]
    fn start_watchers_reports_an_append_to_history() {
        let dir = std::env::temp_dir().join(format!("picard-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let history = runner::DataPaths::beside(&config_path).history;
        let entry = crate::history::Entry {
            time: chrono::Utc::now(),
            task: "node_modules".to_string(),
            volume: "Macintosh HD".to_string(),
            path: "/Users/a/w/shop/node_modules".to_string(),
            bytes: 1,
        };
        crate::history::append(&history, std::slice::from_ref(&entry)).unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let _watcher = start_watchers(&config_path, tx, move || {
            let _ = seen_tx.send(());
        })
        .unwrap();
        crate::history::append(&history, &[entry]).unwrap();

        let seen = seen_rx.recv_timeout(Duration::from_secs(10));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(seen.is_ok(), "no history event within 10 s of the append");
    }

    fn stamp(at: Option<(u32, u32)>) -> DailyStamp {
        DailyStamp {
            date: NaiveDate::from_ymd_opt(2026, 10, 20),
            at: at.map(|(h, m)| NaiveTime::from_hms_opt(h, m, 0).unwrap()),
        }
    }

    #[test]
    fn daily_stamp_round_trips_and_tolerates_junk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily-scan.txt");
        assert_eq!(read_daily_stamp(&path), None);
        let before_first_scan = DailyStamp {
            date: None,
            ..stamp(Some((22, 0)))
        };
        for s in [stamp(Some((3, 0))), stamp(None), before_first_scan] {
            write_daily_stamp(&path, s).unwrap();
            assert_eq!(read_daily_stamp(&path), Some(s));
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "- 22:00\n",
            "the last write, a scan not yet run, has no date"
        );
        for junk in [
            "yesterday",
            "2026-10-20 25:00",
            "2026-10-20 03:00 extra",
            "",
            "- 9pm",
        ] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(read_daily_stamp(&path), None, "{junk:?}");
        }
    }

    #[test]
    fn daily_stamp_write_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("precious.txt");
        std::fs::write(&target, "keep me").unwrap();
        let path = dir.path().join("daily-scan.txt");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(write_daily_stamp(&path, stamp(Some((3, 0)))).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
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
