//! Cleanup tasks: the shared contract, path safety checks and removal.
//!
//! Every task splits into `scan` (what would be removed, with sizes) and `run`
//! (remove it). Preview, the ask-mode estimate and the history log all reuse
//! `scan`. Every path is re-checked by `check_target` immediately before it is
//! removed, because the filesystem can change between scan and run.

mod build_output;
mod cache_prune;
mod command;
mod git;
mod git_worktrees;
mod in_use;
mod size;
pub mod stale_dirs;
mod tool_caches;

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

pub use size::SizeCache;

use crate::config::{CargoMode, TaskConfig, ToolOverrides};
use crate::env::ToolEnv;
use crate::volume::Volume;

/// Something a task proposes to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    /// The amount counted in totals. Zero when the task cannot know it ahead.
    pub bytes: u64,
    pub reason: String,
    pub estimate: Estimate,
}

/// How far `Candidate::bytes` can be trusted, which decides how its size reads.
///
/// Only `bytes` feeds totals. `UpTo` carries a ceiling for display alone,
/// because adding it to a total would overstate what a run frees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Estimate {
    /// `bytes` is the measured size of what will be removed.
    Exact,
    /// Another program decides what to remove; it frees at most this much.
    UpTo(u64),
    /// Nothing about the amount is known until the task runs.
    Unknown,
}

impl Candidate {
    /// The size as shown in preview and the CLI: "2.0 GB", "up to 2.6 GB" or
    /// "unknown".
    pub fn size_text(&self) -> String {
        match self.estimate {
            Estimate::Exact => crate::disk::format_bytes(self.bytes),
            Estimate::UpTo(ceiling) => format!("up to {}", crate::disk::format_bytes(ceiling)),
            Estimate::Unknown => "unknown".to_string(),
        }
    }
}

/// Something a task removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub path: PathBuf,
    pub bytes: u64,
}

/// The outcome of `run`. Partial success is normal: one locked folder must not
/// stop the rest, so failures are collected rather than returned as an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub removed: Vec<Removed>,
    pub failed: Vec<(PathBuf, String)>,
    /// Set when the task stopped early, for example because the drive vanished.
    pub halted: Option<String>,
}

/// Folders the user asked Picard to leave alone (`ignore` in the config).
///
/// A candidate is protected when it is an ignored folder, or when it holds
/// one: removing a worktree would delete an ignored `node_modules` inside it.
/// Protection never reaches down: ignoring a project folder does not protect
/// the `node_modules` inside it. Paths compare by their components, both as
/// given and as resolved, so `~/w/x` still matches after macOS reports it
/// through a symlinked parent such as `/var`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreList {
    /// Every entry, as given and resolved. A candidate equal to one is ignored.
    paths: HashSet<PathBuf>,
    /// The entries that exist now, as given and resolved. Only these make a
    /// candidate hold an ignored folder: removing a folder cannot remove an
    /// ignored folder that is not inside it any more.
    present: HashSet<PathBuf>,
}

/// Why `IgnoreList` protects a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protected {
    /// The path is in the list.
    Ignored,
    /// The path holds a folder in the list.
    HoldsIgnored,
}

impl Protected {
    pub fn reason(self) -> &'static str {
        match self {
            Protected::Ignored => "is ignored",
            Protected::HoldsIgnored => "holds an ignored folder",
        }
    }
}

impl IgnoreList {
    /// Builds the list from config entries, checking which exist now. An
    /// entry that cannot be resolved, for example because the folder is gone,
    /// still matches by its own path, so the folder stays ignored if it comes
    /// back.
    pub fn new(entries: &[PathBuf]) -> Self {
        let mut paths = HashSet::new();
        let mut present = HashSet::new();
        for entry in entries {
            paths.insert(entry.clone());
            if let Ok(canon) = entry.canonicalize() {
                paths.insert(canon.clone());
                present.insert(entry.clone());
                present.insert(canon);
            }
        }
        Self { paths, present }
    }

