//! Cleanup tasks: the shared contract, path safety checks and removal.
//!
//! Every task splits into `scan` (what would be removed, with sizes) and `run`
//! (remove it). Preview, the ask-mode estimate and the history log all reuse
//! `scan`. Every path is re-checked by `check_target` immediately before it is
//! removed, because the filesystem can change between scan and run.

mod cache_prune;
mod command;
mod git;
mod git_worktrees;
mod in_use;
mod size;
pub mod stale_dirs;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    pub bytes: u64,
    pub reason: String,
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
                    let prune = cache_prune::CachePrune::new(
                        env.resolve("npm", overrides),
                        env.resolve("pnpm", overrides),
                        env.path_var(),
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
            // TODO(M5): command. Listed in config but not run yet.
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

/// Removes candidate directories one at a time, stopping if the volume goes away.
pub fn remove_dirs(ctx: &Ctx, candidates: &[Candidate]) -> Report {
    let mut report = Report::default();
    for c in candidates {
        if !ctx.volume_present() {
            report.halted = Some("the volume is no longer mounted".to_string());
            break;
        }
        let canon = match check_target(&c.path, ctx) {
            Ok(p) => p,
            Err(why) => {
                log::warn!("not removing {}: {why}", c.path.display());
                report.failed.push((c.path.clone(), why));
                continue;
            }
        };
        // std's remove_dir_all does not follow symlinks inside the tree.
        match std::fs::remove_dir_all(&canon) {
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
            },
            Candidate {
                path: fx.root.join("link"),
                bytes: 5,
                reason: String::new(),
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
            }],
        );
        assert!(report.halted.is_some());
        assert!(fx.root.join("p/node_modules").exists());
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
}
