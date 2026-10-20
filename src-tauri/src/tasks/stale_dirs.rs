//! Removes regenerable build folders from projects nobody has touched lately.
//!
//! `node_modules`, Cargo `target/` and Python virtualenvs work the same way:
//! find a folder named one of `dir_names` next to one of the `markers` files,
//! decide whether the project is stale, and delete the folder. Each can be
//! rebuilt with `npm install`, `cargo build` or `pip install`, so they are
//! deleted outright. Moving them to the Trash would free nothing.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use walkdir::WalkDir;

use super::cache_prune::CachePrune;
use super::{Candidate, CleanupTask, Ctx, Estimate, Report, check_target, remove_dirs};

/// Folders never descended into while searching for projects or reading mtimes.
/// They are build output or too large to walk usefully. `.next` is Next.js
/// output: it holds a `package.json` and `node_modules` of its own, which would
/// otherwise show up as near-empty candidates, and a rebuild there is not
/// activity in the project.
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git", ".next"];

/// How deep to search under each root unless a task says otherwise.
const DEFAULT_SEARCH_DEPTH: usize = 8;

/// How deep to look inside a project for its newest file.
const MTIME_DEPTH: usize = 6;

const SECS_PER_DAY: u64 = 86_400;

/// Written by Finder when a folder is opened. Not a sign of work.
const FINDER_METADATA: &str = ".DS_Store";

pub struct StaleDirs {
    id: &'static str,
    /// The project must hold at least one of these files.
    markers: &'static [&'static str],
    dir_names: &'static [&'static str],
    /// A file the folder itself must hold, proving it is what its name says.
    inner_marker: Option<&'static str>,
    /// `None` means every match is a candidate (cargo_clean `mode = "all"`).
    stale_days: Option<u32>,
    search_depth: usize,
    /// Runs after the folders are removed (node_modules only).
    cache_prune: Option<CachePrune>,
}

impl StaleDirs {
    pub fn with_cache_prune(mut self, prune: CachePrune) -> Self {
        self.cache_prune = Some(prune);
        self
    }
}

/// Stale `node_modules` next to a `package.json`.
pub fn node_modules(stale_days: u32) -> StaleDirs {
    StaleDirs {
        id: "node_modules",
        markers: &["package.json"],
        dir_names: &["node_modules"],
        inner_marker: None,
        stale_days: Some(stale_days),
        search_depth: DEFAULT_SEARCH_DEPTH,
        cache_prune: None,
    }
}

/// Cargo `target/` next to a `Cargo.toml`. `stale_days = None` cleans them all.
pub fn cargo_clean(stale_days: Option<u32>, depth: Option<u32>) -> StaleDirs {
    StaleDirs {
        id: "cargo_clean",
        markers: &["Cargo.toml"],
        dir_names: &["target"],
        inner_marker: None,
        stale_days,
        search_depth: depth.map_or(DEFAULT_SEARCH_DEPTH, |d| d as usize),
        cache_prune: None,
    }
}

/// Python virtualenvs (`.venv`, `venv`) in a project with a Python manifest.
///
/// The folder must hold `pyvenv.cfg`, which `python -m venv`, virtualenv, uv
/// and Poetry all write, so a source folder that happens to be called `venv`
/// is never a candidate.
pub fn python_venv(stale_days: u32) -> StaleDirs {
    StaleDirs {
        id: "python_venv",
        markers: &[
            "pyproject.toml",
            "requirements.txt",
            "setup.py",
            "setup.cfg",
            "Pipfile",
        ],
        dir_names: &[".venv", "venv"],
        inner_marker: Some("pyvenv.cfg"),
        stale_days: Some(stale_days),
        search_depth: DEFAULT_SEARCH_DEPTH,
        cache_prune: None,
    }
}

