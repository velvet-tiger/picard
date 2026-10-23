//! Removes linked git worktrees whose work is safely elsewhere.
//!
//! A worktree is removed only when every one of these holds:
//! - it is a linked worktree (not the main one), not locked, not prunable
//! - it sits inside a root on this volume and is not excluded (`check_target`)
//! - no running process is working inside it (`lsof`)
//! - `git status --porcelain` is empty, so nothing uncommitted or untracked
//! - any checked-out submodule is clean and holds no commit missing from its
//!   remote branches (`GitCmd::unsafe_to_inspect`)
//! - it is done and the folder has been idle for `min_idle`, or its own
//!   branch has an upstream with nothing unpushed and the folder has been
//!   idle for `stale_days`
//!
//! A worktree is done when another local branch holds its HEAD. Any branch in
//! `refs/heads` counts except the worktree's own, even one checked out in
//! another linked worktree. Remote-tracking branches never count. No default
//! branch is needed, so a repo whose only branch is `trunk` is checked too.
//!
//! A worktree is also done when no other branch holds HEAD, but other local
//! branches hold a cherry-picked copy of every commit no other branch holds.
//! Git's patch ids decide what is a copy, as they do when `git rebase` drops
//! commits that are already upstream. They ignore whitespace, and an empty
//! commit matches any other empty commit. They skip merge commits, so a merge
//! among those commits keeps the worktree: it could hold a conflict
//! resolution that no copy has.
//!
//! A branch holding the commits shows they survive, not that the work was
//! finished. A branch stacked on top of the worktree's branch counts, and so
//! does a branch that merged it in only to sync. The worktree's own branch is
//! kept, so no commit is lost. A detached worktree has no branch, so only the
//! copies of its cherry-picked commits remain. Ignored files in the worktree
//! are lost either way.
//!
//! The candidate reason says why a worktree is done. When a merge commit, or a
//! `merge` entry in another branch's reflog, records the merge, the reason
//! names it, as in "merged into dev by abc1234" or "merged into dev
//! (reflog)". Otherwise it names the branch, as in "on main", or says
//! "cherry-picked to another branch". Only the wording depends on this.
//!
//! Removal is `git worktree remove` without `--force`, so git refuses anything
//! dirty or locked as a second guard. The exception is a worktree with
//! checked-out submodules, which git removes only with `--force`. One `--force`
//! still refuses a locked worktree. Branches are never deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::git::{GitCmd, SAFETY_STATUS, WorktreeEntry, parse_worktree_list};
use super::in_use::InUse;
use super::stale_dirs::{FINDER_METADATA, is_skipped, newest_mtime};
use super::{Candidate, CleanupTask, Ctx, Estimate, Removed, Report, check_target};

/// Folders never descended into while looking for repos.
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git"];
const SEARCH_DEPTH: usize = 8;
const SECS_PER_DAY: u64 = 86_400;

/// Supplies the in-use list. `lsof` in the app, a fixed list in tests.
pub type InUseSource = Arc<dyn Fn() -> Result<InUse> + Send + Sync>;

/// Where a worktree's commits are, which decides whether and when it can go.
#[derive(Debug, PartialEq)]
enum Standing {
    /// Another local branch holds its commits, as they are or as
    /// cherry-picked copies. It can go once idle for `min_idle`. Holds the
    /// start of the candidate reason, such as "on main".
    Merged(String),
    /// Only its upstream holds its commits. It can go once idle for
    /// `stale_days`.
    Elsewhere,
    /// Removing it could lose commits.
    Unsafe,
}

/// The candidate reason for a worktree whose commits were all cherry-picked.
const CHERRY_PICKED: &str = "cherry-picked to another branch";

/// The branches of one repo that every worktree in it is judged against.
struct RepoBranches {
    /// Every local branch, as a full ref name such as `refs/heads/main`.
    local: Vec<String>,
    /// The branch checked out in the main worktree, if any. It proves nothing
    /// about merges. It only goes first when a reason names a branch.
    main_checkout: Option<String>,
}

