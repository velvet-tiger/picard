//! Removes build output folders, such as `dist`, `build`, `.next` and
//! `vendor`, from projects nobody has touched lately.
//!
//! Those names are not always build output. Some repos commit `dist`, Go
//! projects often commit `vendor`, and `build` is sometimes source. So a
//! folder is a candidate only when git vouches for it: git must ignore the
//! folder and track nothing inside it. A folder outside any git repo is kept,
//! because nothing can vouch for it. A folder that holds a git repo of its own
//! (Composer's `--prefer-source` installs do) is kept too, because that repo
//! may hold work that exists nowhere else.

use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use super::git::GitCmd;
use super::stale_dirs::{find_pairs, idle_days};
use super::{Candidate, CleanupTask, Ctx, Report, check_target, remove_dirs};

/// How deep to search under each root for projects.
const SEARCH_DEPTH: usize = 8;

pub struct BuildOutput {
    git: GitCmd,
    dirs: Vec<String>,
    stale_days: u32,
}

impl BuildOutput {
    /// `dirs` are plain folder names. Config validation has checked them.
    pub fn new(git: GitCmd, dirs: Vec<String>, stale_days: u32) -> Self {
        Self {
            git,
            dirs,
            stale_days,
        }
    }

    /// Why `project/name` must be kept, if anything. Only reads.
    ///
    /// `check-ignore` and `ls-files` read the index and ignore files. Neither
    /// runs filters or hooks, and `GitCmd` turns off fsmonitor.
    fn keep_reason(&self, project: &Path, name: &str) -> Option<String> {
        match self.git.check(project, &["check-ignore", "-q", "--", name]) {
            Ok(true) => {}
            Ok(false) => return Some("git does not ignore it".to_string()),
            Err(e) => return Some(format!("cannot ask git about it: {e:#}")),
        }
        match self.git.output(project, &["ls-files", "-z", "--", name]) {
            Ok(tracked) if tracked.is_empty() => {}
            Ok(_) => return Some("git tracks files inside it".to_string()),
            Err(e) => return Some(format!("cannot list its tracked files: {e:#}")),
        }
        nested_repo(&project.join(name))
            .map(|repo| format!("holds a git repo at {}", repo.display()))
    }
}

/// The first `.git` file or folder inside `dir`, if any. Never follows symlinks.
fn nested_repo(dir: &Path) -> Option<PathBuf> {
    WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name() == ".git")
        .map(|e| e.into_path())
}

impl CleanupTask for BuildOutput {
    fn id(&self) -> &'static str {
        "build_output"
    }

    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>> {
        let names: Vec<&str> = self.dirs.iter().map(String::as_str).collect();
        let mut found = Vec::new();
        for (project, target) in find_pairs(ctx, &[], &names, SEARCH_DEPTH) {
            if let Err(why) = check_target(&target, ctx) {
                log::info!("build_output: skipping {}: {why}", target.display());
                continue;
            }
            // Staleness first: it is cheap, and it spares running git for
            // every project that is still in use.
            let idle = match idle_days(&project, ctx.now, &names) {
                Some(idle) if idle >= u64::from(self.stale_days) => idle,
                _ => continue,
            };
            let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(why) = self.keep_reason(&project, name) {
                log::info!("build_output: keeping {}: {why}", target.display());
                continue;
            }
            found.push((target, format!("ignored by git, untouched for {idle} days")));
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
            })
            .collect())
    }

    fn run(&self, ctx: &Ctx, candidates: &[Candidate]) -> Report {
        remove_dirs(ctx, candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn task() -> BuildOutput {
        BuildOutput::new(
            GitCmd::default(),
            ["dist", "build", ".next", "vendor"]
                .map(String::from)
                .to_vec(),
            30,
        )
    }

    fn names(c: &[Candidate], root: &Path) -> Vec<String> {
        let root = root.canonicalize().unwrap();
        let mut v: Vec<String> = c
            .iter()
            .map(|c| c.path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        v.sort();
        v
    }

    /// A git repo at `rel` that ignores `dist/` and `build/`, with a commit.
    fn repo(fx: &Fixture, rel: &str) -> PathBuf {
        fx.file(&format!("{rel}/src/index.js"), 10);
        let dir = fx.root.join(rel);
        std::fs::write(dir.join(".gitignore"), "dist/\nbuild/\n").unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn takes_ignored_output_from_stale_projects_only() {
        let fx = Fixture::new(&[]);
        repo(&fx, "old");
        fx.file("old/dist/app.js", 20_000);
        fx.file("old/build/out.o", 1_000);
        fx.age("old", 60);
        repo(&fx, "fresh");
        fx.file("fresh/dist/app.js", 1);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["old/build", "old/dist"]);
        let dist = found.iter().find(|c| c.path.ends_with("dist")).unwrap();
        assert!(dist.bytes >= 20_000, "{}", dist.bytes);
        assert_eq!(dist.reason, "ignored by git, untouched for 60 days");
    }

    #[test]
    fn a_folder_git_does_not_ignore_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        // Negate the pattern so a global excludes file cannot change the answer.
        std::fs::write(dir.join(".gitignore"), "!dist/\n").unwrap();
        fx.file("p/dist/app.js", 1);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_folder_with_tracked_files_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        fx.file("p/dist/committed.js", 1);
        git(&dir, &["add", "-f", "dist/committed.js"]);
        git(&dir, &["commit", "-q", "-m", "ship dist"]);
        fx.file("p/dist/extra.js", 1);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_folder_outside_any_repo_is_kept() {
        let fx = Fixture::new(&[]);
        fx.file("loose/src/a.js", 1);
        fx.file("loose/dist/a.js", 1);
        fx.age("loose", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_folder_holding_a_git_repo_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        fx.file("p/composer.json", 1);
        fx.file("p/vendor/acme/lib/src/A.php", 1);
        git(&dir.join("vendor/acme/lib"), &["init", "-q"]);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn rebuilding_output_does_not_make_a_project_fresh() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/old.js", 1);
        fx.age("p", 60);
        fx.file("p/dist/new.js", 1);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/dist"]);
    }

    #[test]
    fn nested_output_folders_are_not_searched() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/build/inner.js", 1);
        fx.age("p", 60);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/dist"]);
    }

    #[test]
    fn run_removes_the_folder_and_keeps_sources() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/a.js", 1);
        fx.age("p", 60);
        let ctx = fx.ctx();
        let t = task();
        let found = t.scan(&ctx).unwrap();
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1);
        assert!(!fx.root.join("p/dist").exists());
        assert!(fx.root.join("p/src/index.js").exists());
    }
}
