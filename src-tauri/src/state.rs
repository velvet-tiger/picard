//! State shared by the poll loop and the window commands.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use tauri::{AppHandle, Runtime};

use crate::commands::VolumePreview;
use crate::tray::{self, TrayState};

use crate::env::ToolEnv;
use crate::runner::DataPaths;
use crate::tasks::SizeCache;
use crate::volume::UuidCache;

/// Everything the poll loop and the Settings/Preview commands both need.
/// Managed by Tauri as `Arc<Shared>`.
pub struct Shared {
    pub home: PathBuf,
    pub config_path: PathBuf,
    pub paths: DataPaths,
    pub sizes: Arc<SizeCache>,
    pub uuids: Arc<UuidCache>,
    /// Set once, shortly after launch: asking the login shell takes about a second.
    tools: OnceLock<Arc<ToolEnv>>,
    /// Cleanups in progress (auto mode and the Preview window can overlap
    /// briefly before the run lock refuses one).
    cleaning: AtomicUsize,
    /// Whether the last check found any volume below its threshold.
    low: AtomicBool,
    /// Held across every read, edit and write of the config file from the
    /// window, so two quick edits (Ignore on two rows, or Ignore and Save)
    /// cannot both start from the old text and lose one of them.
    config_write: Mutex<()>,
    /// The latest preview scan of each volume, from a Scan in the Clean tab or
    /// from the estimate behind a notification. The Clean tab opens on it, so
    /// a notification's numbers have a list behind them. Memory only.
    previews: Mutex<Vec<VolumePreview>>,
}

impl Shared {
    pub fn new(home: PathBuf, config_path: PathBuf) -> Self {
        Self {
            paths: DataPaths::beside(&config_path),
            home,
            config_path,
            sizes: Arc::new(SizeCache::default()),
            uuids: Arc::new(UuidCache::default()),
            tools: OnceLock::new(),
            cleaning: AtomicUsize::new(0),
            low: AtomicBool::new(false),
            config_write: Mutex::new(()),
            previews: Mutex::new(Vec::new()),
        }
    }

    /// Takes the config write lock. See `config_write`. A panic in another
    /// writer leaves the file as it was, so a poisoned lock is still usable.
    pub fn lock_config(&self) -> MutexGuard<'_, ()> {
        self.config_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the latest low/not-low reading and updates the menu bar icon.
    pub fn set_low<R: Runtime>(&self, app: &AppHandle<R>, low: bool) {
        self.low.store(low, Ordering::SeqCst);
        self.refresh_tray(app);
    }

    /// Marks a cleanup as running until the returned guard is dropped, which
    /// also happens if the cleanup panics, so the icon cannot stay stuck.
    pub fn start_cleaning<R: Runtime>(self: &Arc<Self>, app: &AppHandle<R>) -> CleaningGuard<R> {
        self.cleaning.fetch_add(1, Ordering::SeqCst);
        self.refresh_tray(app);
        CleaningGuard {
            shared: Arc::clone(self),
            app: app.clone(),
        }
    }

    /// True while any cleanup started with `start_cleaning` is running.
    pub fn is_cleaning(&self) -> bool {
        self.cleaning.load(Ordering::SeqCst) > 0
    }

    fn refresh_tray<R: Runtime>(&self, app: &AppHandle<R>) {
        let state = TrayState::from_flags(self.is_cleaning(), self.low.load(Ordering::SeqCst));
        tray::set_state(app, state);
    }

    /// The stored preview of every volume. See `previews`.
    pub fn previews(&self) -> Vec<VolumePreview> {
        self.lock_previews().clone()
    }

    /// Stores a scan of every mounted volume. See `with_previews_replaced`.
    pub fn replace_previews(&self, fresh: Vec<VolumePreview>) {
        let mut stored = self.lock_previews();
        *stored = with_previews_replaced(std::mem::take(&mut *stored), fresh);
    }

    /// Stores a scan of one volume. See `with_preview`.
    pub fn record_preview(&self, fresh: VolumePreview) {
        let mut stored = self.lock_previews();
        *stored = with_preview(std::mem::take(&mut *stored), fresh);
    }

    /// Drops the stored preview of the volume at `mount`, because a clean
    /// there may have removed folders it lists.
    pub fn forget_preview(&self, mount: &str) {
        self.lock_previews().retain(|v| v.mount != mount);
    }

    /// A panic while holding the lock cannot leave a half-written list, since
    /// every write swaps in a finished one, so a poisoned lock is still usable.
    fn lock_previews(&self) -> MutexGuard<'_, Vec<VolumePreview>> {
        self.previews.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The resolved tool PATH, or `None` while it is still being resolved.
    pub fn tools(&self) -> Option<Arc<ToolEnv>> {
        self.tools.get().cloned()
    }

