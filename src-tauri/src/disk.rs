//! Free-space queries for a mounted volume.
//!
//! Uses `statfs` instead of `statvfs`: on macOS `statvfs` reports block counts as
//! 32-bit `fsblkcnt_t`, while `statfs` uses 64-bit counts. `f_bavail` (blocks
//! available to unprivileged users) is the conservative "free" figure. It does not
//! include APFS purgeable space, so it can read lower than Finder.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Free and total capacity of the volume that holds a path, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// Reads capacity for the volume containing `path`.
///
/// Fails with the OS error if the path does not exist or the volume cannot be
/// queried (for example, an external drive that was just unplugged).
pub fn query(path: &Path) -> io::Result<DiskSpace> {
    let stat = statfs(path)?;
    let block_size = u64::from(stat.f_bsize);
    Ok(DiskSpace {
        free_bytes: stat.f_bavail.saturating_mul(block_size),
        total_bytes: stat.f_blocks.saturating_mul(block_size),
    })
}

/// Returns the mount point of the volume containing `path`, for example `/` or
/// `/Volumes/Projects`.
pub fn mount_point(path: &Path) -> io::Result<PathBuf> {
    let stat = statfs(path)?;
    // SAFETY: the kernel writes a NUL-terminated string into f_mntonname, which
    // is MAXPATHLEN bytes long.
    let name = unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) };
    Ok(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}

fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} contains a NUL byte: {e}", path.display()),
        )
    })?;

    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: c_path is a valid NUL-terminated string and stat points to writable
    // memory of the right size. statfs fully initialises stat when it returns 0.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("statfs({}) failed: {err}", path.display()),
        ));
    }
    // SAFETY: rc == 0, so statfs initialised the struct.
    Ok(unsafe { stat.assume_init() })
}

const MB: f64 = 1_000_000.0;
const GB: f64 = 1_000_000_000.0;
const TB: f64 = 1_000_000_000_000.0;

/// Formats a byte count in decimal units, matching how Finder reports capacity.
///
/// Values under 10 GB keep one decimal place, because "3 GB" versus "3.4 GB"
/// matters when space is tight. Values under 1 GB switch to whole MB so small
/// folders do not read as "0.0 GB". Values of 1 TB or more switch to TB.
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= 10.0 * GB {
        format!("{:.0} GB", b / GB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

/// Builds the tray menu's status line, for example "112 GB free (23%)".
///
/// A zero total cannot give a percentage, so the percentage is left out rather
/// than shown as a misleading 0% or NaN.
pub fn format_free(space: DiskSpace) -> String {
    let free = format_bytes(space.free_bytes);
    if space.total_bytes == 0 {
        return format!("{free} free");
    }
    let pct = (space.free_bytes as f64 / space.total_bytes as f64) * 100.0;
    format!("{free} free ({pct:.0}%)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(free: u64, total: u64) -> DiskSpace {
        DiskSpace {
            free_bytes: free,
            total_bytes: total,
        }
    }

    #[test]
    fn formats_whole_gigabytes_with_percent() {
        assert_eq!(
            format_free(space(112_000_000_000, 487_000_000_000)),
            "112 GB free (23%)"
        );
    }

    #[test]
    fn rounds_gigabytes_and_percent() {
        assert_eq!(
            format_free(space(112_600_000_000, 1_000_000_000_000 - 1)),
            "113 GB free (11%)"
        );
    }

    #[test]
    fn keeps_one_decimal_below_ten_gigabytes() {
        assert_eq!(format_bytes(3_400_000_000), "3.4 GB");
        assert_eq!(format_bytes(9_940_000_000), "9.9 GB");
        assert_eq!(format_bytes(10_000_000_000), "10 GB");
    }

    #[test]
    fn switches_to_megabytes_below_one_gigabyte() {
        assert_eq!(format_bytes(3_002_368), "3 MB");
        assert_eq!(format_bytes(203_000_000), "203 MB");
        assert_eq!(format_bytes(1_000_000_000), "1.0 GB");
    }

    #[test]
    fn switches_to_terabytes() {
        assert_eq!(format_bytes(1_000_000_000_000), "1.0 TB");
        assert_eq!(format_bytes(2_460_000_000_000), "2.5 TB");
    }

    #[test]
    fn zero_bytes() {
        assert_eq!(format_free(space(0, 500_000_000_000)), "0 MB free (0%)");
    }

    #[test]
    fn zero_total_omits_percent() {
        assert_eq!(format_free(space(0, 0)), "0 MB free");
    }

    #[test]
    fn queries_root_volume() {
        let s = query(Path::new("/")).expect("statfs on / should succeed");
        assert!(s.total_bytes > 0, "total should be non-zero: {s:?}");
        assert!(s.free_bytes <= s.total_bytes, "free exceeds total: {s:?}");
    }

    #[test]
    fn mount_point_of_root_is_root() {
        assert_eq!(mount_point(Path::new("/")).unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn missing_path_is_an_error_naming_the_path() {
        let err = query(Path::new("/definitely/not/a/real/path")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("/definitely/not/a/real/path"));
    }
}
