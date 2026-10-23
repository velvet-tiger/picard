//! The poll loop: reads config, checks volumes, sends alerts, updates the menu.
//!
//! It wakes every `poll_minutes`, and also as soon as the config file changes or
//! a drive is mounted or unmounted under `/Volumes`. File-system watchers run
//! on their own thread and send `Wake` messages into the loop. When
//! `daily_scan` is set it also wakes at that time and runs the daily scan.
//! While the Picard window is open it wakes at least every 30 seconds, and
//! `request_refresh` wakes it at once when the menu opens or a scan ends.
//!
//! A pass is routine when it comes on the `poll_minutes` beat. Only routine
//! passes repeat an alert after its cooldown or retry a busy daily clean, so
//! the extra passes cannot chain into repeated alerts or cleans.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_notification::{Action, ActionType, NotificationExt};

use crate::windows::{self, Tab};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::commands::VolumePreview;
use crate::config::{self, Config, Mode};
use crate::disk::{self, DiskSpace};
use crate::env::ToolEnv;
use crate::monitor::{self, DailyAction, States};
use crate::preview_scans::{self, CancelMark, Request, Scope};
use crate::runner::{self, CleanRequest, LockBusy, VolumeRun};
use crate::state::Shared;
use crate::tray::{MenuView, Tray};
use crate::updater;
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
    /// An update was installed. The menu offers the restart.
    UpdateReady,
    /// Something asked for fresh numbers: the menu opened, the window
    /// opened, a preview scan ended, or a clean was written to history.
    Refresh,
}

impl Wake {
    /// File events and update notices come in bursts, so they wait out
    /// `DEBOUNCE` and are handled together. A refresh is answered at once,
    /// because an open menu is waiting on it.
    fn debounced(self) -> bool {
        self != Wake::Refresh
    }
}

/// True if any of `wakes` should wait out `DEBOUNCE` before the check.
fn needs_debounce(wakes: &[Wake]) -> bool {
    wakes.iter().any(|w| w.debounced())
}

/// True if every one of `wakes` only reports an installed update. That adds
/// a menu item and is no reason to scan the drives, which can alert or clean.
fn is_redraw_only(wakes: &[Wake]) -> bool {
    wakes.iter().all(|&w| w == Wake::UpdateReady)
}

/// Moves every wake already queued on `rx` into `wakes`.
fn drain(rx: &mut mpsc::Receiver<Wake>, wakes: &mut Vec<Wake>) {
    while let Ok(next) = rx.try_recv() {
        wakes.push(next);
    }
}

/// Collects one batch of wakes: `first`, everything already queued and,
/// when the batch needs the debounce, everything that arrives during it.
async fn collect_wakes(first: Wake, rx: &mut mpsc::Receiver<Wake>) -> Vec<Wake> {
    let mut wakes = vec![first];
    drain(rx, &mut wakes);
    if needs_debounce(&wakes) {
        tokio::time::sleep(DEBOUNCE).await;
        drain(rx, &mut wakes);
    }
    wakes
}

/// Lets anything with an `AppHandle` ask the poll loop for a check now.
/// Managed by Tauri. Weak, like `Updates::wake`, so it does not keep the
/// loop's channel open after the file watchers are gone.
///
/// Requests are merged: while one `Wake::Refresh` waits in the queue, more
/// requests add nothing. Hovers, scans and history appends can come fast.
/// Unmerged, they could fill the queue and crowd out a config change.
pub struct RefreshSender {
    wake: mpsc::WeakSender<Wake>,
    /// Set while a `Wake::Refresh` is queued. See `mark_refresh_taken`.
    pending: Arc<AtomicBool>,
}

/// What became of one refresh request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requested {
    Queued,
    /// A refresh was already queued. The check it starts comes after this
    /// request, so this one is answered too.
    Merged,
    /// The queue was full. The wakes in it lead to a check anyway.
    QueueFull,
    LoopStopped,
}

impl RefreshSender {
    pub fn new(wake: mpsc::WeakSender<Wake>) -> Self {
        Self {
            wake,
            pending: Arc::default(),
        }
    }

