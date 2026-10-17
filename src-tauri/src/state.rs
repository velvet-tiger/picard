//! State shared by the poll loop and the window commands.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tauri::{AppHandle, Runtime};

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
        }
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

    fn refresh_tray<R: Runtime>(&self, app: &AppHandle<R>) {
        let state = TrayState::from_flags(
            self.cleaning.load(Ordering::SeqCst) > 0,
            self.low.load(Ordering::SeqCst),
        );
        tray::set_state(app, state);
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
