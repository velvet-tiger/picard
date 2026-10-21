//! Runs the cleanup tasks on one volume.
//!
//! Tasks run in config order. Before each task the runner re-reads free space
//! and stops once the volume is back above `threshold + recover_margin`, so the
//! cheap, safe tasks listed first are often the only ones that run. The daily
//! scan in auto mode is the exception: it runs every task in full.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::config::Config;
use crate::disk;
use crate::env::ToolEnv;
use crate::history::{self, Entry};
use crate::tasks::{self, Candidate, CleanupTask, Ctx, IgnoreList, Protected, Removed, SizeCache};
use crate::volume::Volume;

/// Files Picard keeps next to its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPaths {
    pub history: PathBuf,
    pub lock: PathBuf,
    /// The local date of the last daily scan, as `YYYY-MM-DD`.
    pub daily_scan: PathBuf,
    /// Exists once the user has accepted the first-run warning (see welcome.rs).
    pub warning_accepted: PathBuf,
}

impl DataPaths {
    pub fn beside(config_path: &Path) -> Self {
        let dir = config_path.parent().unwrap_or(Path::new("."));
        Self {
            history: dir.join("history.jsonl"),
            lock: dir.join("run.lock"),
            daily_scan: dir.join("daily-scan.txt"),
            warning_accepted: dir.join("warning-accepted.txt"),
        }
    }
}

/// The run lock is held by another cleanup. A separate type, so callers can
/// retry later instead of treating it as a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockBusy;

impl std::fmt::Display for LockBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another cleanup is already running")
    }
}

impl std::error::Error for LockBusy {}

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
                return Err(LockBusy.into());
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
    /// Every candidate, ignored ones included, so preview can show them.
    pub result: Result<Vec<Candidate>, String>,
    /// The candidate paths in `result` that the user ignored. No run removes them.
    pub ignored: HashSet<PathBuf>,
    /// The candidate paths in `result` that hold an ignored folder, such as a
    /// worktree with an ignored `node_modules`. No run removes them either.
    pub holds_ignored: HashSet<PathBuf>,
}

impl TaskScan {
    /// Whether no run will remove this candidate.
    pub fn is_protected(&self, path: &Path) -> bool {
        self.ignored.contains(path) || self.holds_ignored.contains(path)
    }

    /// Bytes a run could free: every candidate that is not protected.
    pub fn cleanable_bytes(&self) -> u64 {
        self.result.as_ref().map_or(0, |cands| {
            cands
                .iter()
                .filter(|c| !self.is_protected(&c.path))
                .map(|c| c.bytes)
                .sum()
        })
    }
}

/// Lists what every task would remove, marking the candidates `ignore`
/// protects. Changes nothing.
pub fn preview(tasks: &[Box<dyn CleanupTask>], ctx: &Ctx, ignore: &IgnoreList) -> Vec<TaskScan> {
    tasks
        .iter()
        .map(|t| {
            let result = t.scan(ctx).map_err(|e| format!("{e:#}"));
            let mut ignored = HashSet::new();
            let mut holds_ignored = HashSet::new();
            for c in result.iter().flatten() {
                match ignore.protects(&c.path) {
                    Some(Protected::Ignored) => ignored.insert(c.path.clone()),
                    Some(Protected::HoldsIgnored) => holds_ignored.insert(c.path.clone()),
                    None => false,
                };
            }
            TaskScan {
                task: t.id(),
                result,
                ignored,
                holds_ignored,
            }
        })
        .collect()
}