impl CleanupTask for StaleDirs {
    fn id(&self) -> &'static str {
        self.id
    }

    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>> {
        let mut found = Vec::new();
        for (project, target) in find_pairs(ctx, self.markers, self.dir_names, self.search_depth) {
            if let Err(why) = check_target(&target, ctx) {
                log::info!("{}: skipping {}: {why}", self.id, target.display());
                continue;
            }
            if let Some(inner) = self.inner_marker
                && !is_file(&target.join(inner))
            {
                log::debug!("{}: skipping {}: no {inner}", self.id, target.display());
                continue;
            }
            let reason = match self.stale_days {
                None => "cleans all (mode = \"all\")".to_string(),
                Some(days) => match idle_days(&project, ctx.now, self.dir_names) {
                    Some(idle) if idle >= u64::from(days) => format!("untouched for {idle} days"),
                    _ => continue,
                },
            };
            found.push((target, reason));
        }
        let paths: Vec<PathBuf> = found.iter().map(|(p, _)| p.clone()).collect();
        let sizes = ctx.sizes.measure_all(&paths);
        Ok(found
            .into_iter()
            .zip(sizes)
            .map(|((path, reason), bytes)| Candidate {
                path,
                bytes,
                reason,
                estimate: Estimate::Exact,
            })
            .collect())
    }

    fn run(&self, ctx: &Ctx, candidates: &[Candidate]) -> Report {
        let mut report = remove_dirs(ctx, candidates);
        if let Some(prune) = &self.cache_prune
            && report.halted.is_none()
        {
            prune.run(ctx, &mut report);
        }
        report
    }
}

/// Finds `(project, project/<one of dir_names>)` pairs under every root.
///
/// A folder is a project when it holds one of `markers`. An empty `markers`
/// makes every folder a project. Never follows symlinks, never descends into
/// `SKIP_DIRS` or `dir_names`, and prunes excluded folders, so a
/// `node_modules` nested inside another is not found.
pub(super) fn find_pairs(
    ctx: &Ctx,
    markers: &[&str],
    dir_names: &[&str],
    depth: usize,
) -> Vec<(PathBuf, PathBuf)> {
    let mut pairs = Vec::new();
    for root in &ctx.roots {
        let walker = WalkDir::new(root)
            .follow_links(false)
            .max_depth(depth)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0
                    || (e.file_type().is_dir()
                        && !is_skipped(e.path(), dir_names)
                        && !ctx.exclude.is_match(e.path()))
            });
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    log::debug!("walk error under {}: {e}", root.display());
                    continue;
                }
            };
            let project = entry.path();
            let is_project =
                markers.is_empty() || markers.iter().any(|m| is_file(&project.join(m)));
            if !is_project {
                continue;
            }
            for name in dir_names {
                let target = project.join(name);
                if is_real_dir(&target) {
                    pairs.push((project.to_path_buf(), target));
                }
            }
        }
    }
    pairs
}

/// True for `SKIP_DIRS` and for any folder named in `extra`.
fn is_skipped(path: &Path, extra: &[&str]) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SKIP_DIRS.contains(&n) || extra.contains(&n))
}

pub(super) fn is_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Whole days since anything in the project last changed, ignoring build
/// output, `.git` and the folders named in `also_skip` (the task's own
/// targets, so rebuilding them is not activity). `None` if nothing could be
/// read or the newest mtime is in the future (a clock or exFAT oddity), which
/// counts as "not stale".
pub(super) fn idle_days(project: &Path, now: SystemTime, also_skip: &[&str]) -> Option<u64> {
    let newest = newest_mtime_skipping(project, also_skip)?;
    let idle = now.duration_since(newest).ok()?;
    Some(idle.as_secs() / SECS_PER_DAY)
}

/// Newest mtime of any file in the project, ignoring build output and `.git`.
///
/// Folder mtimes are not counted. Finder writes a `.DS_Store` into every
/// folder you open, which bumps that folder's mtime, so merely looking at an
/// old project would make it look active. The `.DS_Store` files themselves
/// are skipped for the same reason. A deleted file therefore no longer counts
/// as activity; editing or adding one still does.
pub(super) fn newest_mtime(project: &Path) -> Option<SystemTime> {
    newest_mtime_skipping(project, &[])
}