    /// Records the resolved tool PATH. Later calls are ignored.
    pub fn set_tools(&self, env: ToolEnv) {
        if self.tools.set(Arc::new(env)).is_err() {
            log::warn!("tool PATH was already resolved; keeping the first result");
        }
    }
}

/// `stored` with `fresh` in place of the entry for the same mount. A stored
/// entry from a scan that started later wins, because two scans can finish in
/// either order.
fn with_preview(mut stored: Vec<VolumePreview>, fresh: VolumePreview) -> Vec<VolumePreview> {
    match stored.iter_mut().find(|v| v.mount == fresh.mount) {
        Some(old) if old.scanned_at > fresh.scanned_at => {}
        Some(old) => *old = fresh,
        None => stored.push(fresh),
    }
    stored
}

/// `fresh`, a scan of every mounted volume, in place of `stored`. A volume
/// missing from `fresh` is no longer mounted, so it is dropped. A stored entry
/// from a scan that started later is kept, as in `with_preview`.
fn with_previews_replaced(
    stored: Vec<VolumePreview>,
    fresh: Vec<VolumePreview>,
) -> Vec<VolumePreview> {
    fresh
        .into_iter()
        .map(
            |new| match stored.iter().find(|old| old.mount == new.mount) {
                Some(old) if old.scanned_at > new.scanned_at => old.clone(),
                _ => new,
            },
        )
        .collect()
}

/// Ends one "cleaning" mark when dropped. See `Shared::start_cleaning`.
pub struct CleaningGuard<R: Runtime> {
    shared: Arc<Shared>,
    app: AppHandle<R>,
}

impl<R: Runtime> Drop for CleaningGuard<R> {
    fn drop(&mut self) {
        self.shared.cleaning.fetch_sub(1, Ordering::SeqCst);
        self.shared.refresh_tray(&self.app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 22, 9, minute, 0).unwrap()
    }

    fn preview(mount: &str, minute: u32) -> VolumePreview {
        VolumePreview {
            name: mount.to_string(),
            mount: mount.to_string(),
            free_text: String::new(),
            tasks: Vec::new(),
            total_bytes: 0,
            total_text: String::new(),
            scanned_at: at(minute),
        }
    }

    fn mounts_and_minutes(list: &[VolumePreview]) -> Vec<(&str, DateTime<Utc>)> {
        list.iter()
            .map(|v| (v.mount.as_str(), v.scanned_at))
            .collect()
    }

    #[test]
    fn a_volume_scan_replaces_only_its_own_volume() {
        let stored = vec![preview("/", 1), preview("/Volumes/Work", 1)];
        let merged = with_preview(stored, preview("/Volumes/Work", 5));
        assert_eq!(
            mounts_and_minutes(&merged),
            vec![("/", at(1)), ("/Volumes/Work", at(5))]
        );
    }

    #[test]
    fn a_volume_scan_of_a_new_volume_is_added() {
        let merged = with_preview(vec![preview("/", 1)], preview("/Volumes/Work", 5));
        assert_eq!(
            mounts_and_minutes(&merged),
            vec![("/", at(1)), ("/Volumes/Work", at(5))]
        );
    }

    #[test]
    fn a_scan_that_started_earlier_does_not_replace_a_later_one() {
        let merged = with_preview(vec![preview("/", 5)], preview("/", 1));
        assert_eq!(mounts_and_minutes(&merged), vec![("/", at(5))]);
        let replaced = with_previews_replaced(vec![preview("/", 5)], vec![preview("/", 1)]);
        assert_eq!(mounts_and_minutes(&replaced), vec![("/", at(5))]);
    }

    #[test]
    fn a_full_scan_drops_volumes_that_are_no_longer_mounted() {
        let stored = vec![preview("/", 1), preview("/Volumes/Gone", 1)];
        let replaced = with_previews_replaced(stored, vec![preview("/", 5)]);
        assert_eq!(mounts_and_minutes(&replaced), vec![("/", at(5))]);
    }

    #[test]
    fn forget_drops_one_volume_and_keeps_the_rest() {
        let shared = Shared::new(PathBuf::from("/u"), PathBuf::from("/u/config.toml"));
        shared.replace_previews(vec![preview("/", 1), preview("/Volumes/Work", 1)]);
        shared.forget_preview("/");
        assert_eq!(
            mounts_and_minutes(&shared.previews()),
            vec![("/Volumes/Work", at(1))]
        );
    }
}
