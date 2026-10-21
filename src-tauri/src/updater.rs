//! Automatic updates: finds a newer release, installs it, and offers a restart.
//!
//! The updater plugin does the network work and checks every download against
//! the public key in tauri.conf.json, so a file that was not signed with
//! Picard's key is never installed. This module decides when to check, what
//! to do with the answer, and what the About tab and the menu show.
//!
//! Installing replaces `Picard.app` on disk. The running copy keeps going
//! until the user restarts, so an update never interrupts a cleanup. The
//! decisions are plain functions over `UpdateStatus` so they are unit tested.
//! `Updates` wires them to the plugin and cannot be tested without a network
//! and a signed release.
//!
//! The plugin installs into whatever directory the running program is in. For
//! anything other than an installed `Picard.app` that would swap and delete a
//! build folder, so `bundle_blocker` refuses to check at all in that case.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::{Notify, mpsc};

use crate::config;
use crate::poll::Wake;
use crate::state::Shared;
use crate::windows;

/// Lets a fresh start settle before the first request.
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// How often the background task looks at the clock and the setting.
const TICK: Duration = Duration::from_secs(60 * 60);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The wait after the first failure. It doubles with each failure in a row,
/// up to `CHECK_EVERY`, so a release that cannot be installed is not
/// downloaded again every hour.
const RETRY_BASE: Duration = Duration::from_secs(60 * 60);
/// Limits the question "is there a newer version?".
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Limits the download of the new app. It is a few MB, but links can be slow.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Where a release download may come from. `latest.json` is not signed, so
/// its URL is not trusted. These are the hosts GitHub serves release files
/// from, including the ones it redirects to.
const DOWNLOAD_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

const NO_UPDATE_OFFERED: &str = "No update is waiting to be installed. Check again.";
const NOT_READY: &str = "No update is waiting to be installed.";
const CLEANING: &str = "Picard is cleaning. Restart it when the cleanup has finished.";
const NOT_AN_APP: &str =
    "this copy of Picard was not started from Picard.app, so it cannot replace itself";
const READ_ONLY_LOCATION: &str = "Picard.app is running from a disk image or a quarantined location. Move it to the Applications folder first";

/// Which step an update failed in. The About tab words the two differently
/// and only offers the manual download after an install failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailStage {
    Check,
    Install,
}

/// Where the updater is. Must match `UpdateStatus` in src/lib/api.ts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// Nothing has been checked since Picard started.
    Idle,
    Checking,
    UpToDate,
    /// A newer version exists and has not been installed, because automatic
    /// updates are off, the config could not be read, or Picard cannot write
    /// to the folder it is in without asking for a password.
    Available {
        version: String,
    },
    Downloading {
        version: String,
    },
    /// Installed on disk. The running copy is still the old version.
    Ready {
        version: String,
    },
    Failed {
        message: String,
        stage: FailStage,
    },
}

/// What to do after the plugin answered, with the plugin's update object
/// carried along as `U` so the decision can be tested without one.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<U> {
    Done(UpdateStatus),
    /// Keep `update` so the user can install it from the About tab.
    Offer {
        version: String,
        update: U,
    },
    Install {
        version: String,
        update: U,
    },
}

/// Decides what follows a check. `found` is the newer version and its update
/// object, if there is one.
pub fn after_check<U>(found: Result<Option<(String, U)>, String>, auto_install: bool) -> Step<U> {
    match found {
        Err(message) => Step::Done(UpdateStatus::Failed {
            message,
            stage: FailStage::Check,
        }),
        Ok(None) => Step::Done(UpdateStatus::UpToDate),
        Ok(Some((version, update))) if auto_install => Step::Install { version, update },
        Ok(Some((version, update))) => Step::Offer { version, update },
    }
}

pub fn after_install(version: String, installed: Result<(), String>) -> UpdateStatus {
    match installed {
        Ok(()) => UpdateStatus::Ready { version },
        Err(message) => UpdateStatus::Failed {
            message,
            stage: FailStage::Install,
        },
    }
}