    /// Whether removing `path` would remove an ignored folder, and why.
    pub fn protects(&self, path: &Path) -> Option<Protected> {
        if self.paths.is_empty() {
            return None;
        }
        let canon = path.canonicalize().ok();
        let forms = std::iter::once(path).chain(canon.as_deref());
        let mut found = None;
        for form in forms {
            if self.paths.contains(form) {
                return Some(Protected::Ignored);
            }
            if self.present.iter().any(|entry| entry.starts_with(form)) {
                found = Some(Protected::HoldsIgnored);
            }
        }
        found
    }
}

/// Agent config that tools such as Automatic rewrite on every sync. A change
/// to these is not someone working on the project, so it never resets a
/// project's idle time. Matched against each file and folder name at any depth.
/// The default for the config's `activity_ignore`, which can replace it.
pub const NOT_ACTIVITY: &[&str] = &[
    ".agents",
    ".automatic",
    ".automatic.json",
    ".claude",
    ".codex",
    ".cursor",
    ".gemini",
    ".junie",
    ".mcp.json",
    ".opencode",
    ".windsurf",
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    "opencode.json",
];

/// Asked during a scan; true once the scan should give up, for example
/// because the user pressed Cancel. Cheap, so walks call it on every entry.
pub type Stop<'a> = &'a (dyn Fn() -> bool + Sync);

/// The `Stop` of a run, or of a scan nobody can cancel.
const NEVER_STOP: Stop<'static> = &|| false;

/// What a task may touch on one volume.
pub struct Ctx<'a> {
    /// Canonical roots on this volume. Roots that cannot be resolved are left out.
    /// Everything removed must sit inside one of these.
    pub roots: Vec<PathBuf>,
    /// Where to look for things that own removable folders, such as git repos
    /// whose worktrees live on this volume. Defaults to `roots`.
    pub search_roots: Vec<PathBuf>,
    /// `st_dev` of the volume. Targets on any other device are refused.
    pub dev: u64,
    pub exclude: &'a GlobSet,
    pub now: SystemTime,
    pub sizes: &'a SizeCache,
    /// File and folder names whose changes do not make a project look in use.
    /// `NOT_ACTIVITY` unless replaced by the config's `activity_ignore`.
    pub not_activity: Vec<String>,
    /// Ignored folders, checked again on the resolved path just before each
    /// removal. Not part of `check_target`, because scans must still list them.
    /// Behind a lock so a run can refresh it between tasks.
    keep: RwLock<IgnoreList>,
    /// See `Stop`. Scans check it; removals never do, so a run is never cut short.
    stop: Stop<'a>,
}

impl<'a> Ctx<'a> {
    pub fn for_volume(
        volume: &Volume,
        exclude: &'a GlobSet,
        now: SystemTime,
        sizes: &'a SizeCache,
    ) -> Self {
        let roots: Vec<PathBuf> = volume
            .roots
            .iter()
            .filter_map(|r| match r.canonicalize() {
                Ok(c) => Some(c),
                Err(e) => {
                    log::warn!("skipping root {}: {e}", r.display());
                    None
                }
            })
            .collect();
        Self {
            search_roots: roots.clone(),
            roots,
            dev: volume.dev,
            exclude,
            now,
            sizes,
            not_activity: NOT_ACTIVITY.iter().map(|n| n.to_string()).collect(),
            keep: RwLock::default(),
            stop: NEVER_STOP,
        }
    }

    /// Lets the scan give up early once `stop` returns true. A stopped scan
    /// returns what it found so far; the caller throws that away.
    pub fn with_stop(mut self, stop: Stop<'a>) -> Self {
        self.stop = stop;
        self
    }

    /// True once the scan should give up. See `with_stop`.
    pub fn stopped(&self) -> bool {
        (self.stop)()
    }

    /// Sizes of `dirs`, in order, from `sizes`. A size left unfinished
    /// because the scan stopped reads as zero.
    pub fn measure_all(&self, dirs: &[PathBuf]) -> Vec<u64> {
        self.sizes.measure_all(dirs, self.stop)
    }

