//! State shared by the poll loop and the window commands.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

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
        }
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