    /// The flag the poll loop clears once it takes a refresh. Pass it to
    /// `Poller::new`.
    pub fn pending(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.pending)
    }

    /// Queues a `Wake::Refresh` unless one is already queued.
    fn request(&self) -> Requested {
        let Some(tx) = self.wake.upgrade() else {
            return Requested::LoopStopped;
        };
        if self.pending.swap(true, Ordering::SeqCst) {
            return Requested::Merged;
        }
        match tx.try_send(Wake::Refresh) {
            Ok(()) => Requested::Queued,
            Err(TrySendError::Full(_)) => {
                // Nothing was queued, so no batch will carry a refresh to
                // clear the flag. Left set, it would merge every later
                // request into one that never comes.
                self.pending.store(false, Ordering::SeqCst);
                Requested::QueueFull
            }
            Err(TrySendError::Closed(_)) => Requested::LoopStopped,
        }
    }
}

/// Clears the pending flag once the loop has taken the queued refresh, so a
/// request made during the check that follows queues a new one. A batch
/// without a refresh leaves the flag alone: a refresh queued just after the
/// drain is still waiting.
fn mark_refresh_taken(pending: &AtomicBool, wakes: &[Wake]) {
    if wakes.contains(&Wake::Refresh) {
        pending.store(false, Ordering::SeqCst);
    }
}

/// Asks the poll loop to check free space now, which redraws the menu and
/// the Clean tab's disk tiles.
///
/// A request that is not queued is logged at debug level only. Before setup
/// manages the sender, or after the loop stops, there is nothing to wake. In
/// each case the numbers are only as old as the last check.
pub fn request_refresh<R: Runtime>(app: &AppHandle<R>) {
    let Some(sender) = app.try_state::<RefreshSender>() else {
        log::debug!("refresh asked for before the poll loop was set up");
        return;
    };
    match sender.request() {
        Requested::Queued | Requested::Merged => {}
        other => log::debug!("refresh not queued: {other:?}"),
    }
}

const VOLUMES_DIR: &str = "/Volumes";

// Editors and FSEvents emit several events per save or mount. Waiting briefly
// and draining the queue turns a burst into one check.
const DEBOUNCE: Duration = Duration::from_millis(500);

// Used only when there is no valid config yet, so a fixed config error still
// gets retried even if no file event arrives.
const FALLBACK_POLL: Duration = Duration::from_secs(5 * 60);

/// The longest the loop sleeps while the Picard window is open, so the free
/// space on the Clean tab, in the menu and in the icon stays current.
const WINDOW_OPEN_POLL: Duration = Duration::from_secs(30);

/// True if a pass starting at `now` is routine: the first one, or the first
/// one at least `poll` after the last routine pass.
fn is_routine(last_routine: Option<Instant>, poll: Duration, now: Instant) -> bool {
    last_routine.is_none_or(|at| now.saturating_duration_since(at) >= poll)
}

/// How long the loop sleeps before its next pass: until the next routine
/// pass or the next daily scan, whichever is sooner, and never more than
/// `WINDOW_OPEN_POLL` while the window is open.
///
/// The routine pass is due `poll` after the last routine pass, not after
/// the latest pass. Otherwise a hover every few minutes would keep pushing
/// it back, and with it the repeat alerts that only it may send.
fn wait_until_due(
    poll: Duration,
    last_routine: Option<Instant>,
    now: Instant,
    daily: Option<NaiveTime>,
    wall: NaiveDateTime,
    last_daily: Option<NaiveDate>,
    window_open: bool,
) -> Duration {
    let until_routine = match last_routine {
        None => Duration::ZERO,
        // A due time past what `Instant` can hold is as good as never.
        Some(at) => at
            .checked_add(poll)
            .map_or(poll, |due| due.saturating_duration_since(now)),
    };
    let wait = monitor::next_wait(until_routine, daily, wall, last_daily);
    if window_open {
        wait.min(WINDOW_OPEN_POLL)
    } else {
        wait
    }
}

/// What to log after a check: the status line, and one warning per root
/// that could not be read.
#[derive(Debug, PartialEq, Eq)]
struct StatusLog {
    line: String,
    root_errors: Vec<String>,
}

/// The read errors behind the unavailable roots in `view`, as log messages.
fn root_errors(view: &MenuView) -> Vec<String> {
    view.unavailable
        .iter()
        .filter_map(|u| {
            u.error
                .as_ref()
                .map(|e| format!("cannot read root {}: {e}", u.root.display()))
        })
        .collect()
}