    /// Uses the config's `activity_ignore` list in place of the defaults. The
    /// config has already put the defaults there when the key is missing.
    pub fn with_activity_ignore(mut self, names: &[String]) -> Self {
        self.not_activity = names.to_vec();
        self
    }

    /// Replaces the ignore list that removals check.
    pub fn set_keep(&self, list: IgnoreList) {
        *self.keep.write().unwrap_or_else(PoisonError::into_inner) = list;
    }

    /// Refuses a resolved path that is ignored or holds an ignored folder.
    /// Call it with the path `check_target` returned, just before removing.
    pub fn refuse_kept(&self, canon: &Path) -> Result<(), String> {
        let keep = self.keep.read().unwrap_or_else(PoisonError::into_inner);
        match keep.protects(canon) {
            Some(why) => Err(why.reason().to_string()),
            None => Ok(()),
        }
    }

    /// Searches all of `roots` (every configured root, on any volume) for owners
    /// of removable folders, while removal stays limited to this volume's roots.
    pub fn with_search_roots(mut self, roots: &[PathBuf]) -> Self {
        self.search_roots = roots.iter().filter_map(|r| r.canonicalize().ok()).collect();
        self
    }

    /// True while at least one root is still on the expected device.
    pub fn volume_present(&self) -> bool {
        self.roots.iter().any(|r| {
            std::fs::metadata(r)
                .map(|m| m.dev() == self.dev)
                .unwrap_or(false)
        })
    }
}

/// One kind of cleanup.
pub trait CleanupTask: Send + Sync {
    fn id(&self) -> &'static str;
    /// Lists what would be removed. Never changes anything.
    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>>;
    /// Removes the given candidates, re-checking each one first.
    fn run(&self, ctx: &Ctx, candidates: &[Candidate]) -> Report;
}

/// Builds the enabled tasks in config order. Tools are found through `env`,
/// with `overrides` from the `[tools]` table taking precedence. The config has
/// already passed `config::load`'s privacy check, so its commands and tool
/// paths come from the user.
pub fn from_config(
    tasks: &[TaskConfig],
    env: &ToolEnv,
    overrides: &ToolOverrides,
) -> Vec<Box<dyn CleanupTask>> {
    let mut out: Vec<Box<dyn CleanupTask>> = Vec::new();
    for task in tasks {
        match task {
            TaskConfig::NodeModules {
                enabled: true,
                stale_days,
                prune_package_caches,
            } => {
                let mut task = stale_dirs::node_modules(*stale_days);
                if *prune_package_caches {
                    let steps = [
                        env.resolve("npm", overrides)
                            .map(cache_prune::PruneStep::npm),
                        env.resolve("pnpm", overrides)
                            .map(cache_prune::PruneStep::pnpm),
                    ];
                    let prune = cache_prune::CachePrune::new(
                        steps.into_iter().flatten().collect(),
                        env.path_var(),
                        env.home().to_path_buf(),
                    );
                    if prune.is_empty() {
                        log::warn!("prune_package_caches is on but neither npm nor pnpm was found");
                    } else {
                        task = task.with_cache_prune(prune);
                    }
                }
                out.push(Box::new(task));
            }
            TaskConfig::CargoClean {
                enabled: true,
                mode,
                stale_days,
                depth,
            } => {
                let stale_days = match mode {
                    CargoMode::Stale => Some(*stale_days),
                    CargoMode::All => None,
                };
                out.push(Box::new(stale_dirs::cargo_clean(stale_days, *depth)));
            }
            TaskConfig::PythonVenv {
                enabled: true,
                stale_days,
            } => out.push(Box::new(stale_dirs::python_venv(*stale_days))),
            TaskConfig::ToolCaches {
                enabled: true,
                uv,
                homebrew,
                simulators,
                bun,
            } => {
                let steps = tool_cache_steps(env, overrides, *uv, *homebrew, *simulators, *bun);
                if steps.is_empty() {
                    log::warn!("tool_caches skipped: none of its tools were found or turned on");
                } else {
                    out.push(Box::new(tool_caches::ToolCaches::new(
                        cache_prune::CachePrune::new(
                            steps,
                            env.path_var(),
                            env.home().to_path_buf(),
                        ),
                    )));
                }
            }
            TaskConfig::BuildOutput {
                enabled: true,
                stale_days,
                dirs,
            } => match env.resolve("git", overrides) {
                Some(git) => out.push(Box::new(build_output::BuildOutput::new(
                    git::GitCmd::new(git, env.path_var()),
                    dirs.clone(),
                    *stale_days,
                ))),
                None => log::error!(
                    "build_output skipped: git not found on {} (set [tools] git)",
                    env.path_var()
                ),
            },
            TaskConfig::GitWorktrees {
                enabled: true,
                stale_days,
                min_idle_minutes,
            } => match env.resolve("git", overrides) {
                Some(git) => out.push(Box::new(git_worktrees::GitWorktrees::new(
                    git::GitCmd::new(git, env.path_var()),
                    *stale_days,
                    Duration::from_secs(min_idle_minutes.saturating_mul(60)),
                    Arc::new(in_use::InUse::load),
                ))),
                None => log::error!(
                    "git_worktrees skipped: git not found on {} (set [tools] git)",
                    env.path_var()
                ),
            },
            TaskConfig::Command {
                enabled: true,
                name,
                run,
                timeout_secs,
            } => out.push(Box::new(command::CommandTask::new(
                name.clone(),
                run.clone(),
                Duration::from_secs(*timeout_secs),
                command::Shell::login_zsh(),
                env.path_var(),
                env.home().to_path_buf(),
            ))),
            _ => {}
        }
    }
    out
}