impl RepoBranches {
    /// Every local branch but `own`, as full ref names. A full name can never
    /// be read as an option or as a commit id.
    fn others(&self, own: Option<&str>) -> Vec<&str> {
        self.local
            .iter()
            .map(String::as_str)
            .filter(|r| own.is_none_or(|b| r.strip_prefix("refs/heads/") != Some(b)))
            .collect()
    }
}

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
        branches: &RepoBranches,
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
        if !self.git.output(&wt.path, SAFETY_STATUS)?.trim().is_empty() {
            log::debug!("worktree {}: has uncommitted changes", wt.path.display());
            return Ok(None);
        }
        let standing = self.standing(repo, branches, wt)?;
        if standing == Standing::Unsafe {
            log::debug!(
                "worktree {}: has commits that no other local branch holds, even as cherry-picked copies, and that are not pushed",
                wt.path.display()
            );
            return Ok(None);
        }
        let Some(newest) = newest_mtime(&wt.path, &ctx.not_activity) else {
            return Ok(None);
        };
        let Ok(idle) = ctx.now.duration_since(newest) else {
            return Ok(None);
        };
        let stale = Duration::from_secs(u64::from(self.stale_days) * SECS_PER_DAY);
        if let Standing::Merged(how) = &standing
            && idle >= self.min_idle
        {
            return Ok(Some(format!("{how}, idle {}", format_idle(idle))));
        }
        if idle >= stale {
            return Ok(Some(format!("idle {}", format_idle(idle))));
        }
        Ok(None)
    }

    /// Decides where a worktree's commits are. Reads only; runs no program a
    /// repo can configure.
    fn standing(
        &self,
        repo: &Path,
        branches: &RepoBranches,
        wt: &WorktreeEntry,
    ) -> Result<Standing> {
        let others = self.other_branches_containing(repo, branches, wt)?;
        if let Some(first) = others.first() {
            // A recorded merge only changes the wording, never the timing.
            let how = match self.merge_evidence(repo, &wt.head, &others)? {
                Some(how) => how,
                None => format!("on {first}"),
            };
            return Ok(Standing::Merged(how));
        }
        if self.cherry_picked(repo, branches, wt)? {
            return Ok(Standing::Merged(CHERRY_PICKED.to_string()));
        }
        let pushed = match &wt.branch {
            Some(branch) => self.fully_pushed(repo, branch)?,
            None => false,
        };
        Ok(if pushed {
            Standing::Elsewhere
        } else {
            Standing::Unsafe
        })
    }

    /// Local branches that contain the worktree's HEAD, apart from its own.
    /// The branch checked out in the main worktree comes first, then the rest
    /// by name. The order only decides which branch a reason names.
    fn other_branches_containing(
        &self,
        repo: &Path,
        branches: &RepoBranches,
        wt: &WorktreeEntry,
    ) -> Result<Vec<String>> {
        let raw = self.git.output(
            repo,
            &[
                "for-each-ref",
                "--contains",
                &wt.head,
                "--format=%(refname)",
                "refs/heads/",
            ],
        )?;
        let mut others: Vec<String> = raw
            .lines()
            .filter_map(|r| r.strip_prefix("refs/heads/"))
            .filter(|b| wt.branch.as_deref() != Some(*b))
            .map(str::to_string)
            .collect();
        if let Some(main) = &branches.main_checkout
            && let Some(i) = others.iter().position(|b| b == main)
        {
            let b = others.remove(i);
            others.insert(0, b);
        }
        Ok(others)
    }

    /// Why `head` counts as merged into one of `branches`, or None if nothing
    /// shows it was. A merge commit is checked first, then the reflogs.
    fn merge_evidence(
        &self,
        repo: &Path,
        head: &str,
        branches: &[String],
    ) -> Result<Option<String>> {
        if branches.is_empty() {
            return Ok(None);
        }
        if let Some(merge) = self.merge_commit_of(repo, head, branches)? {
            for b in branches {
                let tip = format!("refs/heads/{b}");
                if self
                    .git
                    .check(repo, &["merge-base", "--is-ancestor", &merge, &tip])?
                {
                    let short: String = merge.chars().take(7).collect();
                    return Ok(Some(format!("merged into {b} by {short}")));
                }
            }
        }
        Ok(self
            .reflog_merged_into(repo, head, branches)?
            .map(|b| format!("merged into {b} (reflog)")))
    }

    /// A merge commit in `branches` that has `head` as a non-first parent.
    /// Refs are passed in full, so a branch name can never read as an option.
    fn merge_commit_of(
        &self,
        repo: &Path,
        head: &str,
        branches: &[String],
    ) -> Result<Option<String>> {
        let exclude = format!("^{head}");
        let tips: Vec<String> = branches.iter().map(|b| format!("refs/heads/{b}")).collect();
        let mut args = vec![
            "rev-list",
            "--merges",
            "--parents",
            "--ancestry-path",
            exclude.as_str(),
        ];
        args.extend(tips.iter().map(String::as_str));
        let raw = self.git.output(repo, &args)?;
        Ok(raw.lines().find_map(|line| {
            let mut ids = line.split_whitespace();
            let merge = ids.next()?;
            // The first parent is the branch merged into, so skip it.
            ids.skip(1)
                .any(|parent| parent == head)
                .then(|| merge.to_string())
        }))
    }

    /// The first of `branches` whose reflog records a `merge` that moved it to
    /// `head`. One git call reads every reflog, so a repo with many branches
    /// costs one process per worktree. A missing or expired reflog reads as
    /// empty. `--no-show-signature` stops repo config from making `log` run gpg.
    fn reflog_merged_into(
        &self,
        repo: &Path,
        head: &str,
        branches: &[String],
    ) -> Result<Option<String>> {
        let refs: Vec<String> = branches.iter().map(|b| format!("refs/heads/{b}")).collect();
        let mut args = vec![
            "log",
            "--walk-reflogs",
            "--no-show-signature",
            "--format=%H%x09%gD%x09%gs",
        ];
        args.extend(refs.iter().map(String::as_str));
        let raw = self.git.output(repo, &args)?;
        let merged: Vec<&str> = raw
            .lines()
            .filter_map(|line| reflog_merge_branch(line, head))
            .collect();
        Ok(branches
            .iter()
            .find(|b| merged.contains(&b.as_str()))
            .cloned())
    }

    /// True if other local branches hold a cherry-picked copy of every commit
    /// that no other branch holds, and none of those commits is a merge. Run
    /// only when no other branch holds HEAD itself.
    ///
    /// The first call lists those commits with their parents, which shows any
    /// merge. The second marks each one that has a patch-equivalent copy on
    /// the right of some `HEAD...branch`. A commit that some branch holds
    /// drops out of the left side, so the marked set must equal the listed
    /// one exactly. A ref name cannot contain `..`, so each range parses as
    /// one. Computing patch ids runs no diff driver or textconv the repo names.
    fn cherry_picked(
        &self,
        repo: &Path,
        branches: &RepoBranches,
        wt: &WorktreeEntry,
    ) -> Result<bool> {
        let others = branches.others(wt.branch.as_deref());
        if others.is_empty() {
            return Ok(false);
        }
        let mut args = vec!["rev-list", "--parents", wt.head.as_str(), "--not"];
        args.extend(others.iter().copied());
        let listed = self.git.output(repo, &args)?;
        let Some(unique) = non_merge_commits(&listed) else {
            return Ok(false);
        };
        if unique.is_empty() {
            return Ok(false);
        }
        let ranges: Vec<String> = others
            .iter()
            .map(|r| format!("{}...{r}", wt.head))
            .collect();
        let mut args = vec!["rev-list", "--left-only", "--cherry-mark", "--no-merges"];
        args.extend(ranges.iter().map(String::as_str));
        let marked = self.git.output(repo, &args)?;
        Ok(patch_equivalent(&marked) == unique)
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

    /// Every local branch of `repo`, as full ref names.
    fn local_branches(&self, repo: &Path) -> Result<Vec<String>> {
        let raw = self.git.output(
            repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
        )?;
        Ok(raw.lines().map(str::to_string).collect())
    }

    fn scan_repo(&self, repo: &Path, ctx: &Ctx, in_use: &InUse) -> Result<Vec<(PathBuf, String)>> {
        let raw = self
            .git
            .output(repo, &["worktree", "list", "--porcelain", "-z"])?;
        let worktrees = parse_worktree_list(&raw);
        // The first entry is always the main worktree.
        let branches = RepoBranches {
            local: self.local_branches(repo)?,
            main_checkout: worktrees.first().and_then(|w| w.branch.clone()),
        };
        let mut found = Vec::new();
        for wt in worktrees.iter().skip(1) {
            if ctx.stopped() {
                break;
            }
            match self.assess(repo, &branches, wt, ctx, in_use) {
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
        // The submodule checks in `unsafe_to_inspect` and the status check below
        // stand in for git's own, which refuses every worktree with submodules.
        // Read before those checks: if submodules appear after it, plain
        // `worktree remove` refuses, so a late change cannot earn `--force`.
        let force = self
            .git
            .has_submodule_stores(&c.path)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(why) = self
            .git
            .unsafe_to_inspect(&c.path)
            .map_err(|e| format!("{e:#}"))?
        {
            return Err(why);
        }
        let dirty = self
            .git
            .output(&c.path, SAFETY_STATUS)
            .map_err(|e| format!("{e:#}"))?;
        if !dirty.trim().is_empty() {
            return Err("now has uncommitted changes".to_string());
        }
        // The checks above take seconds on a large repo, and `--force` skips
        // git's own. A file written since the run started, even an ignored
        // one that status does not list, means someone is working here. A
        // commit in a submodule writes only to its repository, so those are
        // walked too when they are about to be deleted.
        let mut walked = vec![(c.path.clone(), true)];
        if force {
            let modules = self
                .git
                .submodule_store_root(&c.path)
                .map_err(|e| format!("{e:#}"))?;
            walked.push((modules, false));
        }
        if walked
            .iter()
            .any(|(dir, skip_build)| written_since(dir, ctx.now, *skip_build))
        {
            return Err("changed since the run started".to_string());
        }
        let repo = self.repo_of(&c.path).map_err(|e| format!("{e:#}"))?;
        let path = c.path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        self.git
            .output(&repo, &args)
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
            if ctx.stopped() {
                break;
            }
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
        let sizes = ctx.measure_all(&paths);
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
            if ctx.stopped() {
                break;
            }
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

/// True if any file under `dir` was modified after `since`, or if any part
/// of it cannot be read. Unlike `newest_mtime`, which estimates idleness,
/// this guards a removal: it has no depth limit and fails closed. With
/// `skip_build`, regenerable build folders and `.git` are not walked, as in
/// the scan. Finder's `.DS_Store` never counts as work.
fn written_since(dir: &Path, since: SystemTime, skip_build: bool) -> bool {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !skip_build || !is_skipped(e.path(), &[]))
        .any(|entry| match entry {
            Err(e) => {
                log::debug!("{}: cannot check for recent writes: {e}", dir.display());
                true
            }
            Ok(e) if e.file_type().is_dir() || e.file_name() == FINDER_METADATA => false,
            Ok(e) => e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .is_none_or(|modified| modified > since),
        })
}

/// The branch a reflog line belongs to, if the line records a `merge` that
/// moved the branch to `head`. Lines are `<commit>\t<ref>@{<n>}\t<subject>`.
/// Git refuses `@{` in branch names, so the last one ends the name.
fn reflog_merge_branch<'a>(line: &'a str, head: &str) -> Option<&'a str> {
    let mut fields = line.splitn(3, '\t');
    let (id, selector, subject) = (fields.next()?, fields.next()?, fields.next()?);
    if id != head || !subject.starts_with("merge ") {
        return None;
    }
    let (name, _) = selector.strip_prefix("refs/heads/")?.rsplit_once("@{")?;
    Some(name)
}

/// The commit ids in `git rev-list --parents` output, or None if any of them
/// is a merge. Each line is a commit id followed by its parents' ids.
fn non_merge_commits(raw: &str) -> Option<HashSet<&str>> {
    raw.lines()
        .filter_map(|line| {
            let mut ids = line.split_whitespace();
            let id = ids.next()?;
            Some((ids.count() <= 1).then_some(id))
        })
        .collect()
}

/// The commits that `git rev-list --cherry-mark` marks `=`, because the other
/// side holds a patch-equivalent copy. Each line is the mark and a commit id.
/// Any other line is skipped, so output this does not expect can only make a
/// match fail.
fn patch_equivalent(raw: &str) -> HashSet<&str> {
    raw.lines().filter_map(|l| l.strip_prefix('=')).collect()
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
            // A commit otherwise starts a detached `git maintenance` that can
            // add and remove files in .git while Fixture::age walks the repo.
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
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
        // The fast-forward left a `merge` entry in main's reflog.
        assert!(
            found[0]
                .reason
                .starts_with("merged into main (reflog), idle"),
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

    /// Checks out a new `dev` branch in the main worktree, as a repo that
    /// integrates somewhere other than its default branch does.
    fn integrate_on_dev(repo: &Path) {
        git(repo, &["checkout", "-q", "-b", "dev"]);
    }

    fn reason_of<'a>(found: &'a [Candidate], name: &str) -> &'a str {
        &found
            .iter()
            .find(|c| c.path.file_name().is_some_and(|n| n == name))
            .unwrap_or_else(|| panic!("{name} is not a candidate: {found:?}"))
            .reason
    }

    #[test]
    fn merge_commit_into_another_branch_counts_as_merged() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        integrate_on_dev(&r);
        let wt = worktree(&r, "feat");
        commit(&wt, "feature");
        git(&r, &["merge", "-q", "--no-ff", "-m", "merge feat", "feat"]);
        // Cut after the merge, so it holds the merge commit too. It sorts
        // before "dev", so only the main checkout coming first names dev.
        git(&r, &["branch", "aaa-later"]);
        age(&fx, &wt, 1);

        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["feat"]);
        let reason = reason_of(&found, "feat");
        assert!(reason.starts_with("merged into dev by "), "{reason}");
        assert!(reason.ends_with(", idle 24 h"), "{reason}");

        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(git(&r, &["branch", "--list", "feat"]).contains("feat"));
    }

    #[test]
    fn fast_forward_into_another_branch_counts_as_merged_from_the_reflog() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        integrate_on_dev(&r);
        let wt = worktree(&r, "ff");
        commit(&wt, "feature");
        git(&r, &["merge", "-q", "--ff-only", "ff"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["ff"]);
        assert_eq!(
            reason_of(&found, "ff"),
            "merged into dev (reflog), idle 24 h"
        );
    }

    #[test]
    fn detached_head_at_a_merge_result_counts_as_merged_from_the_reflog() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        integrate_on_dev(&r);
        let feat = worktree(&r, "feat");
        commit(&feat, "feature");
        git(&r, &["merge", "-q", "--no-ff", "-m", "merge feat", "feat"]);
        let at_merge = r.join(".claude/worktrees/at-merge");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                at_merge.to_str().unwrap(),
                "dev",
            ],
        );
        age(&fx, &at_merge, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "at-merge"),
            "merged into dev (reflog), idle 24 h"
        );
    }

    /// A branch stacked on top holds the commits, though nothing merged them.
    /// Removing the worktree loses no commit, so it is done.
    #[test]
    fn branch_held_by_another_branch_without_a_merge_counts_as_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "base");
        commit(&wt, "work");
        git(&r, &["branch", "stacked", "base"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["base"]);
        assert_eq!(reason_of(&found, "base"), "on stacked, idle 24 h");
    }

    /// A detached worktree whose commit is held by a branch made for it, as
    /// Claude Code does. The branch keeps the commit, so the worktree is done.
    #[test]
    fn detached_head_held_by_a_branch_made_for_it_counts_as_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = r.join(".claude/worktrees/agent-own");
        git(
            &r,
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        );
        commit(&wt, "agent-work");
        let head = git(&wt, &["rev-parse", "HEAD"]);
        git(&r, &["branch", "agent-own-branch", head.trim()]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "agent-own"),
            "on agent-own-branch, idle 24 h"
        );
    }

    /// When a branch merges something else in, the branch's own commit is
    /// the merge's first parent. That is not evidence it was merged anywhere,
    /// so the reason names the branch without claiming a merge.
    #[test]
    fn first_parent_of_a_merge_is_named_by_its_branch_not_as_merged() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "base");
        commit(&wt, "work");
        git(&r, &["branch", "other", "main"]);
        git(&r, &["checkout", "-q", "other"]);
        commit(&r, "other-work");
        git(&r, &["checkout", "-q", "-b", "combo", "base"]);
        git(
            &r,
            &["merge", "-q", "--no-ff", "-m", "merge other", "other"],
        );
        git(&r, &["checkout", "-q", "main"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["base"]);
        assert_eq!(reason_of(&found, "base"), "on combo, idle 24 h");
    }

    #[test]
    fn every_branch_in_an_octopus_merge_counts_as_merged() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        integrate_on_dev(&r);
        let a = worktree(&r, "octo-a");
        commit(&a, "a.txt");
        let b = worktree(&r, "octo-b");
        commit(&b, "b.txt");
        git(
            &r,
            &[
                "merge", "-q", "--no-ff", "-m", "octopus", "octo-a", "octo-b",
            ],
        );
        age(&fx, &a, 1);
        age(&fx, &b, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        for name in ["octo-a", "octo-b"] {
            let reason = reason_of(&found, name);
            assert!(reason.starts_with("merged into dev by "), "{reason}");
        }
    }

    /// Without its reflog, a fast-forward leaves no record. The commits are
    /// still in another branch, so the worktree is done, and the reason names
    /// that branch.
    #[test]
    fn fast_forward_with_an_expired_reflog_is_named_by_its_branch() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        integrate_on_dev(&r);
        let wt = worktree(&r, "ff-old");
        commit(&wt, "feature");
        git(&r, &["merge", "-q", "--ff-only", "ff-old"]);
        git(&r, &["reflog", "expire", "--expire=now", "--all"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "ff-old"), "on dev, idle 24 h");
    }

    /// Git cannot tell which way a merge went. Merging a branch into another
    /// to sync it, here an integration branch into a feature branch, counts
    /// as merged. The README and module doc say so. The branch is kept.
    #[test]
    fn a_branch_merged_in_to_sync_another_counts_as_merged() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let dev = worktree(&r, "dev");
        commit(&dev, "integration-work");
        git(&r, &["checkout", "-q", "-b", "feature"]);
        commit(&r, "feature-work");
        git(&r, &["merge", "-q", "--no-ff", "-m", "sync dev", "dev"]);
        age(&fx, &dev, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        let reason = reason_of(&found, "dev");
        assert!(reason.starts_with("merged into feature by "), "{reason}");
    }

    fn head_of(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"]).trim().to_string()
    }

    /// Adds a worktree at `repo/.claude/worktrees/<name>` with a detached HEAD.
    fn detached(repo: &Path, name: &str) -> PathBuf {
        let p = repo.join(".claude/worktrees").join(name);
        git(
            repo,
            &["worktree", "add", "-q", "--detach", p.to_str().unwrap()],
        );
        p
    }

    /// Cherry-picks `ids` onto `branch` in the main worktree, then checks out
    /// `main` again. A commit of the branch's own goes first, so each copy
    /// gets a new parent and a new id, even within the same second. It adds
    /// only its own file: `git add .` would add the linked worktrees under
    /// `.claude/worktrees` as embedded repositories.
    fn pick_onto(repo: &Path, branch: &str, ids: &[&str]) {
        git(repo, &["checkout", "-q", branch]);
        let file = format!("{branch}-work");
        std::fs::write(repo.join(&file), &file).unwrap();
        git(repo, &["add", "--", &file]);
        git(repo, &["commit", "-qm", &file]);
        for id in ids {
            git(repo, &["cherry-pick", id]);
        }
        git(repo, &["checkout", "-q", "main"]);
    }

    /// True if any local branch holds `id`.
    fn on_a_branch(repo: &Path, id: &str) -> bool {
        !git(repo, &["branch", "--contains", id]).trim().is_empty()
    }

    #[test]
    fn detached_commit_cherry_picked_to_main_counts_as_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "agent-picked");
        commit(&wt, "agent-work");
        let id = head_of(&wt);
        pick_onto(&r, "main", &[&id]);
        assert!(!on_a_branch(&r, &id), "only a copy may be on main");
        age(&fx, &wt, 1);

        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["agent-picked"]);
        assert_eq!(
            reason_of(&found, "agent-picked"),
            "cherry-picked to another branch, idle 24 h"
        );
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(!wt.exists());
    }

    #[test]
    fn unpushed_branch_whose_commits_were_all_cherry_picked_counts_as_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "picked");
        commit(&wt, "one");
        let one = head_of(&wt);
        commit(&wt, "two");
        let two = head_of(&wt);
        pick_onto(&r, "main", &[&one, &two]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["picked"]);
        assert_eq!(
            reason_of(&found, "picked"),
            "cherry-picked to another branch, idle 24 h"
        );
    }

    #[test]
    fn picks_spread_over_two_branches_count_as_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "spread");
        commit(&wt, "one");
        let one = head_of(&wt);
        commit(&wt, "two");
        let two = head_of(&wt);
        git(&r, &["branch", "left", "main"]);
        git(&r, &["branch", "right", "main"]);
        pick_onto(&r, "left", &[&one]);
        pick_onto(&r, "right", &[&two]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "spread"),
            "cherry-picked to another branch, idle 24 h"
        );
    }

    #[test]
    fn worktree_with_only_some_commits_cherry_picked_is_kept() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "half");
        commit(&wt, "one");
        let one = head_of(&wt);
        commit(&wt, "two");
        pick_onto(&r, "main", &[&one]);
        age(&fx, &wt, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// Patch ids skip merges, so a merge could hold work, such as a conflict
    /// resolution, that no copy has.
    #[test]
    fn merge_among_the_commits_blocks_a_cherry_pick_match() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "mixed");
        commit(&wt, "one");
        let one = head_of(&wt);
        git(&wt, &["checkout", "-q", "--detach"]);
        commit(&wt, "side");
        let side = head_of(&wt);
        git(&wt, &["checkout", "-q", "mixed"]);
        git(&wt, &["merge", "-q", "--no-ff", "-m", "merge side", &side]);
        // Every commit but the merge has a copy on main.
        pick_onto(&r, "main", &[&one, &side]);
        age(&fx, &wt, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// Patch ids come from git's own diff, so the cherry-pick check runs no
    /// external diff or textconv program that the repo config names.
    #[test]
    fn cherry_pick_check_runs_no_repo_diff_program() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let marker = fx.dir.path().join("diff-program-ran");
        let script = tripwire(fx.dir.path(), &marker);
        let script = script.to_str().unwrap();
        std::fs::write(r.join(".gitattributes"), "* diff=evil\n").unwrap();
        git(&r, &["add", ".gitattributes"]);
        git(&r, &["commit", "-qm", "attrs"]);
        let wt = detached(&r, "diffed");
        commit(&wt, "agent-work");
        let id = head_of(&wt);
        pick_onto(&r, "main", &[&id]);
        for key in ["diff.external", "diff.evil.command", "diff.evil.textconv"] {
            git(&r, &["config", key, script]);
        }
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(
            !marker.exists(),
            "repo-configured diff program was executed"
        );
        assert_eq!(
            reason_of(&found, "diffed"),
            "cherry-picked to another branch, idle 24 h"
        );
    }

    #[test]
    fn head_on_a_branch_checked_out_in_another_worktree_counts_as_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let first = worktree(&r, "first");
        commit(&first, "work");
        let second = r.join(".claude/worktrees/second");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "second",
                second.to_str().unwrap(),
                "first",
            ],
        );
        age(&fx, &first, 1);
        // The second worktree is fresh, so only the first is a candidate.
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["first"]);
        assert_eq!(reason_of(&found, "first"), "on second, idle 24 h");
    }

    #[test]
    fn repo_without_main_or_master_is_still_checked() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        git(&r, &["branch", "-m", "main", "trunk"]);
        assert_eq!(
            git(&r, &["for-each-ref", "--format=%(refname)", "refs/heads/"]),
            "refs/heads/trunk\n"
        );
        let held = worktree(&r, "held");
        let orphan = detached(&r, "orphan");
        commit(&orphan, "orphan-work");
        age(&fx, &held, 1);
        age(&fx, &orphan, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["held"]);
        assert_eq!(reason_of(&found, "held"), "on trunk, idle 24 h");
    }

    #[test]
    fn reads_commits_and_merges_from_rev_list_parents() {
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|x| x.repeat(40));
        let linear = format!("{a} {b}\n{b} {c}\n{c}\n");
        assert_eq!(
            non_merge_commits(&linear),
            Some(HashSet::from([a.as_str(), b.as_str(), c.as_str()]))
        );
        let with_merge = format!("{a} {b}\n{b} {c} {d}\n");
        assert_eq!(non_merge_commits(&with_merge), None);
        assert_eq!(non_merge_commits(""), Some(HashSet::new()));
    }

    #[test]
    fn reads_patch_equivalent_commits_from_cherry_mark() {
        let [a, b, c] = ["a", "b", "c"].map(|x| x.repeat(40));
        // A `commit` header, as `--format` would add, is not a mark.
        let raw = format!("={a}\n+{b}\ncommit ={c}\n");
        assert_eq!(patch_equivalent(&raw), HashSet::from([a.as_str()]));
    }

    /// A detached worktree with a commit no branch holds, and a repo that
    /// rewrites `main`'s history to claim that commit as a parent.
    fn detached_commit_claimed_by_main(fx: &Fixture, name: &str) -> (PathBuf, PathBuf, String) {
        let r = repo(fx);
        let wt = r.join(".claude/worktrees").join(name);
        git(
            &r,
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        );
        commit(&wt, "orphan-work");
        let x = git(&wt, &["rev-parse", "HEAD"]).trim().to_string();
        (r, wt, x)
    }

    #[test]
    fn replace_ref_cannot_make_a_commit_look_merged() {
        let fx = Fixture::new(&[]);
        let (r, wt, x) = detached_commit_claimed_by_main(&fx, "replaced");
        git(&r, &["replace", "--graft", "main", &x]);
        age(&fx, &wt, 30);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn grafts_keep_the_worktree() {
        let fx = Fixture::new(&[]);
        let (r, wt, x) = detached_commit_claimed_by_main(&fx, "grafted");
        let main = git(&r, &["rev-parse", "main"]);
        std::fs::write(r.join(".git/info/grafts"), format!("{} {x}\n", main.trim())).unwrap();
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert_eq!(
            refusal(&wt),
            "repo has info/grafts, which can make history look merged"
        );
    }

    #[test]
    fn reads_the_branch_from_a_reflog_merge_line() {
        let head = "a".repeat(40);
        let line = |id: &str, subject: &str| format!("{id}\trefs/heads/fix/x@y@{{3}}\t{subject}");
        assert_eq!(
            reflog_merge_branch(&line(&head, "merge fix/z: Fast-forward"), &head),
            Some("fix/x@y")
        );
        assert_eq!(
            reflog_merge_branch(&line(&head, "branch: Created from main"), &head),
            None
        );
        assert_eq!(
            reflog_merge_branch(&line(&"b".repeat(40), "merge z: Fast-forward"), &head),
            None
        );
        assert_eq!(reflog_merge_branch("garbage", &head), None);
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
        assert_eq!(
            refusal(&wt),
            "submodule vendor/lib config defines filters that git could run: filter.evil.clean"
        );
        assert!(!marker.exists(), "submodule-configured filter was executed");
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

    /// A repo whose main branch has a submodule at `vendor/lib`. The library
    /// sits outside the roots and is the submodule's `origin`, so a fresh
    /// checkout's commit is on a remote branch.
    fn repo_with_submodule(fx: &Fixture) -> PathBuf {
        let lib = fx.dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        git(&lib, &["init", "-q"]);
        commit(&lib, "lib.txt");
        let r = repo(fx);
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
        r
    }

    /// A worktree of `repo` with its submodule checked out.
    fn worktree_with_checkout(repo: &Path, name: &str) -> PathBuf {
        let wt = worktree(repo, name);
        git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "-q",
                "--init",
            ],
        );
        wt
    }

    /// Why `unsafe_to_inspect` refuses `wt`. Asserting the reason shows a
    /// worktree is kept by the submodule check, not by some other failure.
    fn refusal(wt: &Path) -> String {
        GitCmd::default()
            .unsafe_to_inspect(wt)
            .unwrap()
            .unwrap_or_else(|| panic!("{} was not refused", wt.display()))
    }

    #[test]
    fn clean_checked_out_submodule_on_its_remote_is_removed() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "with-lib");
        assert!(wt.join("vendor/lib/lib.txt").exists());
        age(&fx, &wt, 1);

        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["with-lib"]);
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(!wt.exists());
        assert!(
            !r.join(".git/worktrees/with-lib").exists(),
            "admin folder must go too"
        );
    }

    #[test]
    fn submodule_commit_on_no_remote_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "sub-commit");
        // The superproject's status ignores submodules, so only the submodule
        // check can see this commit.
        commit(&wt.join("vendor/lib"), "local-only.txt");
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert!(refusal(&wt).ends_with("has 1 commit(s) that are on no remote branch"));
    }

    #[test]
    fn submodule_stash_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "sub-stash");
        let sub = wt.join("vendor/lib");
        std::fs::write(sub.join("lib.txt"), "stashed edit").unwrap();
        git(&sub, &["stash", "-q"]);
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert!(
            refusal(&wt).contains("on no remote branch"),
            "{}",
            refusal(&wt)
        );
    }

    #[test]
    fn dirty_or_untracked_submodule_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let dirty = worktree_with_checkout(&r, "sub-dirty");
        std::fs::write(dirty.join("vendor/lib/lib.txt"), "changed").unwrap();
        let untracked = worktree_with_checkout(&r, "sub-untracked");
        std::fs::write(untracked.join("vendor/lib/notes.txt"), "mine").unwrap();
        age(&fx, &dirty, 30);
        age(&fx, &untracked, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        for wt in [&dirty, &untracked] {
            assert_eq!(refusal(wt), "submodule vendor/lib has uncommitted changes");
        }
    }

    #[test]
    fn deinitialised_submodule_repo_with_a_local_commit_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "sub-deinit");
        commit(&wt.join("vendor/lib"), "local-only.txt");
        // Deinit empties the folder but keeps the submodule's repository.
        git(&wt, &["submodule", "deinit", "-q", "-f", "vendor/lib"]);
        assert_eq!(std::fs::read_dir(wt.join("vendor/lib")).unwrap().count(), 0);
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert!(refusal(&wt).ends_with("has 1 commit(s) that are on no remote branch"));
    }

    #[test]
    fn run_refuses_when_a_submodule_gains_a_commit_after_the_scan() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "sub-late");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        commit(&wt.join("vendor/lib"), "late.txt");
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(
            report.failed[0].1.contains("on no remote branch"),
            "{}",
            report.failed[0].1
        );
        assert!(wt.exists());
    }

    #[test]
    fn force_still_refuses_a_worktree_locked_after_the_scan() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "sub-locked");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        git(&r, &["worktree", "lock", wt.to_str().unwrap()]);
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(
            report.failed[0].1.contains("locked"),
            "{}",
            report.failed[0].1
        );
        assert!(wt.exists());
    }

    #[test]
    fn submodule_with_submodules_of_its_own_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let inner = fx.dir.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        git(&inner, &["init", "-q"]);
        commit(&inner, "inner.txt");
        let r = repo_with_submodule(&fx);
        let lib = fx.dir.path().join("lib");
        git(
            &lib,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                inner.to_str().unwrap(),
                "inner",
            ],
        );
        git(&lib, &["commit", "-qm", "add inner"]);
        let wt = worktree_with_checkout(&r, "sub-nested");
        let sub = wt.join("vendor/lib");
        git(&sub, &["fetch", "-q", "origin"]);
        git(&sub, &["checkout", "-q", "origin/main"]);
        assert_eq!(
            refusal(&wt),
            "submodule vendor/lib has submodules of its own"
        );
    }

    #[test]
    fn submodule_repo_with_submodules_of_its_own_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "store-nested");
        git(&wt, &["submodule", "deinit", "-q", "vendor/lib"]);
        let store = r.join(".git/worktrees/store-nested/modules/vendor/lib");
        std::fs::create_dir_all(store.join("modules/inner")).unwrap();
        assert!(
            refusal(&wt).ends_with("has submodules of its own"),
            "{}",
            refusal(&wt)
        );
    }

    #[test]
    fn submodule_repo_without_a_remote_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "no-remote");
        git(&wt.join("vendor/lib"), &["remote", "remove", "origin"]);
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert!(
            refusal(&wt).contains("on no remote branch"),
            "{}",
            refusal(&wt)
        );
    }

    #[test]
    fn gitlink_folder_that_is_not_its_submodule_checkout_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        // Stray files where the checkout was.
        let stray = worktree_with_checkout(&r, "stray");
        git(&stray, &["submodule", "deinit", "-q", "vendor/lib"]);
        std::fs::write(stray.join("vendor/lib/notes.txt"), "mine").unwrap();
        // A repository of its own, with a .git folder rather than a .git file.
        let own = worktree_with_checkout(&r, "own-repo");
        git(&own, &["submodule", "deinit", "-q", "vendor/lib"]);
        git(&own.join("vendor/lib"), &["init", "-q"]);
        for wt in [&stray, &own] {
            assert_eq!(
                refusal(wt),
                "submodule vendor/lib is not empty and is not a submodule checkout"
            );
        }
    }

    #[test]
    fn gitlink_folder_borrowing_another_submodules_repo_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let lib = fx.dir.path().join("lib");
        git(
            &r,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/other",
            ],
        );
        git(&r, &["commit", "-qm", "second submodule"]);
        let wt = worktree_with_checkout(&r, "borrowed");
        // vendor/other now points at vendor/lib's repository, whose
        // core.worktree is vendor/lib, so status there never sees mine.txt.
        let borrowed = std::fs::read(wt.join("vendor/lib/.git")).unwrap();
        std::fs::write(wt.join("vendor/other/.git"), borrowed).unwrap();
        std::fs::write(wt.join("vendor/other/mine.txt"), "mine").unwrap();
        assert_eq!(
            refusal(&wt),
            "submodule vendor/other is not empty and is not a submodule checkout"
        );
    }

    #[test]
    fn untracked_files_hidden_by_config_still_keep_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        // Written to the shared repo config, so it applies to every worktree.
        git(&r, &["config", "status.showUntrackedFiles", "no"]);
        let top = worktree(&r, "hidden-top");
        std::fs::write(top.join("notes.txt"), "mine").unwrap();
        let sub = worktree_with_checkout(&r, "hidden-sub");
        git(
            &sub.join("vendor/lib"),
            &["config", "status.showUntrackedFiles", "no"],
        );
        std::fs::write(sub.join("vendor/lib/notes.txt"), "mine").unwrap();
        // Same repo config, nothing untracked: this one alone is a candidate.
        let control = worktree(&r, "hidden-control");
        age(&fx, &top, 30);
        age(&fx, &sub, 30);
        age(&fx, &control, 30);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["hidden-control"]);
        assert_eq!(
            refusal(&sub),
            "submodule vendor/lib has uncommitted changes"
        );
    }

    #[test]
    fn submodule_commit_only_in_the_reflog_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "sub-reflog");
        let sub = wt.join("vendor/lib");
        let recorded = git(&sub, &["rev-parse", "HEAD"]);
        // Work on the detached HEAD, then return to the recorded commit as
        // `submodule update` would. Only the reflog still reaches the work.
        commit(&sub, "detached-work.txt");
        git(&sub, &["checkout", "-q", recorded.trim()]);
        age(&fx, &wt, 30);
        assert!(task(14, vec![]).scan(&fx.ctx()).unwrap().is_empty());
        assert!(
            refusal(&wt).contains("on no remote branch"),
            "{}",
            refusal(&wt)
        );
    }

    #[test]
    fn run_refuses_a_worktree_written_to_after_the_run_started() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        std::fs::write(r.join(".gitignore"), "*.log\n").unwrap();
        git(&r, &["add", ".gitignore"]);
        git(&r, &["commit", "-qm", "ignore logs"]);
        let wt = worktree(&r, "late-ignored");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        // Ignored, so status stays clean. Only the mtime check sees it.
        std::fs::write(wt.join("build.log"), "fresh").unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "changed since the run started");
        assert!(wt.exists());
    }

    #[test]
    fn run_refuses_a_deep_write_after_the_run_started() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        std::fs::write(r.join(".gitignore"), "*.log\n").unwrap();
        git(&r, &["add", ".gitignore"]);
        git(&r, &["commit", "-qm", "ignore logs"]);
        let wt = worktree(&r, "late-deep");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        // Deeper than the scan's mtime walk reaches.
        let deep = wt.join("a/b/c/d/e/f/g/h");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("trace.log"), "fresh").unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "changed since the run started");
    }

    #[test]
    fn run_refuses_a_write_in_a_submodule_repo_after_the_run_started() {
        let fx = Fixture::new(&[]);
        let r = repo_with_submodule(&fx);
        let wt = worktree_with_checkout(&r, "late-store");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        // Outside the worktree, in the repository `--force` would delete.
        let store = r.join(".git/worktrees/late-store/modules/vendor/lib");
        std::fs::write(store.join("description"), "touched").unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "changed since the run started");
        assert!(wt.exists());
    }

    /// Ignores `.claude/` in every worktree of `repo`, as Claude Code does.
    fn ignore_claude_folder(repo: &Path) {
        std::fs::write(repo.join(".git/info/exclude"), ".claude/\n").unwrap();
    }

    #[test]
    fn worktree_holding_a_claude_worktree_is_kept() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        ignore_claude_folder(&r);
        let outer = worktree(&r, "outer");
        let inner = outer.join(".claude/worktrees/inner");
        git(
            &outer,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "inner",
                inner.to_str().unwrap(),
            ],
        );
        std::fs::write(inner.join("work.txt"), "untracked work").unwrap();
        age(&fx, &outer, 30);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(!names(&found).contains(&"outer".to_string()), "{found:?}");
        let why = refusal(&outer);
        assert!(why.starts_with("holds another git checkout at"), "{why}");
        assert!(why.ends_with(".claude/worktrees/inner"), "{why}");
    }

    #[test]
    fn nested_checkout_named_like_build_output_is_still_found() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        ignore_claude_folder(&r);
        let outer = worktree(&r, "outer-target");
        let inner = outer.join(".claude/worktrees/target");
        git(
            &outer,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "target",
                inner.to_str().unwrap(),
            ],
        );
        let why = refusal(&outer);
        assert!(why.ends_with(".claude/worktrees/target"), "{why}");
    }

    #[test]
    fn agent_config_written_lately_does_not_keep_a_worktree_busy() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        ignore_claude_folder(&r);
        let wt = worktree(&r, "synced");
        age(&fx, &wt, 1);
        // An Automatic sync, moments ago. Ignored by git, so status is clean.
        std::fs::create_dir_all(wt.join(".claude/rules")).unwrap();
        std::fs::write(wt.join(".claude/rules/general.md"), "rules").unwrap();
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["synced"]);
    }

    #[test]
    fn agent_config_written_during_the_run_still_blocks_removal() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        ignore_claude_folder(&r);
        let wt = worktree(&r, "late-sync");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        // The last check before removal counts every file, agent config too.
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        std::fs::write(wt.join(".claude/settings.local.json"), "{}").unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "changed since the run started");
        assert!(wt.exists());
    }

    #[test]
    fn formats_idle() {
        assert_eq!(format_idle(Duration::from_secs(45 * 60)), "45 min");
        assert_eq!(format_idle(Duration::from_secs(5 * 3600)), "5 h");
        assert_eq!(format_idle(Duration::from_secs(3 * 86_400)), "3 days");
    }
}