/// The status after an install was asked for and nothing is on offer. A
/// stale About tab can ask while an update is already installed or being
/// downloaded, and that state must survive.
pub fn after_nothing_to_install(current: &UpdateStatus) -> UpdateStatus {
    match current {
        UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Ready { .. } => {
            current.clone()
        }
        _ => UpdateStatus::Failed {
            message: NO_UPDATE_OFFERED.to_string(),
            stage: FailStage::Install,
        },
    }
}

/// A new check may start unless one is running or an update is already
/// installed. Asking again after an install would download the same file.
pub fn needs_check(status: &UpdateStatus) -> bool {
    !matches!(
        status,
        UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Ready { .. }
    )
}

/// The last background attempt, by the wall clock. A sleeping Mac's
/// monotonic clock stops, which would stretch "daily" to several days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastAttempt {
    pub age: Duration,
    /// Attempts in a row that ended in `Failed`. Zero after a success.
    pub failures: u32,
}

/// How long to wait after `failures` failures in a row.
fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(5);
    (RETRY_BASE * 2u32.pow(doublings)).min(CHECK_EVERY)
}

/// True when the background task should ask again. A failure, such as no
/// network, is retried sooner than a success, with a longer wait each time.
pub fn is_due(last: Option<LastAttempt>) -> bool {
    match last {
        None => true,
        Some(last) if last.failures == 0 => last.age >= CHECK_EVERY,
        Some(last) => last.age >= retry_delay(last.failures),
    }
}

/// Time since `at`. A clock that was set back makes the attempt look like it
/// is from the far future, which counts as long ago, so the check runs.
fn age_since(at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(at).unwrap_or(Duration::MAX)
}

/// Whether Picard may restart now. A restart during a cleanup would stop it
/// part way through.
pub fn restart_blocker(status: &UpdateStatus, cleaning: bool) -> Option<&'static str> {
    if !matches!(status, UpdateStatus::Ready { .. }) {
        Some(NOT_READY)
    } else if cleaning {
        Some(CLEANING)
    } else {
        None
    }
}

/// Why Picard cannot replace itself from `exe`, or `None` if it can. The
/// plugin swaps the folder the program is in with the new one. Outside
/// `Picard.app/Contents/MacOS` that folder is a build folder, so nothing is
/// checked or installed from there.
pub fn bundle_blocker(exe: &Path) -> Option<&'static str> {
    let text = exe.to_string_lossy();
    if !text.contains(".app/Contents/MacOS/") {
        Some(NOT_AN_APP)
    } else if text.starts_with("/Volumes/") || text.contains("/AppTranslocation/") {
        Some(READ_ONLY_LOCATION)
    } else {
        None
    }
}