/// Prune steps for the tools that are turned on and installed. A missing tool
/// is logged and left out, so one absent tool does not stop the others.
fn tool_cache_steps(
    env: &ToolEnv,
    overrides: &ToolOverrides,
    uv: bool,
    homebrew: bool,
    simulators: bool,
    bun: bool,
) -> Vec<cache_prune::PruneStep> {
    use cache_prune::PruneStep;
    /// Builds a step from the tool's path and the home folder.
    type MakeStep = fn(PathBuf, &Path) -> PruneStep;
    let wanted: [(bool, &str, MakeStep); 4] = [
        (uv, "uv", |p, _| PruneStep::uv(p)),
        (homebrew, "brew", |p, _| PruneStep::homebrew(p)),
        (simulators, "xcrun", PruneStep::simulators),
        (bun, "bun", |p, _| PruneStep::bun(p)),
    ];
    let mut steps = Vec::new();
    for (on, tool, step) in wanted {
        if !on {
            continue;
        }
        match env.resolve(tool, overrides) {
            Some(program) => steps.push(step(program, env.home())),
            None => log::info!(
                "tool_caches: {tool} not found on {}; skipping it",
                env.path_var()
            ),
        }
    }
    steps
}

/// Compiles the `exclude` globs. Config validation already checked each one.
pub fn build_exclude(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).with_context(|| format!("exclude pattern {p:?}"))?);
    }
    builder.build().context("building exclude globs")
}

/// Decides whether `path` may be removed. Returns its canonical path if so.
///
/// Refuses symlinks, non-directories, anything outside the roots (after
/// resolving symlinks in parent folders), a root itself, excluded paths, and
/// anything on another device.
pub fn check_target(path: &Path, ctx: &Ctx) -> Result<PathBuf, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot read: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("is a symlink".to_string());
    }
    if !meta.is_dir() {
        return Err("is not a directory".to_string());
    }
    if meta.dev() != ctx.dev {
        return Err("is on a different volume".to_string());
    }
    let canon = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve: {e}"))?;
    if !ctx
        .roots
        .iter()
        .any(|root| canon.starts_with(root) && canon != *root)
    {
        return Err("is outside the configured roots".to_string());
    }
    if ctx.exclude.is_match(&canon) {
        return Err("matches an exclude pattern".to_string());
    }
    Ok(canon)
}

