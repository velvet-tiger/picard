//! Removes linked git worktrees whose work is safely elsewhere.
//!
//! A worktree is removed only when every one of these holds:
//! - it is a linked worktree (not the main one), not locked, not prunable
//! - it sits inside a root on this volume and is not excluded (`check_target`)
//! - no running process is working inside it (`lsof`)
//! - `git status --porcelain` is empty, so nothing uncommitted or untracked
//! - its commits exist elsewhere: HEAD is in the default branch, or its branch
//!   has an upstream with nothing unpushed
//! - and either HEAD is in the default branch and the folder has been idle for
//!   `min_idle`, or the folder has been idle for `stale_days`
//!
//! Removal is `git worktree remove` without `--force`, so git refuses anything
//! dirty or locked as a second guard. Branches are never deleted.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::git::{GitCmd, WorktreeEntry, parse_worktree_list};
use super::in_use::InUse;
use super::stale_dirs::newest_mtime;
use super::{Candidate, CleanupTask, Ctx, Estimate, Removed, Report, check_target};

/// Folders never descended into while looking for repos.
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git"];
const SEARCH_DEPTH: usize = 8;
const SECS_PER_DAY: u64 = 86_400;

/// Supplies the in-use list. `lsof` in the app, a fixed list in tests.
pub type InUseSource = Arc<dyn Fn() -> Result<InUse> + Send + Sync>;

pub struct GitWorktrees {
    git: GitCmd,
    stale_days: u32,
    min_idle: Duration,
    in_use: InUseSource,
}

impl GitWorktrees {
    pub fn new(git: GitCmd, stale_days: u32, min_idle: Duration, in_use: InUseSource) -> Self {
        Self {
            git,
            stale_days,
            min_idle,
            in_use,
        }
    }

    /// Decides whether one linked worktree is a candidate. Returns the reason if so.
    fn assess(
        &self,
        repo: &Path,
        default_branch: &str,
        wt: &WorktreeEntry,
        ctx: &Ctx,
        in_use: &InUse,
    ) -> Result<Option<String>> {
        if wt.locked || wt.prunable || wt.bare {
            return Ok(None);
        }
        if let Err(why) = check_target(&wt.path, ctx) {
            log::debug!("worktree {}: {why}", wt.path.display());
            return Ok(None);
        }
        if in_use.contains(&wt.path) {
            log::debug!(
                "worktree {}: in use by a running process",
                wt.path.display()
            );
            return Ok(None);
        }
        if let Some(why) = self.git.unsafe_to_inspect(&wt.path)? {
            log::warn!("worktree {}: skipped: {why}", wt.path.display());
            return Ok(None);
        }
        if !self
            .git
            .output(
                &wt.path,
                &["status", "--porcelain", "--ignore-submodules=all"],
            )?
            .trim()
            .is_empty()
        {
            log::debug!("worktree {}: has uncommitted changes", wt.path.display());
            return Ok(None);
        }
        let merged = self.git.check(
            repo,
            &["merge-base", "--is-ancestor", &wt.head, default_branch],
        )?;
        let pushed = match &wt.branch {
            Some(branch) if !merged => self.fully_pushed(repo, branch)?,
            _ => false,
        };
        if !merged && !pushed {
            log::debug!(
                "worktree {}: has commits that are not in {default_branch} or pushed",
                wt.path.display()
            );
            return Ok(None);
        }
        let Some(newest) = newest_mtime(&wt.path) else {
            return Ok(None);
        };
        let Ok(idle) = ctx.now.duration_since(newest) else {
            return Ok(None);
        };
        let stale = Duration::from_secs(u64::from(self.stale_days) * SECS_PER_DAY);
        if merged && idle >= self.min_idle {
            return Ok(Some(format!(
                "merged into {default_branch}, idle {}",
                format_idle(idle)
            )));
        }
        if idle >= stale {
            return Ok(Some(format!("idle {}", format_idle(idle))));
        }
        Ok(None)
    }

