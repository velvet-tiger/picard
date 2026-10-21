//! State shared by the poll loop and the window commands.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::{DateTime, Utc};
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
    /// a notification's numbers have a list behind them. Memory only, so a
    /// clean from the CLI, which runs in another process, does not reach it;
    /// the Clean button re-checks every item, so that costs only stale numbers.
    previews: Mutex<PreviewStore>,
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
            previews: Mutex::new(PreviewStore::default()),
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
        self.lock_previews().list.clone()
    }

    /// Stores a scan of every mounted volume. See `PreviewStore::replace`.
    pub fn replace_previews(&self, fresh: Vec<VolumePreview>) {
        self.lock_previews().replace(fresh);
    }

    /// Stores a scan of one volume. See `PreviewStore::record`.
    pub fn record_preview(&self, fresh: VolumePreview) {
        self.lock_previews().record(fresh);
    }

    /// Drops the stored preview of the volume at `mount` after a clean there,
    /// and refuses any scan of it that started before now. Call it once the
    /// clean has finished.
    pub fn mark_cleaned(&self, mount: &str) {
        self.lock_previews().mark_cleaned(mount, Utc::now());
    }

    /// A panic while holding the lock cannot leave the store half-written:
    /// each write is one `retain`, `insert` or swap of a finished list, so a
    /// poisoned lock is still usable.
    fn lock_previews(&self) -> MutexGuard<'_, PreviewStore> {
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

/// The latest preview scan of each volume, and when each was last cleaned.
/// See `Shared::previews`.
#[derive(Debug, Default)]
struct PreviewStore {
    list: Vec<VolumePreview>,
    /// When the last clean of each volume finished, keyed by mount.
    cleaned: HashMap<String, DateTime<Utc>>,
}

impl PreviewStore {
    /// False for a scan that started before the last clean of its volume
    /// finished, since it may list folders that clean removed. Scans and
    /// cleans run at the same time, so such a scan can finish afterwards.
    fn is_current(&self, fresh: &VolumePreview) -> bool {
        self.cleaned
            .get(&fresh.mount)
            .is_none_or(|&cleaned| fresh.scanned_at >= cleaned)
    }

    /// Puts `fresh` in place of the entry for the same mount. A stored entry
    /// from a scan that started later wins, because two scans can finish in
    /// either order.
    fn record(&mut self, fresh: VolumePreview) {
        if !self.is_current(&fresh) {
            return;
        }
        match self.list.iter_mut().find(|v| v.mount == fresh.mount) {
            Some(old) if old.scanned_at > fresh.scanned_at => {}
            Some(old) => *old = fresh,
            None => self.list.push(fresh),
        }
    }

    /// Puts `fresh`, a scan of every mounted volume, in place of the list. A
    /// volume missing from `fresh` is no longer mounted, so it is dropped.
    /// Stale scans and later stored entries are handled as in `record`.
    fn replace(&mut self, fresh: Vec<VolumePreview>) {
        let old = std::mem::take(&mut self.list);
        let kept: Vec<VolumePreview> = fresh
            .into_iter()
            .filter(|new| self.is_current(new))
            .map(|new| match old.iter().find(|o| o.mount == new.mount) {
                Some(o) if o.scanned_at > new.scanned_at => o.clone(),
                _ => new,
            })
            .collect();
        self.list = kept;
    }

    /// Drops the entry for `mount` and refuses its scans that started before `at`.
    fn mark_cleaned(&mut self, mount: &str, at: DateTime<Utc>) {
        self.list.retain(|v| v.mount != mount);
        self.cleaned.insert(mount.to_string(), at);
    }
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
    use chrono::TimeZone;

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

    fn store(list: Vec<VolumePreview>) -> PreviewStore {
        PreviewStore {
            list,
            cleaned: HashMap::new(),
        }
    }

    fn mounts_and_times(store: &PreviewStore) -> Vec<(&str, DateTime<Utc>)> {
        store
            .list
            .iter()
            .map(|v| (v.mount.as_str(), v.scanned_at))
            .collect()
    }

    #[test]
    fn a_volume_scan_replaces_only_its_own_volume() {
        let mut s = store(vec![preview("/", 1), preview("/Volumes/Work", 1)]);
        s.record(preview("/Volumes/Work", 5));
        assert_eq!(
            mounts_and_times(&s),
            vec![("/", at(1)), ("/Volumes/Work", at(5))]
        );
    }

    #[test]
    fn a_volume_scan_of_a_new_volume_is_added() {
        let mut s = store(vec![preview("/", 1)]);
        s.record(preview("/Volumes/Work", 5));
        assert_eq!(
            mounts_and_times(&s),
            vec![("/", at(1)), ("/Volumes/Work", at(5))]
        );
    }

    #[test]
    fn a_scan_that_started_earlier_does_not_replace_a_later_one() {
        let mut s = store(vec![preview("/", 5)]);
        s.record(preview("/", 1));
        assert_eq!(mounts_and_times(&s), vec![("/", at(5))]);
        s.replace(vec![preview("/", 1)]);
        assert_eq!(mounts_and_times(&s), vec![("/", at(5))]);
    }

    #[test]
    fn a_full_scan_drops_volumes_that_are_no_longer_mounted() {
        let mut s = store(vec![preview("/", 1), preview("/Volumes/Gone", 1)]);
        s.replace(vec![preview("/", 5)]);
        assert_eq!(mounts_and_times(&s), vec![("/", at(5))]);
    }

    #[test]
    fn a_clean_drops_its_volume_and_keeps_the_rest() {
        let mut s = store(vec![preview("/", 1), preview("/Volumes/Work", 1)]);
        s.mark_cleaned("/", at(3));
        assert_eq!(mounts_and_times(&s), vec![("/Volumes/Work", at(1))]);
    }

    #[test]
    fn a_scan_that_started_before_a_clean_finished_is_refused() {
        // A notification's estimate starts at :2, a clean of the same volume
        // finishes at :3, and the estimate lands after it.
        let mut s = store(Vec::new());
        s.mark_cleaned("/", at(3));
        s.record(preview("/", 2));
        assert!(s.list.is_empty(), "the late estimate must not come back");
        s.replace(vec![preview("/", 2), preview("/Volumes/Work", 2)]);
        assert_eq!(mounts_and_times(&s), vec![("/Volumes/Work", at(2))]);
    }

    #[test]
    fn a_scan_that_started_after_a_clean_is_kept() {
        let mut s = store(Vec::new());
        s.mark_cleaned("/", at(3));
        s.record(preview("/", 4));
        assert_eq!(mounts_and_times(&s), vec![("/", at(4))]);
    }

    #[test]
    fn shared_previews_go_through_the_store() {
        let shared = Shared::new(PathBuf::from("/u"), PathBuf::from("/u/config.toml"));
        shared.replace_previews(vec![preview("/", 1), preview("/Volumes/Work", 1)]);
        shared.mark_cleaned("/");
        let left: Vec<String> = shared.previews().into_iter().map(|v| v.mount).collect();
        assert_eq!(left, vec!["/Volumes/Work".to_string()]);
        // Stamped with the real clock, so a scan from 2000 is stale.
        let mut old = preview("/", 1);
        old.scanned_at = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        shared.record_preview(old);
        assert_eq!(shared.previews().len(), 1);
    }
}
