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

use super::Stop;

#[derive(Default)]
pub struct SizeCache {
    entries: Mutex<HashMap<PathBuf, (SystemTime, u64)>>,
}

impl SizeCache {
    /// Measures every directory in parallel. Returns sizes in the same order.
    /// Once `stop` returns true, the rest read as zero.
    pub fn measure_all(&self, dirs: &[PathBuf], stop: Stop) -> Vec<u64> {
        dirs.par_iter().map(|d| self.measure(d, stop)).collect()
    }

    /// Bytes on disk used by `dir`, from the cache when its mtime is unchanged.
    /// Zero if `stop` cut the count short; an unfinished count is not cached.
    pub fn measure(&self, dir: &Path, stop: Stop) -> u64 {
        let mtime = std::fs::symlink_metadata(dir).and_then(|m| m.modified());
        let Ok(mtime) = mtime else {
            return disk_usage(dir, stop).unwrap_or(0);
        };
        if let Some(&(cached_mtime, bytes)) = self.lock().get(dir)
            && cached_mtime == mtime
        {
            return bytes;
        }
        let Some(bytes) = disk_usage(dir, stop) else {
            return 0;
        };
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
/// counted as links, never followed. Unreadable entries are skipped. `None`
/// once `stop` returns true, since a large tree can take many seconds.
pub fn disk_usage(dir: &Path, stop: Stop) -> Option<u64> {
    let mut total = 0;
    for entry in WalkDir::new(dir).follow_links(false) {
        if stop() {
            return None;
        }
        if let Some(m) = entry.ok().and_then(|e| e.metadata().ok()) {
            total += m.blocks() * 512;
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO: Stop = &|| false;

    #[test]
    fn counts_allocated_bytes_and_caches_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("nm");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("a"), vec![0u8; 100_000]).unwrap();

        let cache = SizeCache::default();
        let first = cache.measure(&d, GO);
        assert!(first >= 100_000, "{first}");

        // Growing a file inside does not change the folder's mtime, so the cache holds.
        std::fs::write(d.join("a"), vec![0u8; 300_000]).unwrap();
        assert_eq!(cache.measure(&d, GO), first);

        // Adding an entry changes the folder's mtime and invalidates the cache.
        std::fs::write(d.join("b"), vec![0u8; 10]).unwrap();
        assert!(cache.measure(&d, GO) >= 300_000);
    }

    #[test]
    fn missing_dir_is_zero() {
        assert_eq!(disk_usage(Path::new("/definitely/not/here"), GO), Some(0));
    }

    #[test]
    fn a_stopped_count_is_neither_returned_nor_cached() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100_000]).unwrap();
        assert_eq!(disk_usage(dir.path(), &|| true), None);
        let cache = SizeCache::default();
        assert_eq!(cache.measure(dir.path(), &|| true), 0);
        assert!(
            cache.measure(dir.path(), GO) >= 100_000,
            "the zero was not cached"
        );
    }
}
