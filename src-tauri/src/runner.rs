//! Runs the cleanup tasks on one volume.
//!
//! Tasks run in config order. Before each task the runner re-reads free space
//! and stops once the volume is back above `threshold + recover_margin`, so the
//! cheap, safe tasks listed first are often the only ones that run.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use crate::config::Config;
use crate::disk;
use crate::history::{self, Entry};
use crate::tasks::{self, Candidate, CleanupTask, Ctx, Removed, SizeCache};
use crate::volume::Volume;

/// Files Picard keeps next to its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    pub history: PathBuf,
    pub lock: PathBuf,
}

impl DataPaths {
    pub fn beside(config_path: &Path) -> Self {
        let dir = config_path.parent().unwrap_or(Path::new("."));
        Self {
            history: dir.join("history.jsonl"),
            lock: dir.join("run.lock"),
        }
    }
}

/// Exclusive lock held for the whole of a cleanup run. Uses `flock`, so it also
/// stops the CLI and the menu bar app from cleaning at the same time. The OS
/// releases it when the file is closed, including if Picard crashes.
pub struct RunLock {
    _file: File,
}

impl RunLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))?;
        // SAFETY: the fd is valid for the life of `file`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                bail!("another cleanup is already running");
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

/// One task's scan result, for preview.
#[derive(Debug)]
pub struct TaskScan {
    pub task: &'static str,
    pub result: Result<Vec<Candidate>, String>,
}

/// Lists what every task would remove. Changes nothing.
pub fn preview(tasks: &[Box<dyn CleanupTask>], ctx: &Ctx) -> Vec<TaskScan> {
    tasks
        .iter()
        .map(|t| TaskScan {
            task: t.id(),
            result: t.scan(ctx).map_err(|e| format!("{e:#}")),
        })
        .collect()
}