/// What a run on one volume did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct VolumeRun {
    pub removed: Vec<(&'static str, Removed)>,
    pub failed: Vec<(&'static str, PathBuf, String)>,
    /// Candidates left alone because the ignore list protects them, with why.
    pub skipped: Vec<(&'static str, PathBuf, &'static str)>,
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
/// Candidates the ignore list protects are never removed, whatever the scope.
/// `ignore` is called before each task, so a folder ignored while a run is
/// under way is honoured from the next task on. It is also set on `ctx`, so
/// each removal checks it again on the resolved path. `read_free` and
/// `record` are passed in so the ordering and early-stop rules can be tested
/// without a real disk or history file. `record` is called after each task
/// with what it removed.
pub fn run_tasks(
    tasks: &[Box<dyn CleanupTask>],
    ctx: &Ctx,
    scope: &Scope,
    ignore: &dyn Fn() -> IgnoreList,
    read_free: &dyn Fn() -> io::Result<u64>,
    record: &mut dyn FnMut(&'static str, &[Removed]),
) -> VolumeRun {
    let mut out = VolumeRun::default();
    for task in tasks {
        if let Scope::UntilFree(target_free) = scope {
            match read_free() {
                Ok(free) if free >= *target_free => {
                    out.stopped = Some("enough space is free".to_string());
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    out.stopped = Some(format!("cannot read free space: {e}"));
                    break;
                }
            }
        }
        let scanned = match task.scan(ctx) {
            Ok(c) => c,
            Err(e) => {
                log::error!("{} scan failed: {e:#}", task.id());
                out.failed
                    .push((task.id(), PathBuf::new(), format!("scan failed: {e:#}")));
                continue;
            }
        };
        let ignore = ignore();
        let mut candidates = scope.filter(scanned);
        candidates.retain(|c| match ignore.protects(&c.path) {
            Some(why) => {
                log::info!(
                    "{}: skipping {}: {}",
                    task.id(),
                    c.path.display(),
                    why.reason()
                );
                out.skipped.push((task.id(), c.path.clone(), why.reason()));
                false
            }
            None => true,
        });
        ctx.set_keep(ignore);
        if candidates.is_empty() {
            continue;
        }
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

/// Notification for the daily scan's cleanup on volume `name`, or `None`
/// when it removed nothing and nothing failed. A daily scan runs on healthy
/// disks too, so "nothing to clean" is not news worth a notification, but a
/// run where every removal failed is.
pub fn daily_outcome_message(
    name: &str,
    run: &Result<VolumeRun, String>,
    after: Option<disk::DiskSpace>,
) -> Option<(String, String)> {
    match run {
        Err(e) => Some((
            format!("Daily scan of {name} failed"),
            format!("Cleanup failed: {e}. See the log."),
        )),
        Ok(r) if r.removed.is_empty() && r.failed.is_empty() => None,
        Ok(r) if r.removed.is_empty() => {
            let items = match r.failed.len() {
                1 => "1 item".to_string(),
                n => format!("{n} items"),
            };
            Some((
                format!("Daily scan of {name} had problems"),
                format!("{items} could not be cleaned. See the log."),
            ))
        }
        Ok(_) => Some(outcome_message(name, run, after)),
    }
}

/// What `clean_volume` should remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanRequest {
    /// Clean until the volume is back above its threshold plus margin.
    UntilTarget,
    /// Run every task in full, however much space is free. For the daily scan.
    Everything,
    /// Only these paths. See `Scope::Selected`.
    Selected(HashSet<PathBuf>),
}

/// How much of a volume a run may clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every candidate, stopping between tasks once free space reaches this many bytes.
    UntilFree(u64),
    /// Every candidate of every task, with no early stop.
    All,
    /// Only these paths, and only if a fresh scan still proposes them. Used for
    /// the Preview window: a stale or tampered selection can never remove
    /// anything Picard would not have proposed itself. No early stop, because
    /// the user picked exactly what to remove.
    Selected(HashSet<PathBuf>),
}

impl Scope {
    fn filter(&self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        match self {
            Scope::UntilFree(_) | Scope::All => candidates,
            Scope::Selected(chosen) => candidates
                .into_iter()
                .filter(|c| chosen.contains(&c.path))
                .collect(),
        }
    }
}

/// The free-space target for a volume: its threshold plus the recovery margin.
pub fn target_free(config: &Config, volume: &Volume, total_bytes: u64) -> u64 {
    config
        .threshold_for(&volume.mount, volume.uuid.as_deref())
        .bytes(total_bytes)
        .saturating_add(config.recover_margin_bytes)
}

/// Lists what every enabled task would remove on one volume. Changes nothing.
pub fn preview_volume(
    config: &Config,
    volume: &Volume,
    sizes: &SizeCache,
    env: &ToolEnv,
) -> Result<Vec<TaskScan>> {
    let exclude = tasks::build_exclude(&config.exclude)?;
    let ctx = Ctx::for_volume(volume, &exclude, SystemTime::now(), sizes)
        .with_search_roots(&config.roots)
        .with_activity_ignore(&config.activity_ignore);
    let tasks = tasks::from_config(&config.tasks, env, &config.tools);
    Ok(preview(&tasks, &ctx, &IgnoreList::new(&config.ignore)))
}

/// Re-reads the `ignore` list from the config file during a run.
pub type ReloadIgnore<'a> = &'a (dyn Fn() -> Result<Vec<PathBuf>> + 'a);

/// A `ReloadIgnore` that reads the config file at `config_path`.
pub fn ignore_from_file<'a>(
    config_path: &'a Path,
    home: &'a Path,
) -> impl Fn() -> Result<Vec<PathBuf>> + 'a {
    move || crate::config::load(config_path, home).map(|c| c.ignore)
}

/// The ignore list for the next task: `base`, from the config the run
/// started with, plus whatever `reload` finds now. Taking the union means a
/// folder ignored mid-run is honoured, and a failed or half-written re-read
/// can never drop an entry. An entry removed mid-run applies from the next run.
fn current_ignore(base: &[PathBuf], reload: ReloadIgnore) -> IgnoreList {
    let mut entries = base.to_vec();
    match reload() {
        Ok(more) => entries.extend(more),
        Err(e) => log::warn!(
            "could not re-read the ignore list; using the one from the start of the run: {e:#}"
        ),
    }
    IgnoreList::new(&entries)
}

/// Cleans one volume for real: takes the lock, runs the tasks and writes history.
/// `request` says how much to remove; see `CleanRequest`. `reload_ignore`
/// re-reads the ignore list before each task; see `current_ignore`.
pub fn clean_volume(
    config: &Config,
    volume: &Volume,
    sizes: &SizeCache,
    paths: &DataPaths,
    env: &ToolEnv,
    request: CleanRequest,
    reload_ignore: ReloadIgnore,
) -> Result<VolumeRun> {
    let _lock = RunLock::acquire(&paths.lock)?;
    let scope = match request {
        CleanRequest::Selected(chosen) => Scope::Selected(chosen),
        CleanRequest::Everything => Scope::All,
        CleanRequest::UntilTarget => Scope::UntilFree(target_free(
            config,
            volume,
            disk::query(&volume.mount)?.total_bytes,
        )),
    };
    let exclude = tasks::build_exclude(&config.exclude)?;
    let ctx = Ctx::for_volume(volume, &exclude, SystemTime::now(), sizes)
        .with_search_roots(&config.roots)
        .with_activity_ignore(&config.activity_ignore);
    let tasks = tasks::from_config(&config.tasks, env, &config.tools);
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
    let ignore = || current_ignore(&config.ignore, reload_ignore);
    Ok(run_tasks(
        &tasks,
        &ctx,
        &scope,
        &ignore,
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
        let run = run_tasks(
            &two_tasks(),
            &ctx,
            &Scope::UntilFree(100),
            &IgnoreList::default,
            &|| Ok(0),
            &mut |t, r| recorded.push((t, r.len())),
        );
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
        let run = run_tasks(
            &two_tasks(),
            &ctx,
            &Scope::UntilFree(100),
            &IgnoreList::default,
            &read_free,
            &mut |_, _| {},
        );
        assert_eq!(run.removed.len(), 1);
        assert_eq!(run.removed[0].0, "node_modules");
        assert_eq!(run.stopped.as_deref(), Some("enough space is free"));
        assert!(
            fx.root.join("rs/target").exists(),
            "cargo task must not run"
        );
    }

    #[test]
    fn scope_all_runs_every_task_even_when_space_is_free() {
        let fx = fixture();
        let run = run_tasks(
            &two_tasks(),
            &fx.ctx(),
            &Scope::All,
            &IgnoreList::default,
            &|| Ok(u64::MAX),
            &mut |_, _| {},
        );
        assert_eq!(run.removed.len(), 2, "{run:?}");
        assert!(run.stopped.is_none());
        assert!(!fx.root.join("rs/target").exists());
    }

    #[test]
    fn stops_when_free_space_cannot_be_read() {
        let fx = fixture();
        let run = run_tasks(
            &two_tasks(),
            &fx.ctx(),
            &Scope::UntilFree(100),
            &IgnoreList::default,
            &|| Err(io::Error::other("gone")),
            &mut |_, _| {},
        );
        assert!(run.removed.is_empty());
        assert!(run.stopped.unwrap().contains("gone"));
    }

    #[test]
    fn selection_removes_only_chosen_paths_that_a_fresh_scan_proposes() {
        let fx = fixture();
        let ctx = fx.ctx();
        let web = fx.root.join("web/node_modules").canonicalize().unwrap();
        // A real folder the scan would never propose, as a tampered request might send.
        fx.file("rs/src/main.rs", 1);
        let outsider = fx.root.join("rs/src").canonicalize().unwrap();
        let chosen: HashSet<PathBuf> = [web.clone(), outsider.clone()].into_iter().collect();
        // Free space reads high, which would stop an UntilFree run at once.
        let run = run_tasks(
            &two_tasks(),
            &ctx,
            &Scope::Selected(chosen),
            &IgnoreList::default,
            &|| Ok(u64::MAX),
            &mut |_, _| {},
        );
        assert_eq!(run.removed.len(), 1, "{run:?}");
        assert_eq!(run.removed[0].1.path, web);
        assert!(
            fx.root.join("rs/target").exists(),
            "unselected target must survive"
        );
        assert!(outsider.exists(), "unproposed path must survive");
    }

    #[test]
    fn preview_changes_nothing() {
        let fx = fixture();
        let scans = preview(&two_tasks(), &fx.ctx(), &IgnoreList::default());
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].result.as_ref().unwrap().len(), 1);
        assert!(fx.root.join("web/node_modules").exists());
        assert!(fx.root.join("rs/target").exists());
    }

    #[test]
    fn ignored_paths_survive_every_scope() {
        let selected = || {
            let fx = fixture();
            let all: HashSet<PathBuf> = ["web/node_modules", "rs/target"]
                .iter()
                .map(|p| fx.root.join(p).canonicalize().unwrap())
                .collect();
            (fx, Scope::Selected(all))
        };
        let scopes: Vec<Box<dyn Fn() -> (Fixture, Scope)>> = vec![
            Box::new(|| (fixture(), Scope::UntilFree(100))),
            Box::new(|| (fixture(), Scope::All)),
            Box::new(selected),
        ];
        for make in scopes {
            let (fx, scope) = make();
            // Written through the un-resolved path, as a `~/…` entry would be.
            let ignore = IgnoreList::new(&[fx.root.join("web/node_modules")]);
            let run = run_tasks(
                &two_tasks(),
                &fx.ctx(),
                &scope,
                &|| ignore.clone(),
                &|| Ok(0),
                &mut |_, _| {},
            );
            assert!(
                fx.root.join("web/node_modules").exists(),
                "{scope:?}: ignored folder was removed"
            );
            assert_eq!(run.removed.len(), 1, "{scope:?}: {run:?}");
            assert_eq!(run.removed[0].0, "cargo_clean");
            assert!(run.failed.is_empty(), "{scope:?}: {run:?}");
            assert_eq!(run.skipped.len(), 1, "{scope:?}: {run:?}");
            assert_eq!(run.skipped[0].2, "is ignored");
        }
    }

    #[test]
    fn ignoring_a_folder_does_not_protect_folders_inside_it() {
        let fx = fixture();
        let ignore = IgnoreList::new(&[fx.root.join("web")]);
        let run = run_tasks(
            &two_tasks(),
            &fx.ctx(),
            &Scope::All,
            &|| ignore.clone(),
            &|| Ok(0),
            &mut |_, _| {},
        );
        assert_eq!(run.removed.len(), 2, "{run:?}");
    }

    #[test]
    fn a_folder_holding_an_ignored_folder_is_not_removed() {
        // A target folder whose inner build folder is ignored stands in for a
        // worktree holding an ignored node_modules.
        let fx = fixture();
        fx.file("rs/target/debug/keep/a", 10);
        let ignore = IgnoreList::new(&[fx.root.join("rs/target/debug/keep")]);
        let run = run_tasks(
            &two_tasks(),
            &fx.ctx(),
            &Scope::All,
            &|| ignore.clone(),
            &|| Ok(0),
            &mut |_, _| {},
        );
        assert!(fx.root.join("rs/target/debug/keep/a").exists(), "{run:?}");
        assert_eq!(run.skipped[0].2, "holds an ignored folder");
        assert_eq!(run.removed.len(), 1, "{run:?}");
        assert_eq!(run.removed[0].0, "node_modules");
    }

    #[test]
    fn ignore_list_is_read_again_before_each_task() {
        let fx = fixture();
        let calls = Cell::new(0);
        // Empty for the first task; the cargo target is ignored by the second.
        let target = fx.root.join("rs/target");
        let ignore = || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                IgnoreList::default()
            } else {
                IgnoreList::new(std::slice::from_ref(&target))
            }
        };
        let run = run_tasks(
            &two_tasks(),
            &fx.ctx(),
            &Scope::All,
            &ignore,
            &|| Ok(0),
            &mut |_, _| {},
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(run.removed.len(), 1, "{run:?}");
        assert!(target.exists());
    }

    #[test]
    fn current_ignore_keeps_the_starting_list_when_a_reload_fails_or_drops_entries() {
        let a = PathBuf::from("/x/a");
        let b = PathBuf::from("/x/b");
        let failed = current_ignore(std::slice::from_ref(&a), &|| Err(anyhow::anyhow!("bad")));
        assert_eq!(failed.protects(&a), Some(Protected::Ignored));
        let grown = current_ignore(std::slice::from_ref(&a), &|| Ok(vec![b.clone()]));
        assert_eq!(grown.protects(&a), Some(Protected::Ignored));
        assert_eq!(grown.protects(&b), Some(Protected::Ignored));
    }

    #[test]
    fn preview_marks_protected_candidates_and_leaves_them_out_of_cleanable_bytes() {
        let fx = fixture();
        fx.file("rs/target/debug/keep/a", 10);
        let ignore = IgnoreList::new(&[
            fx.root.join("web/node_modules"),
            fx.root.join("rs/target/debug/keep"),
        ]);
        let scans = preview(&two_tasks(), &fx.ctx(), &ignore);
        let (node, cargo) = (&scans[0], &scans[1]);
        assert_eq!(node.result.as_ref().unwrap().len(), 1, "still listed");
        assert_eq!(node.ignored.len(), 1);
        assert!(node.holds_ignored.is_empty());
        assert_eq!(node.cleanable_bytes(), 0);
        assert!(cargo.ignored.is_empty());
        assert_eq!(cargo.holds_ignored.len(), 1);
        assert_eq!(cargo.cleanable_bytes(), 0);
    }

    const LOCK_TEST: &str = "runner::tests::lock_blocks_a_second_holder_until_released";
    const LOCK_TEST_CHILD: &str = "PICARD_LOCK_TEST_CHILD";

    // On macOS every process spawn briefly copies the parent's file table,
    // close-on-exec descriptors included, until the child execs. A flock lock
    // belongs to the open file, so it outlives `drop` while a spawn from another
    // test thread is in flight, and the re-acquire fails. The check therefore
    // runs alone in a fresh copy of this test binary, where nothing else spawns.
    #[test]
    fn lock_blocks_a_second_holder_until_released() {
        if std::env::var_os(LOCK_TEST_CHILD).is_some() {
            check_lock_blocks_until_released();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([LOCK_TEST, "--exact", "--test-threads=1", "--nocapture"])
            .env(LOCK_TEST_CHILD, "1")
            .output()
            .expect("running the lock test in a child process");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
        // A wrong test name would run nothing and still exit 0.
        assert!(stdout.contains("1 passed"), "child ran no test:\n{stdout}");
    }

    fn check_lock_blocks_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let first = RunLock::acquire(&path).unwrap();
        let err = RunLock::acquire(&path)
            .err()
            .expect("second lock must fail");
        assert!(err.to_string().contains("already running"), "{err}");
        assert!(err.downcast_ref::<LockBusy>().is_some(), "{err:?}");
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
    fn daily_outcome_is_quiet_when_nothing_was_removed() {
        assert_eq!(
            daily_outcome_message("D", &Ok(VolumeRun::default()), None),
            None
        );
        let failed = daily_outcome_message("D", &Err("locked".into()), None).unwrap();
        assert_eq!(failed.0, "Daily scan of D failed");
        let all_failed = VolumeRun {
            failed: vec![
                ("node_modules", PathBuf::from("/a"), "denied".into()),
                ("node_modules", PathBuf::from("/b"), "denied".into()),
            ],
            ..VolumeRun::default()
        };
        assert_eq!(
            daily_outcome_message("D", &Ok(all_failed), None),
            Some((
                "Daily scan of D had problems".to_string(),
                "2 items could not be cleaned. See the log.".to_string()
            ))
        );
        let run = VolumeRun {
            removed: vec![(
                "node_modules",
                Removed {
                    path: PathBuf::from("/x"),
                    bytes: 1_000_000_000,
                },
            )],
            ..VolumeRun::default()
        };
        let done = daily_outcome_message("D", &Ok(run), None).unwrap();
        assert_eq!(
            done,
            ("Picard cleaned D".to_string(), "Freed 1.0 GB.".to_string())
        );
    }

    #[test]
    fn data_paths_sit_beside_the_config() {
        let p = DataPaths::beside(Path::new("/x/Picard/config.toml"));
        assert_eq!(p.history, PathBuf::from("/x/Picard/history.jsonl"));
        assert_eq!(p.lock, PathBuf::from("/x/Picard/run.lock"));
        assert_eq!(p.daily_scan, PathBuf::from("/x/Picard/daily-scan.txt"));
    }
}
