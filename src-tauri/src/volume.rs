//! Maps configured roots to the volumes that hold them.
//!
//! Two roots belong to the same volume when they share an `st_dev`. A root that
//! does not exist is treated as "not mounted", which is the normal state for a
//! root on an external drive that is unplugged.
//!
//! TODO(M5): also read the volume UUID (`diskutil info -plist`) so a renamed or
//! remounted drive keeps its `[[volume]]` override. M2 matches by mount point only.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::disk;

/// Where a root lives, as reported by the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Mounted {
        dev: u64,
        mount: PathBuf,
    },
    /// The path does not exist. Usually an unplugged drive.
    Missing,
    /// The path exists but could not be read, for example permission denied.
    Unreadable(String),
}

/// A mounted volume that holds at least one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub dev: u64,
    pub mount: PathBuf,
    pub name: String,
    pub roots: Vec<PathBuf>,
}

/// A root that cannot be checked right now, with a short reason for the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableRoot {
    pub root: PathBuf,
    pub name: String,
    pub reason: String,
}

/// The result of mapping every root: volumes to monitor and roots to grey out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeMap {
    pub volumes: Vec<Volume>,
    pub unavailable: Vec<UnavailableRoot>,
}

/// Asks the filesystem where `root` lives.
pub fn locate(root: &Path) -> Location {
    let dev = match std::fs::metadata(root) {
        Ok(meta) => meta.dev(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Location::Missing,
        Err(e) => return Location::Unreadable(format!("{}: {e}", root.display())),
    };
    match disk::mount_point(root) {
        Ok(mount) => Location::Mounted { dev, mount },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Location::Missing,
        Err(e) => Location::Unreadable(e.to_string()),
    }
}

/// Groups roots by volume, keeping the order roots first appear in.
///
/// `locate` is passed in so the grouping can be tested without real mounts.
pub fn map_roots(roots: &[PathBuf], locate: impl Fn(&Path) -> Location) -> VolumeMap {
    let mut map = VolumeMap::default();
    for root in roots {
        match locate(root) {
            Location::Mounted { dev, mount } => {
                if let Some(v) = map.volumes.iter_mut().find(|v| v.dev == dev) {
                    v.roots.push(root.clone());
                } else {
                    map.volumes.push(Volume {
                        dev,
                        name: display_name(&mount),
                        mount,
                        roots: vec![root.clone()],
                    });
                }
            }
            Location::Missing => map.unavailable.push(UnavailableRoot {
                root: root.clone(),
                name: unmounted_name(root),
                reason: "not mounted".to_string(),
            }),
            Location::Unreadable(err) => {
                log::warn!("cannot read root {}: {err}", root.display());
                map.unavailable.push(UnavailableRoot {
                    root: root.clone(),
                    name: unmounted_name(root),
                    reason: "unreadable, see log".to_string(),
                });
            }
        }
    }
    map
}

/// A short name for the menu. The startup disk is mounted at `/` or, for the
/// APFS Data volume that holds home folders, `/System/Volumes/Data`.
fn display_name(mount: &Path) -> String {
    if mount == Path::new("/") || mount == Path::new(crate::config::STARTUP_DATA_MOUNT) {
        return "Startup disk".to_string();
    }
    mount.file_name().map_or_else(
        || mount.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Best guess at the volume name for a root that is not there: the folder
/// directly under `/Volumes`, or the root path itself.
fn unmounted_name(root: &Path) -> String {
    root.strip_prefix("/Volumes")
        .ok()
        .and_then(|rest| rest.components().next())
        .map_or_else(
            || root.display().to_string(),
            |c| c.as_os_str().to_string_lossy().into_owned(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted(dev: u64, mount: &str) -> Location {
        Location::Mounted {
            dev,
            mount: PathBuf::from(mount),
        }
    }

    #[test]
    fn groups_roots_on_the_same_device() {
        let roots = vec![
            PathBuf::from("/Users/me/working"),
            PathBuf::from("/Volumes/Projects/code"),
            PathBuf::from("/Users/me/other"),
        ];
        let map = map_roots(&roots, |p| {
            if p.starts_with("/Volumes/Projects") {
                mounted(2, "/Volumes/Projects")
            } else {
                mounted(1, "/System/Volumes/Data")
            }
        });
        assert_eq!(map.volumes.len(), 2);
        assert_eq!(map.volumes[0].name, "Startup disk");
        assert_eq!(
            map.volumes[0].roots,
            vec![
                PathBuf::from("/Users/me/working"),
                PathBuf::from("/Users/me/other")
            ]
        );
        assert_eq!(map.volumes[1].name, "Projects");
        assert!(map.unavailable.is_empty());
    }

    #[test]
    fn missing_root_is_not_mounted() {
        let roots = vec![
            PathBuf::from("/Volumes/Projects/code"),
            PathBuf::from("/opt/gone"),
        ];
        let map = map_roots(&roots, |_| Location::Missing);
        assert!(map.volumes.is_empty());
        assert_eq!(map.unavailable[0].name, "Projects");
        assert_eq!(map.unavailable[0].reason, "not mounted");
        assert_eq!(map.unavailable[1].name, "/opt/gone");
    }

    #[test]
    fn unreadable_root_is_reported() {
        let roots = vec![PathBuf::from("/secret")];
        let map = map_roots(&roots, |_| Location::Unreadable("denied".into()));
        assert_eq!(map.unavailable[0].reason, "unreadable, see log");
    }

    #[test]
    fn locates_real_paths() {
        let tmp = std::env::temp_dir();
        assert!(matches!(locate(&tmp), Location::Mounted { .. }));
        assert_eq!(locate(Path::new("/definitely/not/here")), Location::Missing);
    }

    /// Mounts a real 200 MB disk image to stand in for an external drive, then
    /// unmounts it. Ignored by default because it attaches a disk; run with
    /// `cargo test -- --ignored disk_image`.
    #[test]
    #[ignore]
    fn disk_image_is_its_own_volume_until_detached() {
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("picard-dmg-{}", std::process::id()));
        let image = dir.join("drive.dmg");
        let mount = dir.join("PicardTestDrive");
        std::fs::create_dir_all(&mount).unwrap();
        let ok = |cmd: &mut Command| {
            let out = cmd.output().expect("running hdiutil");
            assert!(
                out.status.success(),
                "{cmd:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        ok(Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "200m",
                "-fs",
                "APFS",
                "-volname",
                "PicardTestDrive",
            ])
            .arg(&image));
        ok(Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image));

        let root = mount.join("code");
        std::fs::create_dir_all(&root).unwrap();
        let home_root = std::env::temp_dir();
        let map = map_roots(&[home_root.clone(), root.clone()], locate);

        ok(Command::new("hdiutil").args(["detach"]).arg(&mount));
        let after = map_roots(&[home_root, root.clone()], locate);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(map.volumes.len(), 2, "{map:?}");
        let drive = &map.volumes[1];
        assert_eq!(drive.name, "PicardTestDrive");
        assert_eq!(drive.roots, vec![root]);
        assert_eq!(after.volumes.len(), 1, "{after:?}");
        assert_eq!(after.unavailable.len(), 1, "{after:?}");
        assert_eq!(after.unavailable[0].reason, "not mounted");
    }

    #[test]
    fn two_real_folders_on_one_volume_share_a_device() {
        let tmp = std::env::temp_dir();
        let a = tmp.join("picard-volume-test-a");
        let b = tmp.join("picard-volume-test-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let map = map_roots(&[a.clone(), b.clone()], locate);
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
        assert_eq!(map.volumes.len(), 1, "{map:?}");
        assert_eq!(map.volumes[0].roots, vec![a, b]);
    }
}