/// What a run on one volume did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct VolumeRun {
    pub removed: Vec<(&'static str, Removed)>,
    pub failed: Vec<(&'static str, PathBuf, String)>,
    /// Why the run ended before the last task, if it did.
    pub stopped: Option<String>,
}

impl VolumeRun {
    pub fn freed_bytes(&self) -> u64 {
        self.removed.iter().map(|(_, r)| r.bytes).sum()
    }
}

/// Runs tasks in order until free space reaches `target_free`.
///
/// `read_free` and `record` are passed in so the ordering and early-stop rules
/// can be tested without a real disk or history file. `record` is called after
/// each task with what it removed.
pub fn run_tasks(
    tasks: &[Box<dyn CleanupTask>],
    ctx: &Ctx,
    target_free: u64,
    read_free: &dyn Fn() -> io::Result<u64>,
    record: &mut dyn FnMut(&'static str, &[Removed]),
) -> VolumeRun {
    let mut out = VolumeRun::default();
    for task in tasks {
        match read_free() {
            Ok(free) if free >= target_free => {
                out.stopped = Some("enough space is free".to_string());
                break;
            }
            Ok(_) => {}
            Err(e) => {
                out.stopped = Some(format!("cannot read free space: {e}"));
                break;
            }
        }
        let candidates = match task.scan(ctx) {
            Ok(c) => c,
            Err(e) => {
                log::error!("{} scan failed: {e:#}", task.id());
                out.failed
                    .push((task.id(), PathBuf::new(), format!("scan failed: {e:#}")));
                continue;
            }
        };
        let report = task.run(ctx, &candidates);
        record(task.id(), &report.removed);
        out.removed
            .extend(report.removed.into_iter().map(|r| (task.id(), r)));
        out.failed.extend(
            report
                .failed
                .into_iter()
                .map(|(p, why)| (task.id(), p, why)),
        );
        if let Some(why) = report.halted {
            out.stopped = Some(why);
            break;
        }
    }
    out
}

/// Notification title and body for a cleanup on volume `name`.
///
/// `after` is the free space once the run finished, if it could be read.
pub fn outcome_message(
    name: &str,
    run: &Result<VolumeRun, String>,
    after: Option<disk::DiskSpace>,
) -> (String, String) {
    let now = after.map_or_else(String::new, |s| format!(" {}.", disk::format_free(s)));
    match run {
        Err(e) => (
            format!("{name} is low"),
            format!("Cleanup failed: {e}. See the log."),
        ),
        Ok(r) if r.removed.is_empty() => {
            (format!("{name} is low"), format!("Nothing to clean.{now}"))
        }
        Ok(r) => (
            format!("Picard cleaned {name}"),
            format!("Freed {}.{now}", disk::format_bytes(r.freed_bytes())),
        ),
    }
}

/// Cleans one volume for real: takes the lock, runs the tasks and writes history.
pub fn clean_volume(
    config: &Config,
    volume: &Volume,
    sizes: &SizeCache,
    paths: &DataPaths,
) -> Result<VolumeRun> {
    let _lock = RunLock::acquire(&paths.lock)?;
    let space = disk::query(&volume.mount)?;
    let target_free = config
        .threshold_for(&volume.mount)
        .bytes(space.total_bytes)
        .saturating_add(config.recover_margin_bytes);
    let exclude = tasks::build_exclude(&config.exclude)?;
    let ctx = Ctx::for_volume(volume, &exclude, SystemTime::now(), sizes)
        .with_search_roots(&config.roots);
    let tasks = tasks::from_config(&config.tasks);
    let read_free = || disk::query(&volume.mount).map(|s| s.free_bytes);
    let mut record = |task: &'static str, removed: &[Removed]| {
        let entries: Vec<Entry> = removed
            .iter()
            .map(|r| Entry {
                time: Utc::now(),
                task: task.to_string(),
                volume: volume.name.clone(),
                path: r.path.display().to_string(),
                bytes: r.bytes,
            })
            .collect();
        if let Err(e) = history::append(&paths.history, &entries) {
            log::error!("could not write history: {e:#}");
        }
    };
    Ok(run_tasks(
        &tasks,
        &ctx,
        target_free,
        &read_free,
        &mut record,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::stale_dirs;
    use crate::tasks::test_support::Fixture;
    use std::cell::Cell;

    fn two_tasks() -> Vec<Box<dyn CleanupTask>> {
        vec![
            Box::new(stale_dirs::node_modules(30)),
            Box::new(stale_dirs::cargo_clean(None, None)),
        ]
    }

    fn fixture() -> Fixture {
        let fx = Fixture::new(&[]);
        fx.file("web/package.json", 1);
        fx.file("web/node_modules/x.js", 1000);
        fx.file("rs/Cargo.toml", 1);
        fx.file("rs/target/debug/app", 1000);
        fx.age("web", 60);
        fx
    }

    #[test]
    fn runs_every_task_while_still_low_and_records_each() {
        let fx = fixture();
        let ctx = fx.ctx();
        let mut recorded = Vec::new();
        let run = run_tasks(&two_tasks(), &ctx, 100, &|| Ok(0), &mut |t, r| {
            recorded.push((t, r.len()))
        });
        assert_eq!(run.removed.len(), 2);
        assert_eq!(recorded, vec![("node_modules", 1), ("cargo_clean", 1)]);
        assert!(run.stopped.is_none());
        assert!(run.freed_bytes() >= 2000);
    }

    #[test]
    fn stops_once_free_space_reaches_target() {
        let fx = fixture();
        let ctx = fx.ctx();
        // First read is low; after the first task the volume reads as recovered.
        let reads = Cell::new(0);
        let read_free = || {
            reads.set(reads.get() + 1);
            Ok(if reads.get() == 1 { 0 } else { 100 })
        };
        let run = run_tasks(&two_tasks(), &ctx, 100, &read_free, &mut |_, _| {});
        assert_eq!(run.removed.len(), 1);
        assert_eq!(run.removed[0].0, "node_modules");
        assert_eq!(run.stopped.as_deref(), Some("enough space is free"));
        assert!(
            fx.root.join("rs/target").exists(),
            "cargo task must not run"
        );
    }

    #[test]
    fn stops_when_free_space_cannot_be_read() {
        let fx = fixture();
        let run = run_tasks(
            &two_tasks(),
            &fx.ctx(),
            100,
            &|| Err(io::Error::other("gone")),
            &mut |_, _| {},
        );
        assert!(run.removed.is_empty());
        assert!(run.stopped.unwrap().contains("gone"));
    }

    #[test]
    fn preview_changes_nothing() {
        let fx = fixture();
        let scans = preview(&two_tasks(), &fx.ctx());
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].result.as_ref().unwrap().len(), 1);
        assert!(fx.root.join("web/node_modules").exists());
        assert!(fx.root.join("rs/target").exists());
    }

    #[test]
    fn lock_blocks_a_second_holder_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let first = RunLock::acquire(&path).unwrap();
        let err = RunLock::acquire(&path)
            .err()
            .expect("second lock must fail");
        assert!(err.to_string().contains("already running"), "{err}");
        drop(first);
        assert!(RunLock::acquire(&path).is_ok());
    }

    #[test]
    fn outcome_messages() {
        let after = Some(disk::DiskSpace {
            free_bytes: 15_000_000_000,
            total_bytes: 994_000_000_000,
        });
        let run = VolumeRun {
            removed: vec![(
                "node_modules",
                Removed {
                    path: PathBuf::from("/x"),
                    bytes: 3_200_000_000,
                },
            )],
            ..VolumeRun::default()
        };
        assert_eq!(
            outcome_message("Startup disk", &Ok(run), after),
            (
                "Picard cleaned Startup disk".to_string(),
                "Freed 3.2 GB. 15 GB free (2%).".to_string()
            )
        );
        assert_eq!(
            outcome_message("D", &Ok(VolumeRun::default()), None).1,
            "Nothing to clean."
        );
        assert_eq!(
            outcome_message("D", &Err("another cleanup is already running".into()), None).1,
            "Cleanup failed: another cleanup is already running. See the log."
        );
    }

    #[test]
    fn data_paths_sit_beside_the_config() {
        let p = DataPaths::beside(Path::new("/x/Picard/config.toml"));
        assert_eq!(p.history, PathBuf::from("/x/Picard/history.jsonl"));
        assert_eq!(p.lock, PathBuf::from("/x/Picard/run.lock"));
    }
}