/// True if this user can add and remove files in `dir`.
fn can_write(dir: &Path) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Why a download from this address is refused, or `None` if it is fine. The
/// signature protects what is installed. This protects what is requested.
pub fn download_blocker(scheme: &str, host: Option<&str>) -> Option<String> {
    let host = host.unwrap_or("");
    if scheme != "https" || !DOWNLOAD_HOSTS.contains(&host) {
        Some(format!(
            "the update points to {scheme}://{host}, which is not a GitHub release download"
        ))
    } else {
        None
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The values are replaced whole, so a panic elsewhere cannot leave them half-written.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The update state, shared by the background task, the window and the menu.
/// Managed by Tauri as `Arc<Updates>`.
pub struct Updates {
    status: Mutex<UpdateStatus>,
    /// The update offered but not installed. Set only with `Available`.
    offered: Mutex<Option<Update>>,
    /// Held for the length of a check or an install, so two cannot overlap.
    busy: tokio::sync::Mutex<()>,
    /// Weak, so this does not keep the poll loop's channel open after the
    /// file watchers are gone.
    wake: mpsc::WeakSender<Wake>,
    /// Pinged when the config is reloaded, so turning `auto_update` on takes
    /// effect now and not at the next hourly look.
    config_reloaded: Notify,
}

impl Updates {
    pub fn new(wake: mpsc::WeakSender<Wake>) -> Self {
        Self {
            status: Mutex::new(UpdateStatus::Idle),
            offered: Mutex::new(None),
            busy: tokio::sync::Mutex::new(()),
            wake,
            config_reloaded: Notify::new(),
        }
    }

    pub fn status(&self) -> UpdateStatus {
        lock(&self.status).clone()
    }

    /// Records `status`, tells the open window, and, when an update has just
    /// been installed, wakes the poll loop so the menu offers the restart.
    fn set<R: Runtime>(&self, app: &AppHandle<R>, status: UpdateStatus) {
        log::info!("update status: {status:?}");
        let ready = matches!(status, UpdateStatus::Ready { .. });
        *lock(&self.status) = status.clone();
        if let Err(e) = windows::update_status_changed(app, &status) {
            log::warn!("could not tell the window the update status: {e}");
        }
        if ready {
            match self.wake.upgrade() {
                Some(tx) => {
                    if let Err(e) = tx.try_send(Wake::UpdateReady) {
                        log::warn!("could not wake the menu for the update: {e}");
                    }
                }
                None => log::warn!("the poll loop has stopped; the menu will not show the update"),
            }
        }
    }

    /// Asks for a newer version and, when `auto_install` is set, installs it.
    /// Returns the status afterwards. If a check or install is already
    /// running, or an update is already installed, returns that state.
    pub async fn check<R: Runtime>(&self, app: &AppHandle<R>, auto_install: bool) -> UpdateStatus {
        let Ok(_busy) = self.busy.try_lock() else {
            return self.status();
        };
        let current = self.status();
        if !needs_check(&current) {
            return current;
        }
        self.set(app, UpdateStatus::Checking);
        // Installing into a folder the user cannot write to makes macOS ask
        // for an administrator password. That must follow a click, not a timer.
        let auto_install = auto_install && install_folder_is_writable();
        match after_check(fetch(app).await, auto_install) {
            Step::Done(status) => {
                *lock(&self.offered) = None;
                self.set(app, status);
            }
            Step::Offer { version, update } => {
                *lock(&self.offered) = Some(update);
                self.set(app, UpdateStatus::Available { version });
            }
            Step::Install { version, update } => {
                *lock(&self.offered) = None;
                self.install(app, &update, version).await;
            }
        }
        self.status()
    }

    /// Installs the update that `check` offered.
    pub async fn install_offered<R: Runtime>(&self, app: &AppHandle<R>) -> UpdateStatus {
        let Ok(_busy) = self.busy.try_lock() else {
            return self.status();
        };
        let offered = lock(&self.offered).take();
        match offered {
            Some(update) => {
                let version = update.version.clone();
                self.install(app, &update, version).await;
            }
            None => {
                let current = self.status();
                let next = after_nothing_to_install(&current);
                if next != current {
                    self.set(app, next);
                }
            }
        }
        self.status()
    }

    async fn install<R: Runtime>(&self, app: &AppHandle<R>, update: &Update, version: String) {
        self.set(
            app,
            UpdateStatus::Downloading {
                version: version.clone(),
            },
        );
        let installed = update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
            .map_err(|e| format!("{e} (version {version})"));
        self.set(app, after_install(version, installed));
    }

    /// Restarts Picard into the installed version. Does not return on success.
    ///
    /// A cleanup could start between the check and the exit. The window is
    /// microseconds, and Quit in the menu has no check at all.
    pub fn restart<R: Runtime>(&self, app: &AppHandle<R>, shared: &Shared) -> Result<(), String> {
        if let Some(reason) = restart_blocker(&self.status(), shared.is_cleaning()) {
            return Err(reason.to_string());
        }
        log::info!("restarting to finish the update");
        app.restart()
    }
}

/// The version waiting for a restart, for the menu.
pub fn ready_version<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let updates = app.try_state::<Arc<Updates>>()?;
    match updates.status() {
        UpdateStatus::Ready { version } => Some(version),
        _ => None,
    }
}

/// Tells the background task that the config was reloaded.
pub fn config_reloaded<R: Runtime>(app: &AppHandle<R>) {
    if let Some(updates) = app.try_state::<Arc<Updates>>() {
        updates.config_reloaded.notify_one();
    }
}

/// True if replacing `Picard.app` needs no administrator password. The
/// installer swaps the bundle with the new one, which needs write access to
/// the folder holding it and to the bundle itself, and a bundle owned by
/// someone else fails even when the folder is writable.
fn can_replace_without_password(exe: &Path) -> bool {
    let Some(bundle) = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
    else {
        return false;
    };
    let Some(parent) = bundle.parent() else {
        return false;
    };
    // SAFETY: geteuid has no arguments and cannot fail.
    let me = unsafe { libc::geteuid() };
    let owned = std::fs::metadata(bundle).is_ok_and(|m| m.uid() == me);
    can_write(parent) && can_write(bundle) && owned
}

fn install_folder_is_writable() -> bool {
    let writable = std::env::current_exe()
        .ok()
        .is_some_and(|exe| can_replace_without_password(&exe));
    if !writable {
        log::info!("Picard cannot replace itself without a password; updates wait for a click");
    }
    writable
}

async fn fetch<R: Runtime>(app: &AppHandle<R>) -> Result<Option<(String, Update)>, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot find the running program: {e}"))?;
    if let Some(reason) = bundle_blocker(&exe) {
        return Err(reason.to_string());
    }
    let updater = app
        .updater_builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("the updater did not start: {e}"))?;
    let found = tokio::time::timeout(CHECK_TIMEOUT, updater.check())
        .await
        .map_err(|_| "the update server did not answer in time".to_string())?
        .map_err(|e| e.to_string())?;
    let Some(mut update) = found else {
        return Ok(None);
    };
    if let Some(reason) =
        download_blocker(update.download_url.scheme(), update.download_url.host_str())
    {
        return Err(reason);
    }
    // The builder's timeout covers only the check. Without this, a stalled
    // download would hold the busy lock until Picard restarts.
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    Ok(Some((update.version.clone(), update)))
}