/// `newest_mtime`, also skipping folders named in `also_skip`.
fn newest_mtime_skipping(project: &Path, also_skip: &[&str]) -> Option<SystemTime> {
    WalkDir::new(project)
        .follow_links(false)
        .max_depth(MTIME_DEPTH)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_skipped(e.path(), also_skip))
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_type().is_dir() && e.file_name() != FINDER_METADATA)
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::*;
    use std::os::unix::fs::symlink;

    fn names(c: &[Candidate], root: &Path) -> Vec<String> {
        let root = root.canonicalize().unwrap();
        let mut v: Vec<String> = c
            .iter()
            .map(|c| {
                c.path
                    .canonicalize()
                    .unwrap()
                    .strip_prefix(&root)
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        v.sort();
        v
    }

    /// A node project with a `node_modules` holding `bytes` bytes.
    fn node_project(fx: &Fixture, rel: &str, bytes: usize, age_days: u64) {
        fx.file(&format!("{rel}/package.json"), 10);
        fx.file(&format!("{rel}/src/index.js"), 10);
        fx.file(&format!("{rel}/node_modules/dep/index.js"), bytes);
        fx.age(rel, age_days);
    }

    #[test]
    fn finds_stale_node_modules_with_size_and_reason() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "old", 50_000, 40);
        node_project(&fx, "fresh", 50_000, 2);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["old/node_modules"]);
        assert!(found[0].bytes >= 50_000, "{}", found[0].bytes);
        assert_eq!(found[0].reason, "untouched for 40 days");
    }

    #[test]
    fn a_recent_source_edit_keeps_the_project_fresh() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "p", 10, 90);
        fx.file("p/src/new.js", 1);
        assert!(node_modules(30).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn recent_activity_inside_node_modules_does_not_count() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "p", 10, 90);
        // Touching a file deep inside node_modules leaves the project idle.
        fx.file("p/node_modules/dep/extra.js", 1);
        fx.age("p/node_modules/dep", 0);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/node_modules"]);
    }

    #[test]
    fn browsing_in_finder_does_not_make_a_project_fresh() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "p", 10, 40);
        // Opening folders in Finder writes .DS_Store files, which also bumps
        // the mtime of each folder they land in.
        fx.file("p/.DS_Store", 1);
        fx.file("p/src/.DS_Store", 1);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/node_modules"]);
        assert_eq!(found[0].reason, "untouched for 40 days");
    }

    #[test]
    fn nested_node_modules_are_not_scanned() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "p", 10, 90);
        fx.file("p/node_modules/dep/package.json", 1);
        fx.file("p/node_modules/dep/node_modules/x/i.js", 1);
        fx.age("p", 90);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/node_modules"]);
    }

    #[test]
    fn monorepo_packages_are_each_candidates() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "mono", 10, 60);
        node_project(&fx, "mono/packages/a", 10, 60);
        fx.age("mono", 60);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(
            names(&found, &fx.root),
            vec!["mono/node_modules", "mono/packages/a/node_modules"]
        );
    }

    #[test]
    fn symlinked_node_modules_is_not_a_candidate() {
        let fx = Fixture::new(&[]);
        fx.file("shared/dep/i.js", 1);
        fx.file("p/package.json", 1);
        symlink(fx.root.join("shared"), fx.root.join("p/node_modules")).unwrap();
        fx.age("p", 90);
        assert!(node_modules(30).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn excluded_projects_are_skipped() {
        let fx = Fixture::new(&["**/keep-*/**", "**/keep-*"]);
        node_project(&fx, "keep-me", 10, 90);
        node_project(&fx, "other", 10, 90);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["other/node_modules"]);
    }

    #[test]
    fn next_build_output_is_not_a_project() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "site", 10, 60);
        fx.file("site/.next/package.json", 1);
        fx.file("site/.next/node_modules/x.js", 1);
        fx.age("site", 60);
        // A fresh build inside .next does not make the site look active.
        fx.file("site/.next/server/page.js", 1);
        let found = node_modules(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["site/node_modules"]);
    }

    #[test]
    fn no_package_json_means_no_candidate() {
        let fx = Fixture::new(&[]);
        fx.file("loose/node_modules/x.js", 1);
        fx.age("loose", 90);
        assert!(node_modules(30).scan(&fx.ctx()).unwrap().is_empty());
    }

    fn cargo_project(fx: &Fixture, rel: &str, age_days: u64) {
        fx.file(&format!("{rel}/Cargo.toml"), 10);
        fx.file(&format!("{rel}/src/main.rs"), 10);
        fx.file(&format!("{rel}/target/debug/app"), 20_000);
        fx.age(rel, age_days);
    }

    #[test]
    fn cargo_stale_mode_only_takes_idle_projects() {
        let fx = Fixture::new(&[]);
        cargo_project(&fx, "old", 20);
        cargo_project(&fx, "today", 0);
        let found = cargo_clean(Some(14), None).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["old/target"]);
    }

    #[test]
    fn cargo_all_mode_takes_everything() {
        let fx = Fixture::new(&[]);
        cargo_project(&fx, "old", 20);
        cargo_project(&fx, "today", 0);
        let found = cargo_clean(None, None).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["old/target", "today/target"]);
    }

    #[test]
    fn cargo_workspace_member_edit_keeps_workspace_target() {
        let fx = Fixture::new(&[]);
        cargo_project(&fx, "ws", 30);
        fx.file("ws/crates/a/Cargo.toml", 1);
        fx.age("ws", 30);
        fx.file("ws/crates/a/src/lib.rs", 1);
        assert!(
            cargo_clean(Some(14), None)
                .scan(&fx.ctx())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cargo_depth_limits_the_search() {
        let fx = Fixture::new(&[]);
        cargo_project(&fx, "a/b/c/deep", 30);
        assert!(
            cargo_clean(None, Some(2))
                .scan(&fx.ctx())
                .unwrap()
                .is_empty()
        );
        assert_eq!(cargo_clean(None, Some(5)).scan(&fx.ctx()).unwrap().len(), 1);
    }

    /// A Python project with a virtualenv in `rel/venv_name`.
    fn python_project(fx: &Fixture, rel: &str, venv_name: &str, age_days: u64) {
        fx.file(&format!("{rel}/pyproject.toml"), 10);
        fx.file(&format!("{rel}/app/main.py"), 10);
        fx.file(&format!("{rel}/{venv_name}/pyvenv.cfg"), 10);
        fx.file(&format!("{rel}/{venv_name}/lib/site.py"), 30_000);
        fx.age(rel, age_days);
    }

    #[test]
    fn finds_stale_venvs_under_either_name() {
        let fx = Fixture::new(&[]);
        python_project(&fx, "old", ".venv", 40);
        python_project(&fx, "older", "venv", 90);
        python_project(&fx, "fresh", ".venv", 2);
        let found = python_venv(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["old/.venv", "older/venv"]);
        assert!(found.iter().all(|c| c.bytes >= 30_000));
    }

    #[test]
    fn any_python_manifest_marks_a_project() {
        let fx = Fixture::new(&[]);
        fx.file("reqs/requirements.txt", 1);
        fx.file("reqs/.venv/pyvenv.cfg", 1);
        fx.file("pipenv/Pipfile", 1);
        fx.file("pipenv/.venv/pyvenv.cfg", 1);
        fx.file("none/.venv/pyvenv.cfg", 1);
        fx.age("", 90);
        let found = python_venv(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["pipenv/.venv", "reqs/.venv"]);
    }

    #[test]
    fn a_venv_folder_without_pyvenv_cfg_is_kept() {
        let fx = Fixture::new(&[]);
        fx.file("p/pyproject.toml", 1);
        // Source code that happens to live in a folder called venv.
        fx.file("p/venv/notes.py", 1);
        fx.age("p", 90);
        assert!(python_venv(30).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn installing_into_the_venv_does_not_make_the_project_fresh() {
        let fx = Fixture::new(&[]);
        python_project(&fx, "p", ".venv", 60);
        fx.file("p/.venv/lib/new_dep.py", 1);
        let found = python_venv(30).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/.venv"]);
        assert_eq!(found[0].reason, "untouched for 60 days");
    }

    #[test]
    fn run_removes_only_the_scanned_folders() {
        let fx = Fixture::new(&[]);
        node_project(&fx, "old", 10, 40);
        let ctx = fx.ctx();
        let task = node_modules(30);
        let found = task.scan(&ctx).unwrap();
        let report = task.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1);
        assert!(!fx.root.join("old/node_modules").exists());
        assert!(fx.root.join("old/package.json").exists());
        assert!(fx.root.join("old/src/index.js").exists());
    }
}