    /// True if `branch` has an upstream and nothing on it is unpushed.
    fn fully_pushed(&self, repo: &Path, branch: &str) -> Result<bool> {
        let upstream = self.git.output(
            repo,
            &[
                "for-each-ref",
                "--format=%(upstream)",
                &format!("refs/heads/{branch}"),
            ],
        )?;
        let upstream = upstream.trim();
        if upstream.is_empty()
            || !self
                .git
                .check(repo, &["rev-parse", "--verify", "--quiet", upstream])?
        {
            return Ok(false);
        }
        let ahead = self.git.output(
            repo,
            &[
                "rev-list",
                "--count",
                &format!("{upstream}..refs/heads/{branch}"),
            ],
        )?;
        Ok(ahead.trim() == "0")
    }

    /// The local default branch: the one `origin/HEAD` names, else `main`, else `master`.
    fn default_branch(&self, repo: &Path) -> Option<String> {
        let from_origin = self
            .git
            .output(
                repo,
                &[
                    "symbolic-ref",
                    "--quiet",
                    "--short",
                    "refs/remotes/origin/HEAD",
                ],
            )
            .ok()
            .and_then(|s| s.trim().split_once('/').map(|(_, b)| b.to_string()));
        from_origin
            .into_iter()
            .chain(["main".to_string(), "master".to_string()])
            .find(|b| {
                self.git
                    .check(
                        repo,
                        &[
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            &format!("refs/heads/{b}"),
                        ],
                    )
                    .unwrap_or(false)
            })
    }

    fn scan_repo(&self, repo: &Path, ctx: &Ctx, in_use: &InUse) -> Result<Vec<(PathBuf, String)>> {
        let Some(default_branch) = self.default_branch(repo) else {
            log::debug!("{}: no main or master branch; skipping", repo.display());
            return Ok(Vec::new());
        };
        let raw = self
            .git
            .output(repo, &["worktree", "list", "--porcelain", "-z"])?;
        let mut found = Vec::new();
        // The first entry is always the main worktree.
        for wt in parse_worktree_list(&raw).iter().skip(1) {
            match self.assess(repo, &default_branch, wt, ctx, in_use) {
                Ok(Some(reason)) => found.push((wt.path.clone(), reason)),
                Ok(None) => {}
                Err(e) => log::warn!("worktree {}: {e:#}", wt.path.display()),
            }
        }
        Ok(found)
    }

    /// The main repo a worktree belongs to, from its common git dir.
    fn repo_of(&self, worktree: &Path) -> Result<PathBuf> {
        let common = self.git.output(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = PathBuf::from(common.trim());
        common
            .parent()
            .map(Path::to_path_buf)
            .with_context(|| format!("git common dir {} has no parent", common.display()))
    }

    fn remove_one(&self, ctx: &Ctx, in_use: &InUse, c: &Candidate) -> Result<(), String> {
        let canon = check_target(&c.path, ctx)?;
        ctx.refuse_kept(&canon)?;
        if in_use.contains(&c.path) {
            return Err("now in use by a running process".to_string());
        }
        if let Some(why) = self
            .git
            .unsafe_to_inspect(&c.path)
            .map_err(|e| format!("{e:#}"))?
        {
            return Err(why);
        }
        let dirty = self
            .git
            .output(
                &c.path,
                &["status", "--porcelain", "--ignore-submodules=all"],
            )
            .map_err(|e| format!("{e:#}"))?;
        if !dirty.trim().is_empty() {
            return Err("now has uncommitted changes".to_string());
        }
        let repo = self.repo_of(&c.path).map_err(|e| format!("{e:#}"))?;
        let path = c.path.to_string_lossy();
        self.git
            .output(&repo, &["worktree", "remove", &path])
            .map_err(|e| format!("{e:#}"))?;
        if let Err(e) = self.git.output(&repo, &["worktree", "prune"]) {
            log::warn!("{}: worktree prune failed: {e:#}", repo.display());
        }
        Ok(())
    }
}

impl CleanupTask for GitWorktrees {
    fn id(&self) -> &'static str {
        "git_worktrees"
    }

    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>> {
        // Without a working in-use list nothing is safe to remove, so the scan fails.
        let in_use = (self.in_use)().context("checking which worktrees are in use")?;
        let mut found = Vec::new();
        let repos = find_repos(ctx);
        for repo in &repos {
            match self.scan_repo(repo, ctx, &in_use) {
                Ok(mut f) => found.append(&mut f),
                Err(e) => log::warn!("{}: {e:#}", repo.display()),
            }
        }
        // Per-worktree reasons for skipping are at debug level; with many
        // worktrees they would otherwise flood the log on every scan.
        log::info!(
            "git_worktrees: {} candidate(s) in {} repo(s)",
            found.len(),
            repos.len()
        );
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
        let mut report = Report::default();
        let in_use = match (self.in_use)() {
            Ok(u) => u,
            Err(e) => {
                report.halted = Some(format!("cannot check which worktrees are in use: {e:#}"));
                return report;
            }
        };
        for c in candidates {
            if !ctx.volume_present() {
                report.halted = Some("the volume is no longer mounted".to_string());
                break;
            }
            match self.remove_one(ctx, &in_use, c) {
                Ok(()) => {
                    log::info!("removed worktree {} ({} bytes)", c.path.display(), c.bytes);
                    report.removed.push(Removed {
                        path: c.path.clone(),
                        bytes: c.bytes,
                    });
                }
                Err(why) => {
                    log::warn!("not removing worktree {}: {why}", c.path.display());
                    report.failed.push((c.path.clone(), why));
                }
            }
        }
        report
    }
}