/// Whether Picard may install updates without being asked. A config that
/// cannot be read counts as no: the setting is unknown, so nothing installs
/// unattended, and the About tab offers the install instead.
pub async fn auto_install_enabled(shared: &Shared) -> bool {
    let path = shared.config_path.clone();
    let home = shared.home.clone();
    match tauri::async_runtime::spawn_blocking(move || config::load(&path, &home)).await {
        Ok(Ok(cfg)) => cfg.auto_update,
        Ok(Err(e)) => {
            log::warn!("not installing updates by itself while the config has an error: {e:#}");
            false
        }
        Err(e) => {
            log::warn!("not installing updates by itself: reading the config failed: {e}");
            false
        }
    }
}

/// Checks at start and then daily, and installs what it finds, while
/// `auto_update` is on. Runs until the app exits. Does nothing when Picard
/// was not started from an installed `Picard.app`, such as under `make dev`.
pub async fn run_in_background<R: Runtime>(
    app: AppHandle<R>,
    shared: Arc<Shared>,
    updates: Arc<Updates>,
) {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Some(reason) = bundle_blocker(&exe) {
                log::info!("automatic updates are off: {reason}");
                return;
            }
        }
        Err(e) => {
            log::warn!("automatic updates are off: cannot find the running program: {e}");
            return;
        }
    }
    tokio::time::sleep(STARTUP_DELAY).await;
    let mut last: Option<(SystemTime, u32)> = None;
    loop {
        let attempt = last.map(|(at, failures)| LastAttempt {
            age: age_since(at, SystemTime::now()),
            failures,
        });
        if is_due(attempt) && auto_install_enabled(&shared).await {
            let status = updates.check(&app, true).await;
            let failures = match status {
                UpdateStatus::Failed { .. } => last.map_or(0, |(_, n)| n) + 1,
                _ => 0,
            };
            last = Some((SystemTime::now(), failures));
        }
        // Waits for the next look, or less if the config was reloaded.
        let _ = tokio::time::timeout(TICK, updates.config_reloaded.notified()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn found(version: &str) -> Result<Option<(String, u8)>, String> {
        Ok(Some((version.to_string(), 7)))
    }

    fn failed(message: &str, stage: FailStage) -> UpdateStatus {
        UpdateStatus::Failed {
            message: message.into(),
            stage,
        }
    }

    fn ready() -> UpdateStatus {
        UpdateStatus::Ready {
            version: "0.4.0".into(),
        }
    }

    #[test]
    fn a_failed_check_reports_the_reason() {
        let step = after_check::<u8>(Err("no network".into()), true);
        assert_eq!(step, Step::Done(failed("no network", FailStage::Check)));
    }

    #[test]
    fn no_newer_version_means_up_to_date() {
        assert_eq!(
            after_check::<u8>(Ok(None), true),
            Step::Done(UpdateStatus::UpToDate)
        );
    }

    #[test]
    fn a_newer_version_installs_when_automatic() {
        assert_eq!(
            after_check(found("0.4.0"), true),
            Step::Install {
                version: "0.4.0".into(),
                update: 7
            }
        );
    }

    #[test]
    fn a_newer_version_is_offered_when_not_automatic() {
        assert_eq!(
            after_check(found("0.4.0"), false),
            Step::Offer {
                version: "0.4.0".into(),
                update: 7
            }
        );
    }

    #[test]
    fn an_install_ends_ready_or_failed_in_the_install_stage() {
        assert_eq!(after_install("0.4.0".into(), Ok(())), ready());
        assert_eq!(
            after_install("0.4.0".into(), Err("bad signature".into())),
            failed("bad signature", FailStage::Install)
        );
    }

    #[test]
    fn asking_to_install_with_nothing_on_offer_keeps_a_running_or_finished_update() {
        let downloading = UpdateStatus::Downloading {
            version: "0.4.0".into(),
        };
        for status in [UpdateStatus::Checking, downloading, ready()] {
            assert_eq!(after_nothing_to_install(&status), status);
        }
        for status in [
            UpdateStatus::Idle,
            UpdateStatus::UpToDate,
            failed("x", FailStage::Check),
        ] {
            assert_eq!(
                after_nothing_to_install(&status),
                failed(NO_UPDATE_OFFERED, FailStage::Install),
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_check_is_skipped_while_busy_or_once_installed() {
        let v = || "0.4.0".to_string();
        assert!(needs_check(&UpdateStatus::Idle));
        assert!(needs_check(&UpdateStatus::UpToDate));
        assert!(needs_check(&UpdateStatus::Available { version: v() }));
        assert!(needs_check(&failed("x", FailStage::Install)));
        assert!(!needs_check(&UpdateStatus::Checking));
        assert!(!needs_check(&UpdateStatus::Downloading { version: v() }));
        assert!(!needs_check(&ready()));
    }

    fn due(secs: u64, failures: u32) -> bool {
        is_due(Some(LastAttempt {
            age: Duration::from_secs(secs),
            failures,
        }))
    }

    #[test]
    fn the_first_check_is_due_at_once_and_then_daily() {
        assert!(is_due(None));
        assert!(!due(60, 0));
        assert!(!due(24 * 3600 - 1, 0));
        assert!(due(24 * 3600, 0));
    }

    #[test]
    fn failed_checks_back_off_from_one_hour_up_to_a_day() {
        let hours = |n: u64| n * 3600;
        assert!(!due(hours(1) - 1, 1));
        assert!(due(hours(1), 1));
        assert!(!due(hours(2) - 1, 2));
        assert!(due(hours(2), 2));
        assert!(due(hours(4), 3));
        assert!(!due(hours(8) - 1, 4));
        // Many failures in a row never wait longer than a success does.
        assert_eq!(retry_delay(40), CHECK_EVERY);
        assert!(due(hours(24), 40));
    }

    #[test]
    fn a_clock_set_back_counts_as_long_ago() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let later = now + Duration::from_secs(500);
        assert_eq!(age_since(now, later), Duration::from_secs(500));
        assert_eq!(age_since(later, now), Duration::MAX);
        assert!(is_due(Some(LastAttempt {
            age: age_since(later, now),
            failures: 0
        })));
    }

    #[test]
    fn a_restart_needs_an_installed_update_and_no_cleanup() {
        assert_eq!(restart_blocker(&ready(), false), None);
        assert_eq!(restart_blocker(&ready(), true), Some(CLEANING));
        for status in [
            UpdateStatus::Idle,
            UpdateStatus::UpToDate,
            UpdateStatus::Available {
                version: "0.4.0".into(),
            },
        ] {
            assert_eq!(
                restart_blocker(&status, false),
                Some(NOT_READY),
                "{status:?}"
            );
        }
    }

    #[test]
    fn only_an_installed_app_bundle_may_replace_itself() {
        let ok = Path::new("/Applications/Picard.app/Contents/MacOS/picard");
        assert_eq!(bundle_blocker(ok), None);
        assert_eq!(
            bundle_blocker(Path::new(
                "/Users/me/Applications/Picard.app/Contents/MacOS/picard"
            )),
            None
        );
        for dev in [
            "/work/picard/src-tauri/target/debug/picard",
            "/work/picard/src-tauri/target/release/picard",
            "/usr/local/bin/picard",
        ] {
            assert_eq!(bundle_blocker(Path::new(dev)), Some(NOT_AN_APP), "{dev}");
        }
        for read_only in [
            "/Volumes/Picard 0.4.0/Picard.app/Contents/MacOS/picard",
            "/private/var/folders/x/AppTranslocation/ABC/d/Picard.app/Contents/MacOS/picard",
        ] {
            assert_eq!(
                bundle_blocker(Path::new(read_only)),
                Some(READ_ONLY_LOCATION),
                "{read_only}"
            );
        }
    }

    #[test]
    fn a_folder_is_writable_only_if_the_user_can_write_to_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(can_write(dir.path()));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root can write anywhere, so this half only means something for a normal user.
        if unsafe { libc::geteuid() } != 0 {
            assert!(!can_write(dir.path()));
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!can_write(&PathBuf::from("/definitely/not/here")));
    }

    #[test]
    fn an_app_can_be_replaced_only_when_its_folder_and_bundle_are_the_users() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Picard.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        let exe = bundle.join("Contents/MacOS/picard");
        assert!(can_replace_without_password(&exe));

        // Not an app bundle at all.
        assert!(!can_replace_without_password(&dir.path().join("picard")));

        // Root can write anywhere, so the read-only cases mean something only for a normal user.
        if unsafe { libc::geteuid() } != 0 {
            std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o555)).unwrap();
            assert!(!can_replace_without_password(&exe), "read-only bundle");
            std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            assert!(!can_replace_without_password(&exe), "read-only folder");
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn shared_with_config(text: Option<&str>) -> (tempfile::TempDir, Shared) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        if let Some(text) = text {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        let shared = Shared::new(dir.path().to_path_buf(), path);
        (dir, shared)
    }

    #[test]
    fn automatic_installs_follow_the_config_and_fail_closed() {
        let enabled = |text: Option<&str>| {
            let (_dir, shared) = shared_with_config(text);
            tauri::async_runtime::block_on(auto_install_enabled(&shared))
        };
        let base = "min_free_gb = 1\nroots = [\"/a\"]\n";
        assert!(enabled(Some(base)));
        assert!(enabled(Some(&format!("auto_update = true\n{base}"))));
        assert!(!enabled(Some(&format!("auto_update = false\n{base}"))));
        // A broken config and a missing one mean the setting is unknown.
        assert!(!enabled(Some("auto_update = \"yes\"\n")));
        assert!(!enabled(None));
    }

    #[test]
    fn downloads_must_be_https_from_a_github_release_host() {
        for host in DOWNLOAD_HOSTS {
            assert_eq!(download_blocker("https", Some(host)), None, "{host}");
        }
        assert!(download_blocker("http", Some("github.com")).is_some());
        assert!(download_blocker("https", Some("evil.example")).is_some());
        assert!(download_blocker("https", Some("github.com.evil.example")).is_some());
        assert!(download_blocker("https", Some("127.0.0.1")).is_some());
        assert!(download_blocker("https", None).is_some());
        assert!(download_blocker("file", None).is_some());
    }

    #[test]
    fn the_status_reaches_the_window_as_tagged_json() {
        let json = |s: &UpdateStatus| serde_json::to_value(s).unwrap();
        assert_eq!(
            json(&UpdateStatus::Idle),
            serde_json::json!({"state": "idle"})
        );
        assert_eq!(
            json(&ready()),
            serde_json::json!({"state": "ready", "version": "0.4.0"})
        );
        assert_eq!(
            json(&failed("x", FailStage::Install)),
            serde_json::json!({"state": "failed", "message": "x", "stage": "install"})
        );
        assert_eq!(
            json(&failed("x", FailStage::Check)),
            serde_json::json!({"state": "failed", "message": "x", "stage": "check"})
        );
    }
}
