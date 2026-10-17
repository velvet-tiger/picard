//! Measures directory sizes, in parallel and with a cache.
//!
//! Large `node_modules` trees are slow to measure, and the same folders are
//! measured on every scan. The cache key is the folder's own mtime, which
//! changes when entries are added or removed at its top level. Deep edits inside
//! an unchanged top level keep the old size; that is acceptable for an estimate.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use rayon::prelude::*;
use walkdir::WalkDir;

#[derive(Default)]
pub struct SizeCache {
    entries: Mutex<HashMap<PathBuf, (SystemTime, u64)>>,
}

impl SizeCache {
    /// Measures every directory in parallel. Returns sizes in the same order.
    pub fn measure_all(&self, dirs: &[PathBuf]) -> Vec<u64> {
        dirs.par_iter().map(|d| self.measure(d)).collect()
    }

    /// Bytes on disk used by `dir`, from the cache when its mtime is unchanged.
    pub fn measure(&self, dir: &Path) -> u64 {
        let mtime = std::fs::symlink_metadata(dir).and_then(|m| m.modified());
        let Ok(mtime) = mtime else {
            return disk_usage(dir);
        };
        if let Some(&(cached_mtime, bytes)) = self.lock().get(dir)
            && cached_mtime == mtime
        {
            return bytes;
        }
        let bytes = disk_usage(dir);
        self.lock().insert(dir.to_path_buf(), (mtime, bytes));
        bytes
    }

    // A panic while holding the lock leaves the map usable; at worst one entry
    // is stale, which only affects an estimate.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, (SystemTime, u64)>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Allocated bytes (512-byte blocks), which is what removal frees. Symlinks are
/// counted as links, never followed. Unreadable entries are skipped.
pub fn disk_usage(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.blocks() * 512)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_allocated_bytes_and_caches_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("nm");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("a"), vec![0u8; 100_000]).unwrap();

        let cache = SizeCache::default();
        let first = cache.measure(&d);
        assert!(first >= 100_000, "{first}");

        // Growing a file inside does not change the folder's mtime, so the cache holds.
        std::fs::write(d.join("a"), vec![0u8; 300_000]).unwrap();
        assert_eq!(cache.measure(&d), first);

        // Adding an entry changes the folder's mtime and invalidates the cache.
        std::fs::write(d.join("b"), vec![0u8; 10]).unwrap();
        assert!(cache.measure(&d) >= 300_000);
    }

    #[test]
    fn missing_dir_is_zero() {
        assert_eq!(disk_usage(Path::new("/definitely/not/here")), 0);
    }
}