/// How many times `remove_dirs` tries a folder that comes back "not empty".
const REMOVE_ATTEMPTS: u32 = 3;
const REMOVE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Removes `dir` with `remove`, trying again when it fails with ENOTEMPTY.
///
/// On macOS another process can write into a folder while it is deleted. Finder
/// and Finder extensions drop `.DS_Store` files into folders whose contents
/// change, so the final `rmdir` sees a new file. A later pass removes it.
/// Other errors return at once.
fn remove_tree(
    dir: &Path,
    attempts: u32,
    delay: Duration,
    remove: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match remove(dir) {
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty && attempt < attempts => {
                log::warn!(
                    "removing {} (attempt {attempt} of {attempts}): {e}; retrying",
                    dir.display()
                );
                attempt += 1;
                std::thread::sleep(delay);
            }
            result => return result,
        }
    }
}

/// Removes candidate directories one at a time, stopping if the volume goes away.
pub fn remove_dirs(ctx: &Ctx, candidates: &[Candidate]) -> Report {
    let mut report = Report::default();
    for c in candidates {
        if !ctx.volume_present() {
            report.halted = Some("the volume is no longer mounted".to_string());
            break;
        }
        let canon = match check_target(&c.path, ctx).and_then(|p| ctx.refuse_kept(&p).map(|()| p)) {
            Ok(p) => p,
            Err(why) => {
                log::warn!("not removing {}: {why}", c.path.display());
                report.failed.push((c.path.clone(), why));
                continue;
            }
        };
        // std's remove_dir_all does not follow symlinks inside the tree.
        match remove_tree(&canon, REMOVE_ATTEMPTS, REMOVE_RETRY_DELAY, |p| {
            std::fs::remove_dir_all(p)
        }) {
            Ok(()) => {
                log::info!("removed {} ({} bytes)", canon.display(), c.bytes);
                report.removed.push(Removed {
                    path: canon,
                    bytes: c.bytes,
                });
            }
            Err(e) => {
                log::error!("removing {} failed: {e}", canon.display());
                report.failed.push((canon, e.to_string()));
                if !ctx.volume_present() {
                    report.halted = Some(format!("the volume went away: {e}"));
                    break;
                }
            }
        }
    }
    report
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::fs;

    /// A temporary root plus everything needed to build a `Ctx` over it.
    pub struct Fixture {
        pub dir: tempfile::TempDir,
        pub root: PathBuf,
        pub exclude: GlobSet,
        pub sizes: SizeCache,
    }

    impl Fixture {
        pub fn new(exclude: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            fs::create_dir_all(&root).unwrap();
            let patterns: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
            Self {
                root,
                dir,
                exclude: build_exclude(&patterns).unwrap(),
                sizes: SizeCache::default(),
            }
        }

        pub fn ctx(&self) -> Ctx<'_> {
            let volume = Volume {
                dev: fs::metadata(&self.root).unwrap().dev(),
                mount: PathBuf::from("/"),
                name: "test".into(),
                roots: vec![self.root.clone()],
                uuid: None,
            };
            Ctx::for_volume(&volume, &self.exclude, SystemTime::now(), &self.sizes)
        }

        /// Writes a file, creating parent folders.
        pub fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, vec![b'x'; bytes]).unwrap();
            p
        }

        /// Sets the mtime of everything under `rel` (and `rel` itself) to `days` ago.
        pub fn age(&self, rel: &str, days: u64) {
            let t = filetime::FileTime::from_system_time(
                SystemTime::now() - std::time::Duration::from_secs(days * 86_400),
            );
            for e in walkdir::WalkDir::new(self.root.join(rel)).contents_first(true) {
                let e = e.unwrap();
                filetime::set_symlink_file_times(e.path(), t, t).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Fixture;
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn allows_a_plain_folder_inside_the_root() {
        let fx = Fixture::new(&[]);
        fx.file("proj/node_modules/a.js", 10);
        let ctx = fx.ctx();
        let target = fx.root.join("proj/node_modules");
        assert_eq!(
            check_target(&target, &ctx).unwrap(),
            target.canonicalize().unwrap()
        );
    }

    #[test]
    fn refuses_the_root_itself() {
        let fx = Fixture::new(&[]);
        assert!(
            check_target(&fx.root, &fx.ctx())
                .unwrap_err()
                .contains("outside")
        );
    }

    #[test]
    fn refuses_symlinks() {
        let fx = Fixture::new(&[]);
        fx.file("real/a.js", 1);
        symlink(fx.root.join("real"), fx.root.join("link")).unwrap();
        assert_eq!(
            check_target(&fx.root.join("link"), &fx.ctx()).unwrap_err(),
            "is a symlink"
        );
    }

    #[test]
    fn refuses_escape_through_a_symlinked_parent() {
        let fx = Fixture::new(&[]);
        let outside = fx.dir.path().join("outside/node_modules");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(fx.dir.path().join("outside"), fx.root.join("sneaky")).unwrap();
        let err = check_target(&fx.root.join("sneaky/node_modules"), &fx.ctx()).unwrap_err();
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn ignore_list_protects_exact_folders_and_their_parents_through_symlinks() {
        let fx = Fixture::new(&[]);
        fx.file("w/node_modules/x", 1);
        let real = fx.root.join("w/node_modules");
        let link = fx.dir.path().join("link");
        std::os::unix::fs::symlink(&fx.root, &link).unwrap();
        let ignore = IgnoreList::new(&[link.join("w/node_modules")]);
        assert_eq!(ignore.protects(&real), Some(Protected::Ignored));
        assert_eq!(
            ignore.protects(&real.canonicalize().unwrap()),
            Some(Protected::Ignored)
        );
        assert_eq!(
            ignore.protects(&fx.root.join("w")),
            Some(Protected::HoldsIgnored)
        );
        assert_eq!(ignore.protects(&real.join("x")), None, "never reaches down");
        assert_eq!(
            ignore.protects(&fx.root.join("w2")),
            None,
            "components, not text"
        );
        let gone = IgnoreList::new(&[PathBuf::from("/nowhere/at/all")]);
        assert_eq!(
            gone.protects(Path::new("/nowhere/at/all")),
            Some(Protected::Ignored)
        );
        assert_eq!(
            gone.protects(Path::new("/nowhere/at")),
            None,
            "a missing entry does not protect the folder above it"
        );
        assert_eq!(IgnoreList::default().protects(&real), None);
    }

    #[test]
    fn removal_refuses_kept_paths_even_when_a_task_proposes_them() {
        let fx = Fixture::new(&[]);
        fx.file("w/node_modules/x", 1);
        let target = fx.root.join("w/node_modules");
        let ctx = fx.ctx();
        ctx.set_keep(IgnoreList::new(std::slice::from_ref(&target)));
        let cand = Candidate {
            path: fx.root.join("w"),
            bytes: 1,
            reason: String::new(),
            estimate: Estimate::Exact,
        };
        let report = remove_dirs(&ctx, &[cand]);
        assert!(report.removed.is_empty());
        assert_eq!(report.failed[0].1, "holds an ignored folder");
        assert!(target.exists());
    }

    #[test]
    fn refuses_excluded_paths() {
        let fx = Fixture::new(&["**/keep-*/**"]);
        fx.file("keep-me/node_modules/a.js", 1);
        let err = check_target(&fx.root.join("keep-me/node_modules"), &fx.ctx()).unwrap_err();
        assert!(err.contains("exclude"), "{err}");
    }

    #[test]
    fn refuses_files() {
        let fx = Fixture::new(&[]);
        let f = fx.file("a.txt", 1);
        assert_eq!(
            check_target(&f, &fx.ctx()).unwrap_err(),
            "is not a directory"
        );
    }

    #[test]
    fn remove_dirs_removes_valid_and_reports_refused() {
        let fx = Fixture::new(&[]);
        fx.file("p/node_modules/a.js", 1);
        fx.file("real/a.js", 1);
        symlink(fx.root.join("real"), fx.root.join("link")).unwrap();
        let ctx = fx.ctx();
        let cands = vec![
            Candidate {
                path: fx.root.join("p/node_modules"),
                bytes: 5,
                reason: String::new(),
                estimate: Estimate::Exact,
            },
            Candidate {
                path: fx.root.join("link"),
                bytes: 5,
                reason: String::new(),
                estimate: Estimate::Exact,
            },
        ];
        let report = remove_dirs(&ctx, &cands);
        assert_eq!(report.removed.len(), 1);
        assert!(!fx.root.join("p/node_modules").exists());
        assert_eq!(report.failed.len(), 1);
        assert!(
            fx.root.join("real/a.js").exists(),
            "symlink target must survive"
        );
        assert!(report.halted.is_none());
    }

    #[test]
    fn remove_dirs_halts_when_volume_is_gone() {
        let fx = Fixture::new(&[]);
        fx.file("p/node_modules/a.js", 1);
        let mut ctx = fx.ctx();
        ctx.dev = ctx.dev.wrapping_add(1);
        let report = remove_dirs(
            &ctx,
            &[Candidate {
                path: fx.root.join("p/node_modules"),
                bytes: 1,
                reason: String::new(),
                estimate: Estimate::Exact,
            }],
        );
        assert!(report.halted.is_some());
        assert!(fx.root.join("p/node_modules").exists());
    }

    #[test]
    fn remove_tree_retries_directory_not_empty() {
        let calls = std::cell::Cell::new(0);
        let result = remove_tree(Path::new("/x"), 3, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn remove_tree_gives_up_after_the_last_attempt() {
        let calls = std::cell::Cell::new(0);
        let result = remove_tree(Path::new("/x"), 3, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn remove_tree_does_not_retry_other_errors() {
        let calls = std::cell::Cell::new(0);
        let result = remove_tree(Path::new("/x"), 3, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn from_config_skips_disabled_tasks() {
        let tasks = vec![
            TaskConfig::GitWorktrees {
                enabled: false,
                stale_days: 14,
                min_idle_minutes: 30,
            },
            TaskConfig::Command {
                enabled: true,
                name: "x".into(),
                run: "true".into(),
                timeout_secs: 1,
            },
            TaskConfig::NodeModules {
                enabled: false,
                stale_days: 30,
                prune_package_caches: false,
            },
            TaskConfig::CargoClean {
                enabled: true,
                mode: CargoMode::All,
                stale_days: 14,
                depth: None,
            },
        ];
        let env = ToolEnv::from_dirs(Path::new("/"), vec![PathBuf::from("/usr/bin")]);
        let ids: Vec<_> = from_config(&tasks, &env, &ToolOverrides::default())
            .iter()
            .map(|t| t.id())
            .collect();
        assert_eq!(ids, vec!["command", "cargo_clean"]);
    }

    #[test]
    fn tool_caches_keeps_only_tools_that_are_on_and_installed() {
        use std::os::unix::fs::PermissionsExt;
        let bin = tempfile::tempdir().unwrap();
        for tool in ["uv", "brew"] {
            let p = bin.path().join(tool);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = ToolEnv::from_dirs(Path::new("/Users/me"), vec![bin.path().to_path_buf()]);
        let o = ToolOverrides::default();
        let labels = |steps: Vec<cache_prune::PruneStep>| -> Vec<String> {
            steps.into_iter().map(|s| s.label).collect()
        };
        // bun and xcrun are not on this PATH.
        assert_eq!(
            labels(tool_cache_steps(&env, &o, true, true, true, true)),
            vec!["uv cache", "Homebrew cache"]
        );
        assert_eq!(
            labels(tool_cache_steps(&env, &o, false, true, true, true)),
            vec!["Homebrew cache"]
        );
        let none = vec![TaskConfig::ToolCaches {
            enabled: true,
            uv: false,
            homebrew: false,
            simulators: true,
            bun: true,
        }];
        assert!(from_config(&none, &env, &o).is_empty());
    }
}