/// What to log for `view`, or `None` when its status lines and root errors
/// match `last`. The loop can check every 30 seconds and reads every root
/// each time, so an unchanged state is not logged again.
fn status_log(last: Option<&MenuView>, view: &MenuView) -> Option<StatusLog> {
    let lines = crate::tray::status_lines(view);
    let errors = root_errors(view);
    let unchanged =
        last.is_some_and(|l| crate::tray::status_lines(l) == lines && root_errors(l) == errors);
    if unchanged {
        return None;
    }
    Some(StatusLog {
        line: lines.join("; "),
        root_errors: errors,
    })
}

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
                // Refreshes are merged (see `RefreshSender`) and update
                // notices are rare, so only a burst of file events while the
                // loop is busy fills the queue. A dropped volume wake then
                // loses nothing, since the queued wakes lead to the same
                // check. A dropped config wake loses its reload until the
                // file changes again.
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

/// The roots on `volume`, marked not readable because its free space could
/// not be read. The error goes to the debug log only. The poll loop logs it
/// at warn when it changes, because this runs on every refresh and every
/// window read.
fn unreadable_roots(volume: &Volume, e: &std::io::Error) -> Vec<UnavailableRoot> {
    let error = format!("reading {} failed: {e}", volume.mount.display());
    log::debug!("{error}");
    volume
        .roots
        .iter()
        .map(|root| UnavailableRoot {
            root: root.clone(),
            name: volume.name.clone(),
            reason: "not readable".to_string(),
            error: Some(error.clone()),
        })
        .collect()
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
            Err(e) => unavailable.extend(unreadable_roots(&v, &e)),
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
    /// date. Retried at each routine pass that same day.
    daily_retry: Option<(NaiveDate, Vec<PathBuf>)>,
    /// When the last routine pass started. See the module notes.
    last_routine: Option<Instant>,
    /// Shared with `RefreshSender`; see `mark_refresh_taken`.
    refresh_pending: Arc<AtomicBool>,
    /// What the menu last showed, so an installed update can be added to it
    /// without scanning the drives again.
    last_view: Option<MenuView>,
    /// Volumes with a notification estimate still running. See `PendingEstimates`.
    estimates: Arc<PendingEstimates>,
}

/// Mounts with a notification estimate still running, so the poll loop starts
/// at most one per volume. An estimate can wait minutes for a clean, and each
/// waiting one holds a blocking thread; with a short cooldown the loop would
/// otherwise start another on every poll.
#[derive(Debug, Default)]
struct PendingEstimates {
    mounts: Mutex<HashSet<String>>,
}

impl PendingEstimates {
    /// Claims `mount` for a new estimate. `None` if one is already running
    /// there. The claim ends when the returned guard is dropped, however the
    /// estimate ends: done, failed, cancelled or panicked.
    fn claim(self: &Arc<Self>, mount: &str) -> Option<EstimateClaim> {
        if !self.lock().insert(mount.to_string()) {
            return None;
        }
        Some(EstimateClaim {
            pending: Arc::clone(self),
            mount: mount.to_string(),
        })
    }

    /// The set holds plain strings, so a poisoned lock is still usable.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.mounts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One running estimate's hold on its mount. See `PendingEstimates::claim`.
struct EstimateClaim {
    pending: Arc<PendingEstimates>,
    mount: String,
}

impl Drop for EstimateClaim {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.mount);
    }
}

/// Runs the shared preview scan of the volume mounted at `mount` on a
/// blocking thread. `mark` is when it was asked for; see
/// `PreviewScans::mark`. `None` if it cannot run, fails or is cancelled; the
/// reason is logged.
async fn scan_one<R: Runtime>(
    app: AppHandle<R>,
    shared: Arc<Shared>,
    config: Config,
    mount: String,
    mark: CancelMark,
) -> Option<Vec<VolumePreview>> {
    let Some(tools) = shared.tools() else {
        log::warn!("preview scan of {mount} skipped: tool paths are not resolved");
        return None;
    };
    let scope_mount = mount.clone();
    let scanned = tauri::async_runtime::spawn_blocking(move || {
        let request = Request {
            scope: Scope::Volume(&scope_mount),
            mark,
            ticket: None,
        };
        preview_scans::scan(&shared, &config, &tools, request, &app)
    })
    .await;
    match scanned {
        Ok(Ok(Some(found))) => Some(found),
        Ok(Ok(None)) => {
            log::info!("preview scan of {mount} was cancelled");
            None
        }
        Ok(Err(e)) => {
            log::warn!("preview scan of {mount} failed: {e}");
            None
        }
        Err(e) => {
            log::warn!("preview scan task for {mount} failed: {e}");
            None
        }
    }
}