/// Main repos (folders holding a `.git` directory) under the search roots.
/// Linked worktrees hold a `.git` file instead and are reached through their repo.
fn find_repos(ctx: &Ctx) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    for root in &ctx.search_roots {
        let walker = WalkDir::new(root)
            .follow_links(false)
            .max_depth(SEARCH_DEPTH)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0
                    || (e.file_type().is_dir()
                        && !e
                            .file_name()
                            .to_str()
                            .is_some_and(|n| SKIP_DIRS.contains(&n))
                        && !ctx.exclude.is_match(e.path()))
            });
        for entry in walker.filter_map(|e| e.ok()) {
            let dot_git = entry.path().join(".git");
            if std::fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_dir()) {
                repos.push(entry.path().to_path_buf());
            }
        }
    }
    repos.sort();
    repos.dedup();
    repos
}

fn format_idle(idle: Duration) -> String {
    let mins = idle.as_secs() / 60;
    match mins {
        0..=59 => format!("{mins} min"),
        60..=2879 => format!("{} h", mins / 60),
        _ => format!("{} days", mins / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::*;
    use std::process::Command;

    /// Runs git for fixture setup, isolated from the user's global config so
    /// commit signing or hooks cannot interfere.
    fn git(dir: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn commit(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", file]);
    }

    /// A repo at `root/repo` with one commit on main.
    fn repo(fx: &Fixture) -> PathBuf {
        let r = fx.root.join("repo");
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q"]);
        commit(&r, "README");
        r
    }

    /// Adds a worktree at `repo/.claude/worktrees/<name>` on a new branch.
    fn worktree(repo: &Path, name: &str) -> PathBuf {
        let p = repo.join(".claude/worktrees").join(name);
        git(
            repo,
            &["worktree", "add", "-q", "-b", name, p.to_str().unwrap()],
        );
        p
    }

    fn task(stale_days: u32, in_use: Vec<PathBuf>) -> GitWorktrees {
        let used = InUse::from_cwds(in_use);
        GitWorktrees::new(
            GitCmd::default(),
            stale_days,
            Duration::from_secs(30 * 60),
            Arc::new(move || Ok(used.clone())),
        )
    }

    fn names(found: &[Candidate]) -> Vec<String> {
        let mut v: Vec<String> = found
            .iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn age(fx: &Fixture, wt: &Path, days: u64) {
        let rel = wt
            .strip_prefix(&fx.root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        fx.age(&rel, days);
    }

    #[test]
    fn merged_clean_idle_worktree_is_removed_and_branch_kept() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "done");
        commit(&wt, "feature");
        git(&r, &["merge", "-q", "--ff-only", "done"]);
        age(&fx, &wt, 1);

        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["done"]);
        assert!(
            found[0].reason.starts_with("merged into main, idle"),
            "{}",
            found[0].reason
        );

        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(!wt.exists());
        assert!(
            git(&r, &["branch", "--list", "done"]).contains("done"),
            "branch must survive"
        );
    }

    #[test]
    fn fresh_merged_worktree_waits_for_min_idle() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        worktree(&r, "brand-new");
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn worktree_in_use_is_kept() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "busy");
        age(&fx, &wt, 1);
        let inside = wt.canonicalize().unwrap().join("src");
        assert!(task(14, vec![inside]).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn dirty_and_untracked_worktrees_are_kept() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let dirty = worktree(&r, "dirty");
        std::fs::write(dirty.join("README"), "changed").unwrap();
        let untracked = worktree(&r, "untracked");
        std::fs::write(untracked.join("notes.txt"), "mine").unwrap();
        age(&fx, &dirty, 30);
        age(&fx, &untracked, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn locked_worktree_is_kept() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "locked");
        git(&r, &["worktree", "lock", wt.to_str().unwrap()]);
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn unmerged_branch_without_upstream_is_kept_even_when_stale() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "local-only");
        commit(&wt, "wip");
        age(&fx, &wt, 60);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn pushed_unmerged_branch_is_removed_once_stale() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let remote = fx.dir.path().join("remote.git");
        git(
            fx.dir.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(&r, &["remote", "add", "origin", remote.to_str().unwrap()]);
        let wt = worktree(&r, "pushed");
        commit(&wt, "work");
        git(&wt, &["push", "-q", "-u", "origin", "pushed"]);

        age(&fx, &wt, 5);
        assert!(
            task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty(),
            "not stale yet"
        );
        age(&fx, &wt, 20);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["pushed"]);
        assert_eq!(found[0].reason, "idle 20 days");
    }

    #[test]
    fn detached_head_only_when_contained_in_main() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let on_main = r.join(".claude/worktrees/agent-on-main");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                on_main.to_str().unwrap(),
            ],
        );
        let off_main = r.join(".claude/worktrees/agent-off-main");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                off_main.to_str().unwrap(),
            ],
        );
        commit(&off_main, "orphan-work");
        age(&fx, &on_main, 1);
        age(&fx, &off_main, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["agent-on-main"]);
    }

    #[test]
    fn main_worktree_is_never_a_candidate() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        fx.age("repo", 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.iter().all(|c| c.path != r), "{found:?}");
    }

    #[test]
    fn worktree_outside_the_roots_is_ignored() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let outside = fx.dir.path().join("elsewhere");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "outside",
                outside.to_str().unwrap(),
            ],
        );
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert!(outside.exists());
    }

    #[test]
    fn run_refuses_a_worktree_that_became_dirty() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "late-edit");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        std::fs::write(wt.join("README"), "edited after scan").unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty());
        assert_eq!(report.failed[0].1, "now has uncommitted changes");
        assert!(wt.exists());
    }

    #[test]
    fn scan_fails_when_in_use_cannot_be_checked() {
        let fx = Fixture::new(&[]);
        repo(&fx);
        let t = GitWorktrees::new(
            GitCmd::default(),
            14,
            Duration::from_secs(1800),
            Arc::new(|| anyhow::bail!("lsof broke")),
        );
        assert!(t.scan(&fx.ctx()).is_err());
    }

    /// Writes an executable script that creates `marker` when run.
    fn tripwire(dir: &Path, marker: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("tripwire.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn repo_fsmonitor_is_never_run() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "fsmon");
        let marker = fx.dir.path().join("fsmonitor-ran");
        let script = tripwire(fx.dir.path(), &marker);
        git(&r, &["config", "core.fsmonitor", script.to_str().unwrap()]);
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        t.run(&ctx, &found);
        assert!(!marker.exists(), "repo-configured fsmonitor was executed");
    }

    #[test]
    fn worktree_with_repo_filter_is_skipped_and_filter_never_runs() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let marker = fx.dir.path().join("filter-ran");
        let script = tripwire(fx.dir.path(), &marker);
        std::fs::write(r.join(".gitattributes"), "* filter=evil\n").unwrap();
        git(&r, &["add", ".gitattributes"]);
        git(&r, &["commit", "-qm", "attrs"]);
        let wt = worktree(&r, "filtered");
        git(
            &r,
            &["config", "filter.evil.clean", script.to_str().unwrap()],
        );
        // Stale timestamps make status re-read file contents, which runs clean filters.
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert!(!marker.exists(), "repo-configured filter was executed");
    }

    #[test]
    fn worktree_with_submodule_is_skipped_and_submodule_filter_never_runs() {
        let fx = Fixture::new(&[]);
        let marker = fx.dir.path().join("submodule-filter-ran");
        let script = tripwire(fx.dir.path(), &marker);

        // A library repo whose files go through a filter named "evil".
        let lib = fx.dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        git(&lib, &["init", "-q"]);
        std::fs::write(lib.join(".gitattributes"), "* filter=evil\n").unwrap();
        commit(&lib, "lib.txt");

        let r = repo(&fx);
        let file_ok = ["-c", "protocol.file.allow=always"];
        git(
            &r,
            &[
                &file_ok[..],
                &[
                    "submodule",
                    "add",
                    "-q",
                    lib.to_str().unwrap(),
                    "vendor/lib",
                ],
            ]
            .concat(),
        );
        git(&r, &["commit", "-qm", "add submodule"]);
        let wt = worktree(&r, "with-sub");
        git(
            &wt,
            &[&file_ok[..], &["submodule", "update", "-q", "--init"]].concat(),
        );
        // The filter lives only in the submodule's own config.
        git(
            &wt.join("vendor/lib"),
            &["config", "filter.evil.clean", script.to_str().unwrap()],
        );
        age(&fx, &wt, 1);

        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(!marker.exists(), "submodule-configured filter was executed");
        assert!(found.is_empty(), "{found:?}");
    }

    /// Claude Code's agent worktrees have a submodule in the index but never
    /// check it out, so its folder is empty and git can remove the worktree.
    #[test]
    fn worktree_with_unchecked_out_submodule_is_removed() {
        let fx = Fixture::new(&[]);
        let lib = fx.dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        git(&lib, &["init", "-q"]);
        commit(&lib, "lib.txt");

        let r = repo(&fx);
        git(
            &r,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/lib",
            ],
        );
        git(&r, &["commit", "-qm", "add submodule"]);
        let wt = worktree(&r, "agent-sub");
        assert!(wt.join(".gitmodules").exists());
        assert_eq!(std::fs::read_dir(wt.join("vendor/lib")).unwrap().count(), 0);
        age(&fx, &wt, 1);

        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["agent-sub"]);
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(!wt.exists());
    }

    /// Files in a gitlink folder that is not checked out do not show in `git
    /// status`, and `git worktree remove` would delete them.
    #[test]
    fn worktree_with_files_in_unchecked_out_submodule_folder_is_kept() {
        let fx = Fixture::new(&[]);
        let lib = fx.dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        git(&lib, &["init", "-q"]);
        commit(&lib, "lib.txt");

        let r = repo(&fx);
        git(
            &r,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/lib",
            ],
        );
        git(&r, &["commit", "-qm", "add submodule"]);
        let wt = worktree(&r, "stray-files");
        std::fs::write(wt.join("vendor/lib/notes.txt"), "mine").unwrap();
        age(&fx, &wt, 1);

        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn formats_idle() {
        assert_eq!(format_idle(Duration::from_secs(45 * 60)), "45 min");
        assert_eq!(format_idle(Duration::from_secs(5 * 3600)), "5 h");
        assert_eq!(format_idle(Duration::from_secs(3 * 86_400)), "3 days");
    }
}