/// Shows a notification with the Clean… button.
fn notify<R: Runtime>(app: &AppHandle<R>, title: &str, body: &str) {
    log::info!("alert: {title}: {body}");
    if let Err(e) = app
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

impl<R: Runtime> Poller<R> {
    /// `refresh_pending` comes from `RefreshSender::pending`.
    pub fn new(
        app: AppHandle<R>,
        tray: Tray<R>,
        shared: Arc<Shared>,
        refresh_pending: Arc<AtomicBool>,
    ) -> Self {
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
            last_routine: None,
            refresh_pending,
            last_view: None,
            estimates: Arc::default(),
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
        let mut redraw_only = false;
        loop {
            if redraw_only {
                self.show_update_in_menu();
                redraw_only = false;
            } else {
                let now = Instant::now();
                let routine = is_routine(self.last_routine, self.poll_interval(), now);
                if routine {
                    self.last_routine = Some(now);
                }
                self.check(routine).await;
                self.daily_scan_if_due(Local::now().naive_local(), routine)
                    .await;
            }
            let wait = self.next_wait(Local::now().naive_local(), Instant::now());
            match tokio::time::timeout(wait, rx.recv()).await {
                Err(_elapsed) => {}
                Ok(None) => {
                    log::error!("file watcher channel closed; poll loop stopping");
                    return;
                }
                Ok(Some(first)) => {
                    let wakes = collect_wakes(first, &mut rx).await;
                    mark_refresh_taken(&self.refresh_pending, &wakes);
                    let config_changed = wakes.contains(&Wake::ConfigChanged);
                    redraw_only = is_redraw_only(&wakes);
                    // Hovering over the icon asks for a refresh, so these
                    // would fill the log at info level.
                    let level = if wakes.iter().all(|&w| w == Wake::Refresh) {
                        log::Level::Debug
                    } else {
                        log::Level::Info
                    };
                    log::log!(
                        level,
                        "woken early: config changed = {config_changed}, menu redraw only = {redraw_only}"
                    );
                    if config_changed {
                        self.reload_config();
                    }
                }
            }
        }
    }

    /// How long apart routine passes are: `poll_minutes`, or
    /// `FALLBACK_POLL` while there is no valid config.
    fn poll_interval(&self) -> Duration {
        self.config.as_ref().map_or(FALLBACK_POLL, |c| {
            Duration::from_secs(c.poll_minutes.saturating_mul(60))
        })
    }

    /// How long to sleep before the next pass. See `wait_until_due`.
    fn next_wait(&self, wall: NaiveDateTime, now: Instant) -> Duration {
        wait_until_due(
            self.poll_interval(),
            self.last_routine,
            now,
            self.config.as_ref().and_then(|c| c.daily_scan),
            wall,
            self.last_daily_scan,
            windows::is_open(&self.app),
        )
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
    /// When it is not due, a routine pass retries any clean that found the
    /// run lock busy.
    async fn daily_scan_if_due(&mut self, now: NaiveDateTime, routine: bool) {
        let Some(config) = self.config.clone() else {
            return;
        };
        let Some(at) = config.daily_scan else {
            return;
        };
        if !monitor::daily_scan_due(at, now, self.last_daily_scan) {
            // A retry starts a clean, which shows in the icon and the Clean
            // tab even when the lock is still busy. Refreshes come every 30
            // seconds and on every hover, so only routine passes retry.
            if routine {
                self.retry_daily_cleans(&config, now.date()).await;
            }
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
                    self.notify_with_estimate(&config, &volume, move |cleanable| {
                        let message = monitor::daily_scan_message(&name, space, cleanable);
                        if message.is_none() {
                            log::info!("daily scan: nothing to offer on {name}");
                        }
                        message
                    });
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
    /// found the run lock busy are kept for a retry at the next routine pass.
    ///
    /// A volume that `check` just cleaned for low space is cleaned again here,
    /// in full, and can produce a second notification. That only happens when
    /// a drive is low at the scheduled time, and the second run is still useful.
    async fn clean_daily(&mut self, config: &Config, date: NaiveDate, volumes: Vec<Volume>) {
        let mut busy = Vec::new();
        let mut rescan = Vec::new();
        for volume in volumes {
            let mount = volume.mount.clone();
            match self.clean(config, volume, Trigger::Daily).await {
                Cleaned::Removed => rescan.push(mount.display().to_string()),
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
        // Refresh the menu with the space the cleanup freed. That check can
        // clean too, so the rescans wait until it is done. It is not routine,
        // so it cannot repeat an alert the pass's own check just sent.
        if !rescan.is_empty() {
            self.check(false).await;
        }
        self.rescan_if_open(config, rescan);
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
                updater::config_reloaded(&self.app);
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
    /// Only a `routine` check repeats an alert after its cooldown; see
    /// `monitor::step`.
    async fn check(&mut self, routine: bool) {
        let Some(config) = self.config.clone() else {
            self.show(MenuView {
                config_error: true,
                update_ready: updater::ready_version(&self.app),
                ..MenuView::default()
            });
            return;
        };
        let Some(mut found) = self.scan_volumes(&config).await else {
            return;
        };
        let (states, alerts) = monitor::evaluate(
            &self.states,
            &found.readings,
            &config,
            Instant::now(),
            routine,
        );
        self.states = states;
        let mut cleaned = false;
        let mut rescan = Vec::new();
        for alert in &alerts {
            let volume = found.readings.iter().find(|(v, _)| v.mount == alert.mount);
            match volume {
                Some((volume, _)) if config.mode == Mode::Auto => {
                    let mount = volume.mount.display().to_string();
                    if self.clean(&config, volume.clone(), Trigger::LowSpace).await
                        == Cleaned::Removed
                    {
                        rescan.push(mount);
                    }
                    cleaned = true;
                }
                Some((volume, _)) => {
                    let alert = alert.clone();
                    self.notify_with_estimate(&config, volume, move |cleanable| {
                        Some(alert.message(cleanable))
                    });
                }
                None => {
                    let (title, body) = alert.message(None);
                    self.notify(&title, &body);
                }
            }
        }
        self.rescan_if_open(&config, rescan);
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
            update_ready: updater::ready_version(&self.app),
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

    /// Sends an ask-mode notification about `volume` once a preview scan has
    /// counted what could be cleaned. `message` turns that count into the
    /// title and body, or `None` to send nothing; the count is `None` if the
    /// scan could not run, failed or was cancelled.
    ///
    /// The scan is the shared preview scan, so it is stored for the Clean tab,
    /// which the notification opens, and an open tab shows it running. It runs
    /// apart from the loop: it waits for its turn and for any clean, which can
    /// take minutes, and the loop must keep checking meanwhile.
    fn notify_with_estimate(
        &self,
        config: &Config,
        volume: &Volume,
        message: impl FnOnce(Option<u64>) -> Option<(String, String)> + Send + 'static,
    ) {
        let mount = volume.mount.display().to_string();
        let Some(claim) = self.estimates.claim(&mount) else {
            log::info!("an estimate for {mount} is still running; not starting another");
            return;
        };
        let (app, shared) = (self.app.clone(), Arc::clone(&self.shared));
        let config = config.clone();
        let mark = shared.scans.mark();
        tauri::async_runtime::spawn(async move {
            let _claim = claim;
            let found = scan_one(app.clone(), shared, config, mount, mark).await;
            let cleanable = found.and_then(|f| f.first().map(|v| v.total_bytes));
            if let Some((title, body)) = message(cleanable) {
                notify(&app, &title, &body);
            }
        });
    }

    /// Rescans each volume in `mounts` after a clean there, so an open Clean
    /// tab gets a fresh list, not only a note that its list is out of date.
    /// Call it once the loop's cleans have all finished, so a rescan does not
    /// run while the next volume is cleaned. Does nothing with no window open.
    fn rescan_if_open(&self, config: &Config, mounts: Vec<String>) {
        if mounts.is_empty() || !windows::is_open(&self.app) {
            return;
        }
        for mount in mounts {
            let (app, shared) = (self.app.clone(), Arc::clone(&self.shared));
            let mark = shared.scans.mark();
            tauri::async_runtime::spawn(scan_one(app, shared, config.clone(), mount, mark));
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
        let mount = volume.mount.display().to_string();
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
            // The stored scan may list what was just removed. An open Clean
            // tab is told, so it can say its list is out of date.
            self.shared.mark_cleaned(&mount);
            if let Err(e) = windows::preview_changed(&self.app) {
                log::warn!("could not tell the window about the clean: {e}");
            }
            Cleaned::Removed
        } else {
            Cleaned::NothingRemoved
        }
    }

    fn notify(&self, title: &str, body: &str) {
        notify(&self.app, title, body);
    }

    /// Shows `view` in the menu and tells the open window to reload its disk
    /// tiles. The status and root errors are logged only when they change.
    fn show(&mut self, view: MenuView) {
        if let Some(changed) = status_log(self.last_view.as_ref(), &view) {
            log::info!("status: {}", changed.line);
            for error in &changed.root_errors {
                log::warn!("{error}");
            }
        }
        if let Err(e) = self.tray.show(&view) {
            log::error!("could not update tray menu: {e}");
        }
        if let Err(e) = windows::status_changed(&self.app) {
            log::warn!("could not tell the window about the new free space: {e}");
        }
        self.last_view = Some(view);
    }

    /// Redraws the last menu with the installed update's restart item.
    fn show_update_in_menu(&mut self) {
        if let Some(mut view) = self.last_view.clone() {
            view.update_ready = updater::ready_version(&self.app);
            self.show(view);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refresh_skips_the_debounce() {
        assert!(!needs_debounce(&[Wake::Refresh]));
        assert!(!needs_debounce(&[Wake::Refresh, Wake::Refresh]));
        for wake in [Wake::ConfigChanged, Wake::VolumesChanged, Wake::UpdateReady] {
            assert!(needs_debounce(&[wake]), "{wake:?}");
            assert!(needs_debounce(&[Wake::Refresh, wake]), "{wake:?}");
        }
    }

    #[test]
    fn only_update_notices_skip_the_check() {
        assert!(is_redraw_only(&[Wake::UpdateReady]));
        assert!(is_redraw_only(&[Wake::UpdateReady, Wake::UpdateReady]));
        assert!(!is_redraw_only(&[Wake::Refresh]));
        assert!(!is_redraw_only(&[Wake::UpdateReady, Wake::Refresh]));
        assert!(!is_redraw_only(&[Wake::UpdateReady, Wake::VolumesChanged]));
    }

    #[test]
    fn drain_takes_every_queued_wake() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.try_send(Wake::Refresh).unwrap();
        tx.try_send(Wake::ConfigChanged).unwrap();
        let mut wakes = vec![Wake::VolumesChanged];
        drain(&mut rx, &mut wakes);
        assert_eq!(
            wakes,
            vec![Wake::VolumesChanged, Wake::Refresh, Wake::ConfigChanged]
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_refresh_batch_skips_the_debounce() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.try_send(Wake::Refresh).unwrap();
        let started = Instant::now();
        let wakes = tauri::async_runtime::block_on(collect_wakes(Wake::Refresh, &mut rx));
        assert!(started.elapsed() < DEBOUNCE / 2, "{:?}", started.elapsed());
        assert_eq!(wakes, vec![Wake::Refresh, Wake::Refresh]);
    }

    #[test]
    fn a_file_batch_waits_out_the_debounce_and_takes_what_arrives() {
        let (tx, mut rx) = mpsc::channel(4);
        let started = Instant::now();
        let late = std::thread::spawn(move || {
            std::thread::sleep(DEBOUNCE / 5);
            tx.try_send(Wake::VolumesChanged).unwrap();
        });
        let wakes = tauri::async_runtime::block_on(collect_wakes(Wake::ConfigChanged, &mut rx));
        late.join().unwrap();
        assert!(started.elapsed() >= DEBOUNCE, "{:?}", started.elapsed());
        assert_eq!(wakes, vec![Wake::ConfigChanged, Wake::VolumesChanged]);
    }

    #[test]
    fn refresh_requests_merge_into_one_queued_wake() {
        let (tx, mut rx) = mpsc::channel(16);
        let sender = RefreshSender::new(tx.downgrade());
        assert_eq!(sender.request(), Requested::Queued);
        assert_eq!(sender.request(), Requested::Merged);
        assert_eq!(rx.try_recv(), Ok(Wake::Refresh));
        assert!(rx.try_recv().is_err(), "only one refresh was queued");

        // Still pending until the loop marks it taken.
        assert_eq!(sender.request(), Requested::Merged);
        mark_refresh_taken(&sender.pending(), &[Wake::Refresh]);
        assert_eq!(sender.request(), Requested::Queued);
        assert_eq!(rx.try_recv(), Ok(Wake::Refresh));
    }

    #[test]
    fn a_batch_without_a_refresh_keeps_the_flag() {
        let pending = AtomicBool::new(true);
        mark_refresh_taken(&pending, &[Wake::VolumesChanged]);
        assert!(pending.load(Ordering::SeqCst));
        mark_refresh_taken(&pending, &[Wake::VolumesChanged, Wake::Refresh]);
        assert!(!pending.load(Ordering::SeqCst));
    }

    #[test]
    fn a_config_change_fits_after_a_flood_of_refreshes() {
        let (tx, mut rx) = mpsc::channel(16);
        let sender = RefreshSender::new(tx.downgrade());
        for _ in 0..100 {
            sender.request();
        }
        // Refreshes take one slot, so 14 file wakes and a config change fit.
        for _ in 0..14 {
            tx.try_send(Wake::VolumesChanged).unwrap();
        }
        tx.try_send(Wake::ConfigChanged)
            .expect("the config change still fits");
        let mut wakes = Vec::new();
        drain(&mut rx, &mut wakes);
        assert_eq!(wakes.iter().filter(|&&w| w == Wake::Refresh).count(), 1);
        assert_eq!(wakes.last(), Some(&Wake::ConfigChanged));
    }

    #[test]
    fn a_full_queue_does_not_leave_the_refresh_stuck() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = RefreshSender::new(tx.downgrade());
        tx.try_send(Wake::VolumesChanged).unwrap();
        assert_eq!(sender.request(), Requested::QueueFull);
        assert_eq!(rx.try_recv(), Ok(Wake::VolumesChanged));
        assert_eq!(sender.request(), Requested::Queued);
    }

    #[test]
    fn a_request_after_the_loop_stops_is_not_an_error() {
        let (tx, rx) = mpsc::channel(1);
        let sender = RefreshSender::new(tx.downgrade());
        drop(tx);
        drop(rx);
        assert_eq!(sender.request(), Requested::LoopStopped);
    }

    #[test]
    fn a_pass_is_routine_once_poll_has_passed_since_the_last_one() {
        let t0 = Instant::now();
        let poll = Duration::from_secs(300);
        assert!(is_routine(None, poll, t0), "the first pass");
        assert!(!is_routine(Some(t0), poll, t0 + Duration::from_secs(299)));
        assert!(is_routine(Some(t0), poll, t0 + poll));
        assert!(is_routine(Some(t0), poll, t0 + Duration::from_secs(900)));
    }

    fn wall(hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 20)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn the_wait_runs_to_the_next_routine_pass() {
        let t0 = Instant::now();
        let poll = Duration::from_secs(300);
        let wait = |now: Instant, window_open: bool| {
            wait_until_due(poll, Some(t0), now, None, wall(14, 0), None, window_open)
        };
        assert_eq!(wait(t0, false), poll);
        // A refresh two minutes in does not push the routine pass back.
        assert_eq!(
            wait(t0 + Duration::from_secs(120), false),
            Duration::from_secs(180)
        );
        // Overdue, for example after a long clean: the next pass is now.
        assert_eq!(wait(t0 + Duration::from_secs(400), false), Duration::ZERO);
        assert_eq!(
            wait_until_due(poll, None, t0, None, wall(14, 0), None, false),
            Duration::ZERO,
            "no routine pass yet"
        );
    }

    #[test]
    fn an_open_window_caps_the_wait_at_30_seconds() {
        let t0 = Instant::now();
        let poll = Duration::from_secs(300);
        let wait =
            |now: Instant| wait_until_due(poll, Some(t0), now, None, wall(14, 0), None, true);
        assert_eq!(wait(t0), WINDOW_OPEN_POLL);
        assert_eq!(wait(t0 + Duration::from_secs(290)), Duration::from_secs(10));
        assert_eq!(
            wait_until_due(FALLBACK_POLL, Some(t0), t0, None, wall(14, 0), None, true),
            WINDOW_OPEN_POLL
        );
    }

    #[test]
    fn a_sooner_daily_scan_cuts_the_wait() {
        let t0 = Instant::now();
        let poll = Duration::from_secs(300);
        let at = NaiveTime::from_hms_opt(14, 1, 0);
        let wait = |window_open: bool| {
            wait_until_due(poll, Some(t0), t0, at, wall(14, 0), None, window_open)
        };
        let daily = Duration::from_secs(60) + monitor::DAILY_SCAN_SLACK;
        assert_eq!(wait(false), daily);
        assert_eq!(wait(true), WINDOW_OPEN_POLL);
    }

    fn view(free_gb: u64, unavailable: Vec<UnavailableRoot>) -> MenuView {
        MenuView {
            readings: vec![(
                Volume {
                    dev: 1,
                    mount: PathBuf::from("/"),
                    name: "Startup disk".into(),
                    roots: vec![],
                    uuid: None,
                },
                DiskSpace {
                    free_bytes: free_gb * 1_000_000_000,
                    total_bytes: 500_000_000_000,
                },
            )],
            unavailable,
            config_error: false,
            update_ready: None,
        }
    }

    fn unreadable(error: &str) -> UnavailableRoot {
        UnavailableRoot {
            root: PathBuf::from("/secret/code"),
            name: "/secret/code".into(),
            reason: "unreadable, see log".into(),
            error: Some(error.into()),
        }
    }

    #[test]
    fn the_status_is_logged_only_when_it_changes() {
        let first = view(20, vec![]);
        let logged = status_log(None, &first).expect("first check is logged");
        assert_eq!(logged.line, "Startup disk: 20 GB free (4%)");
        assert!(logged.root_errors.is_empty());
        assert_eq!(status_log(Some(&first), &view(20, vec![])), None);
        assert!(status_log(Some(&first), &view(19, vec![])).is_some());
    }

    #[test]
    fn root_errors_are_logged_when_they_change() {
        let denied = view(20, vec![unreadable("denied")]);
        let logged = status_log(None, &denied).unwrap();
        assert_eq!(
            logged.root_errors,
            vec!["cannot read root /secret/code: denied"]
        );
        assert_eq!(
            status_log(Some(&denied), &view(20, vec![unreadable("denied")])),
            None
        );
        // Same menu line, new error.
        let timed_out = view(20, vec![unreadable("timed out")]);
        assert_eq!(
            status_log(Some(&denied), &timed_out).unwrap().root_errors,
            vec!["cannot read root /secret/code: timed out"]
        );
    }

    #[test]
    fn one_estimate_runs_per_mount_until_it_ends() {
        let pending = Arc::new(PendingEstimates::default());
        let first = pending.claim("/").expect("nothing pending yet");
        assert!(pending.claim("/").is_none(), "a second estimate of / waits");
        let other = pending.claim("/Volumes/Work");
        assert!(other.is_some(), "another volume is not held up");
        drop(first);
        assert!(
            pending.claim("/").is_some(),
            "the claim ends with its estimate"
        );
    }

    #[test]
    fn an_estimate_that_panics_still_frees_its_mount() {
        let pending = Arc::new(PendingEstimates::default());
        let claim = pending.claim("/").unwrap();
        let ended = std::thread::spawn(move || {
            let _claim = claim;
            panic!("estimate failed");
        })
        .join();
        assert!(ended.is_err());
        assert!(pending.claim("/").is_some());
    }

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
        // FSEvents can replay the append made before the watch started. Wait
        // until the stream is quiet, so only the append below can pass.
        while seen_rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
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
    fn a_volume_that_cannot_be_read_keeps_the_error_for_each_root() {
        let volume = Volume {
            dev: 1,
            mount: PathBuf::from("/Volumes/Work"),
            name: "Work".into(),
            roots: vec![
                PathBuf::from("/Volumes/Work/a"),
                PathBuf::from("/Volumes/Work/b"),
            ],
            uuid: None,
        };
        let e = std::io::Error::other("timed out");
        let roots = unreadable_roots(&volume, &e);
        assert_eq!(roots.len(), 2);
        for root in &roots {
            assert_eq!(root.name, "Work");
            assert_eq!(root.reason, "not readable");
            assert_eq!(
                root.error.as_deref(),
                Some("reading /Volumes/Work failed: timed out")
            );
        }
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
