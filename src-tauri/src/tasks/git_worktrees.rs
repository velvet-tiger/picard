//! Removes linked git worktrees whose work is safely elsewhere.
//!
//! A worktree is removed only when every one of these holds:
//! - it is a linked worktree (not the main one), not locked, not prunable
//! - it sits inside a root on this volume and is not excluded (`check_target`)
//! - no running process is working inside it (`lsof`)
//! - it points back at the repo that lists it, and its HEAD is the one that
//!   repo lists. A copied repo, made with `cp -R` or Finder's Duplicate, still
//!   lists the original's worktrees, with the HEAD each had when copied.
//! - no rebase, `git am`, merge, cherry-pick, revert, bisect or notes merge
//!   is paused in it
//! - each of its own refs, under `refs/worktree/` and `refs/bisect/`, points
//!   at a commit that another local branch holds. Removal deletes them.
//! - `git status --porcelain` is empty, so nothing uncommitted or untracked
//! - any checked-out submodule is clean and holds no commit missing from its
//!   remote branches (`GitCmd::unsafe_to_inspect`)
//! - it is done and the folder has been idle for `min_idle`, or it waits and
//!   the folder has been idle for `stale_days`. It waits when only its own
//!   branch's upstream holds its commits, or when it is done but made
//!   commits that are now on no branch.
//!
//! A worktree is done when another local branch holds its HEAD. Any branch in
//! `refs/heads` counts except the worktree's own, even one checked out in
//! another linked worktree. A symbolic ref there is only another name for a
//! ref, so it never counts. Neither do remote-tracking branches. No default
//! branch is needed, so a repo whose only branch is `trunk` is checked too.
//!
//! A worktree is also done when no other branch holds HEAD, but other local
//! branches hold a cherry-picked copy of each commit that no other branch
//! holds. The copies can be spread over several branches. Git's patch ids
//! decide what is a copy, as they do when `git rebase` drops commits that are
//! already upstream. They have limits:
//! - Only the change is compared. Commit messages, authors and dates are not.
//! - Whitespace is ignored everywhere, including indentation and string
//!   literals. An empty commit matches any other empty commit.
//! - A binary file that `.gitattributes` marks as text is compared as text.
//! - A merge commit has no patch id. A merge among the commits fails this
//!   rule, because it could hold a conflict resolution that no copy has.
//! - One copy matches every commit that makes the same change, such as a
//!   change made, reverted and made again. Two such commits fail this rule.
//! - A copy still counts after its branch reverts it. A commit that
//!   reapplies a reverted change also counts as a copy of the original.
//!
//! A failed match only fails this rule. A pushed branch still waits for
//! `stale_days`. An error here, such as a blob missing from a partial clone,
//! counts as no match, and the pushed check still runs.
//!
//! A done worktree can still hold commits that no branch does. A detached
//! worktree can make a commit and then check out another one, after which
//! only its HEAD reflog reaches that commit. Removal deletes that reflog. So
//! a done worktree whose reflog shows a commit made in it, which no other
//! local branch holds and which has no copy on one, waits for `stale_days`
//! instead. Commits it only checked out or reset to do not count. Neither
//! do drafts with a copy on a branch, such as commits from before a rebase.
//! The reflog message decides: `ONLY_MOVED` lists the plain movements, and
//! any other message, even an unknown one, counts as a commit made here.
//! With no reflog there is nothing to read.
//!
//! A branch holding the commits shows they survive, not that the work was
//! finished. A branch stacked on top of the worktree's branch counts, and so
//! does a branch that merged it in only to sync. The worktree's own branch is
//! kept, so no commit is lost. A detached worktree has no branch. Once it is
//! removed, its own commits are unreachable, and the next `git gc` can delete
//! them. Picard logs the HEAD of each detached worktree it removes, so `git
//! fsck --lost-found` can find it before then. Ignored files in the worktree
//! are lost either way.
//!
//! The candidate reason says why a worktree is done. When a merge commit, or a
//! `merge` entry in another branch's reflog, records the merge, the reason
//! names it, as in "merged into dev by abc1234" or "merged into dev
//! (reflog)". Otherwise it names the branch, as in "on main", or says
//! "cherry-picked to another branch". Only the wording depends on this. A
//! worktree that waited says "idle 20 days" when it is only pushed, and "has
//! commits on no branch, idle 20 days" when the reflog made it wait.
//!
//! Each candidate is judged again just before removal, with the repo's
//! worktrees and branches read afresh. One found done must still be done.
//! Its HEAD is read once more right before `git worktree remove`, and a HEAD
//! that moved keeps it.
//!
//! Removal is `git worktree remove` without `--force`, so git refuses anything
//! dirty or locked as a second guard. The exception is a worktree with
//! checked-out submodules, which git removes only with `--force`. One `--force`
//! still refuses a locked worktree. Branches are never deleted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
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
    /// It is done: another local branch holds its HEAD, or holds a
    /// cherry-picked copy of each commit that no other branch holds. It can
    /// go once idle for `min_idle`. Holds the start of the candidate reason,
    /// such as "on main".
    Done(String),
    /// Its commits survive the removal, but it must wait for `stale_days`.
    Elsewhere(Wait),
    /// Removing it could lose commits.
    Unsafe,
}

/// Why a worktree whose commits survive must still wait for `stale_days`.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Wait {
    /// Only its upstream holds its commits.
    Pushed,
    /// It would be done, but its HEAD reflog shows commits made in it that
    /// no branch holds, with no copy on another branch.
    Unbranched,
}

impl Wait {
    /// How the candidate reason starts, before the idle time. A done
    /// worktree's reason starts with how it is done, such as "on main", so
    /// `waits_for_stale_days` can tell the kinds apart when a candidate is
    /// judged again.
    fn prefix(self) -> &'static str {
        match self {
            Wait::Pushed => "idle ",
            Wait::Unbranched => "has commits on no branch, idle ",
        }
    }
}

/// True if candidate reason `reason` is one for a worktree that waited for
/// `stale_days`, rather than one found done.
fn waits_for_stale_days(reason: &str) -> bool {
    [Wait::Pushed, Wait::Unbranched]
        .iter()
        .any(|w| reason.starts_with(w.prefix()))
}

/// Whether other local branches hold a copy of each commit that no other
/// branch holds.
#[derive(Debug, PartialEq)]
enum Copies {
    All,
    /// They do not. Holds why, for the debug log.
    Missing(String),
}

/// The candidate reason for a worktree whose commits were all cherry-picked.
const CHERRY_PICKED: &str = "cherry-picked to another branch";

/// HEAD reflog messages that show HEAD only moved to a commit that already
/// existed. The new commit of any other entry, including one with a message
/// not listed here, counts as made in the worktree. An unknown message can
/// therefore only make a worktree wait longer.
const ONLY_MOVED: &[&str] = &[
    "checkout:",
    "reset:",
    "rebase (start)",
    "rebase (finish)",
    "rebase (abort)",
    "rebase -i (start)",
    "rebase -i (finish)",
    "rebase -i (abort)",
];

/// Lists refs with whether each is symbolic. Ref names cannot hold a tab.
const REF_FORMAT: &str = "--format=%(refname)%09%(symref)";

/// What every worktree of one repo is judged against.
struct RepoState {
    /// The repo's git common dir, canonical. A worktree whose own common dir
    /// differs belongs to another repo.
    common_dir: PathBuf,
    /// Every local branch, as a full ref name such as `refs/heads/main`.
    /// Symbolic refs are left out.
    local: Vec<String>,
    /// The branch checked out in the main worktree, if any. It proves nothing
    /// about merges. It only goes first when a reason names a branch.
    main_checkout: Option<String>,
}

impl RepoState {
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

/// An error as one line, for a removal's failure reason.
fn err_text(e: anyhow::Error) -> String {
    format!("{e:#}")
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
        state: &RepoState,
        wt: &WorktreeEntry,
        ctx: &Ctx,
        in_use: &InUse,
    ) -> Result<Option<String>> {
        if !self.inspectable(state, wt, ctx, in_use)? {
            return Ok(None);
        }
        // `standing` logs why a worktree is unsafe.
        let standing = self.standing(repo, state, wt)?;
        Ok(self.reason(&standing, wt, ctx))
    }

    /// True if nothing about the worktree itself rules it out, before its
    /// commits are judged. Logs why it is ruled out.
    fn inspectable(
        &self,
        state: &RepoState,
        wt: &WorktreeEntry,
        ctx: &Ctx,
        in_use: &InUse,
    ) -> Result<bool> {
        if wt.locked || wt.prunable || wt.bare {
            return Ok(false);
        }
        if let Err(why) = check_target(&wt.path, ctx) {
            log::debug!("worktree {}: {why}", wt.path.display());
            return Ok(false);
        }
        if in_use.contains(&wt.path) {
            log::debug!(
                "worktree {}: in use by a running process",
                wt.path.display()
            );
            return Ok(false);
        }
        if let Some(why) = self.identity_problem(state, wt)? {
            log::warn!("worktree {}: skipped: {why}", wt.path.display());
            return Ok(false);
        }
        if let Some(op) = self.git.operation_in_progress(&wt.path)? {
            log::debug!("worktree {}: has {op} in progress", wt.path.display());
            return Ok(false);
        }
        if let Some(why) = self.git.unsafe_to_inspect(&wt.path)? {
            log::warn!("worktree {}: skipped: {why}", wt.path.display());
            return Ok(false);
        }
        if !self.git.output(&wt.path, SAFETY_STATUS)?.trim().is_empty() {
            log::debug!("worktree {}: has uncommitted changes", wt.path.display());
            return Ok(false);
        }
        Ok(true)
    }

    /// The candidate reason for a worktree with `standing`, or None while it
    /// has not been idle long enough.
    fn reason(&self, standing: &Standing, wt: &WorktreeEntry, ctx: &Ctx) -> Option<String> {
        let newest = newest_mtime(&wt.path, &ctx.not_activity)?;
        let idle = ctx.now.duration_since(newest).ok()?;
        let stale = Duration::from_secs(u64::from(self.stale_days) * SECS_PER_DAY);
        let wait = match standing {
            Standing::Done(how) if idle >= self.min_idle => {
                return Some(format!("{how}, idle {}", format_idle(idle)));
            }
            Standing::Done(_) => Wait::Pushed,
            Standing::Elsewhere(wait) => *wait,
            Standing::Unsafe => return None,
        };
        (idle >= stale).then(|| format!("{}{}", wait.prefix(), format_idle(idle)))
    }

    /// Why `wt` cannot be judged as a worktree of the repo `state` describes,
    /// if it cannot. A copied repo lists the original's worktrees, each with
    /// the HEAD it had when copied, and judging one by that HEAD could remove
    /// newer work. Only the repo the worktree points back to counts, and only
    /// the HEAD it has now.
    fn identity_problem(&self, state: &RepoState, wt: &WorktreeEntry) -> Result<Option<String>> {
        let raw = self.git.output(
            &wt.path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
                "--verify",
                "HEAD",
            ],
        )?;
        let mut lines = raw.lines();
        let (Some(common), Some(head), None) = (lines.next(), lines.next(), lines.next()) else {
            anyhow::bail!(
                "git rev-parse in {}: unexpected output {raw:?}",
                wt.path.display()
            );
        };
        let common = Path::new(common)
            .canonicalize()
            .with_context(|| format!("resolving git common dir {common}"))?;
        if common != state.common_dir {
            return Ok(Some(format!(
                "belongs to the repo at {}, not the one that lists it",
                common.display()
            )));
        }
        if head != wt.head {
            return Ok(Some(format!(
                "HEAD is {head}, but its repo lists {}",
                wt.head
            )));
        }
        Ok(None)
    }

    /// Decides where a worktree's commits are, and logs why one is unsafe or
    /// must wait. Reads only; runs no program a repo can configure.
    ///
    /// Rules 1 to 3 judge HEAD. Then two checks look at what the worktree's
    /// own git dir holds, which removal deletes. A per-worktree ref holding a
    /// commit no other branch holds makes it unsafe. A commit made here, by
    /// the HEAD reflog, that no branch holds or has a copy of makes a done
    /// worktree wait for `stale_days`. Neither check ever makes it go sooner.
    fn standing(&self, repo: &Path, state: &RepoState, wt: &WorktreeEntry) -> Result<Standing> {
        let standing = self.head_standing(repo, state, wt)?;
        let path = wt.path.display();
        if standing == Standing::Unsafe {
            log::debug!(
                "worktree {path}: kept: no other local branch holds its HEAD or a copy of each of its commits, and it has no fully pushed branch"
            );
            return Ok(standing);
        }
        if let Some(why) = self.worktree_ref_problem(repo, state, wt)? {
            log::debug!("worktree {path}: kept: {why}");
            return Ok(Standing::Unsafe);
        }
        if !matches!(standing, Standing::Done(_)) {
            return Ok(standing);
        }
        match self.unkept_commit(repo, state, wt)? {
            Some(id) => {
                log::debug!(
                    "worktree {path}: waits for stale_days: commit {id} was made here, and no other branch holds it or a copy"
                );
                Ok(Standing::Elsewhere(Wait::Unbranched))
            }
            None => Ok(standing),
        }
    }

    /// Rules 1 to 3: where the worktree's HEAD and its own commits are.
    fn head_standing(
        &self,
        repo: &Path,
        state: &RepoState,
        wt: &WorktreeEntry,
    ) -> Result<Standing> {
        let others = self.other_branches_containing(repo, state, wt)?;
        if let Some(first) = others.first() {
            return Ok(Standing::Done(self.held_wording(repo, wt, &others, first)));
        }
        if self.cherry_picked(repo, state, wt) {
            return Ok(Standing::Done(CHERRY_PICKED.to_string()));
        }
        let pushed = match &wt.branch {
            Some(branch) => self.fully_pushed(repo, branch)?,
            None => false,
        };
        Ok(if pushed {
            Standing::Elsewhere(Wait::Pushed)
        } else {
            Standing::Unsafe
        })
    }

    /// Why the worktree's own refs would lose a commit on removal, if they
    /// would. Refs under `refs/worktree/` and `refs/bisect/` live in the
    /// worktree's git dir, and `for-each-ref` run in the worktree lists only
    /// its own. Removal deletes them, so each must point at a commit that
    /// another local branch holds. A ref to anything but a commit counts as a
    /// loss too.
    fn worktree_ref_problem(
        &self,
        repo: &Path,
        state: &RepoState,
        wt: &WorktreeEntry,
    ) -> Result<Option<String>> {
        let raw = self.git.output(
            &wt.path,
            &[
                "for-each-ref",
                "--format=%(objectname)%09%(objecttype)%09%(refname)",
                "refs/worktree/",
                "refs/bisect/",
            ],
        )?;
        let mut refs = Vec::new();
        for line in raw.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(id), Some(kind), Some(name)) = (fields.next(), fields.next(), fields.next())
            else {
                anyhow::bail!("git for-each-ref printed {line:?}");
            };
            if kind != "commit" {
                return Ok(Some(format!("{name} points at a {kind}")));
            }
            refs.push((id, name));
        }
        if refs.is_empty() {
            return Ok(None);
        }
        let ids: Vec<&str> = refs.iter().map(|(id, _)| *id).collect();
        let unheld = self.unheld(repo, &ids, &state.others(wt.branch.as_deref()))?;
        Ok(refs
            .iter()
            .find(|(id, _)| unheld.contains(id))
            .map(|(id, name)| {
                format!("{name} holds commit {id}, which no other local branch holds")
            }))
    }

    /// The first commit made in the worktree, by its HEAD reflog, that no
    /// other local branch holds and that has no patch-equivalent copy on one.
    /// None when there is no such commit, or no reflog to read.
    ///
    /// A detached worktree can make a commit and then check out another one.
    /// The commit is then reachable only from the reflog, which removal
    /// deletes. Commits the worktree only checked out, or reset to, do not
    /// count: they existed already. A merge made here has no patch id, so
    /// unless a branch holds it, it has no copy. Two drafts with one patch id,
    /// such as a commit and its amended version, can share one copy here.
    fn unkept_commit(
        &self,
        repo: &Path,
        state: &RepoState,
        wt: &WorktreeEntry,
    ) -> Result<Option<String>> {
        let log = self
            .git
            .output(
                &wt.path,
                &[
                    "log",
                    "--walk-reflogs",
                    "--no-show-signature",
                    "--format=%H%x09%gs",
                    "HEAD",
                    "--",
                ],
            )
            .context("reading the worktree's HEAD reflog")?;
        let made = made_here(&log)?;
        if made.is_empty() {
            return Ok(None);
        }
        let others = state.others(wt.branch.as_deref());
        let unheld = self.unheld(repo, &made, &others)?;
        if unheld.is_empty() || others.is_empty() {
            return Ok(unheld.first().map(|id| id.to_string()));
        }
        let marked = self.copied_commits(repo, &unheld, &others)?;
        let copied = patch_equivalent(&marked);
        Ok(unheld
            .iter()
            .find(|id| !copied.contains(*id))
            .map(|id| id.to_string()))
    }

    /// Those of `ids` that none of `others` holds, in the same order.
    /// Revisions go through stdin.
    fn unheld<'a>(&self, repo: &Path, ids: &[&'a str], others: &[&str]) -> Result<Vec<&'a str>> {
        let mut input = String::new();
        for id in ids {
            input.push_str(&format!("{id}\n"));
        }
        for r in others {
            input.push_str(&format!("^{r}\n"));
        }
        let listed = self
            .git
            .output_with_input(repo, &["rev-list", "--stdin"], input)?;
        let reachable: HashSet<&str> = listed.lines().collect();
        Ok(ids
            .iter()
            .copied()
            .filter(|id| reachable.contains(id))
            .collect())
    }

    /// The start of the reason for a worktree whose HEAD `others` hold. A
    /// recorded merge only changes the wording, so a failure to read merge
    /// records falls back to naming `first`.
    fn held_wording(
        &self,
        repo: &Path,
        wt: &WorktreeEntry,
        others: &[String],
        first: &str,
    ) -> String {
        match self.merge_evidence(repo, &wt.head, others) {
            Ok(Some(how)) => how,
            Ok(None) => format!("on {first}"),
            Err(e) => {
                log::warn!(
                    "worktree {}: cannot read merge records, so the reason names the branch: {e:#}",
                    wt.path.display()
                );
                format!("on {first}")
            }
        }
    }

    /// Local branches that contain the worktree's HEAD, apart from its own.
    /// Symbolic refs are left out. The branch checked out in the main worktree
    /// comes first, then the rest by name. The order only decides which
    /// branch a reason names.
    fn other_branches_containing(
        &self,
        repo: &Path,
        state: &RepoState,
        wt: &WorktreeEntry,
    ) -> Result<Vec<String>> {
        let raw = self.git.output(
            repo,
            &[
                "for-each-ref",
                "--contains",
                &wt.head,
                REF_FORMAT,
                "refs/heads/",
            ],
        )?;
        let mut others: Vec<String> = branch_refs(&raw)?
            .into_iter()
            .filter_map(|r| r.strip_prefix("refs/heads/"))
            .filter(|b| wt.branch.as_deref() != Some(*b))
            .map(str::to_string)
            .collect();
        if let Some(main) = &state.main_checkout
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
    /// Refs go through stdin in full, so no number of branches overflows the
    /// command line, and a ref that no longer exists is an error rather than
    /// a path to limit the history to.
    fn merge_commit_of(
        &self,
        repo: &Path,
        head: &str,
        branches: &[String],
    ) -> Result<Option<String>> {
        let mut input = format!("^{head}\n");
        for b in branches {
            input.push_str(&format!("refs/heads/{b}\n"));
        }
        let raw = self.git.output_with_input(
            repo,
            &[
                "rev-list",
                "--merges",
                "--parents",
                "--ancestry-path",
                "--stdin",
            ],
            input,
        )?;
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
    /// costs one process per worktree. Refs go through stdin, as in
    /// `merge_commit_of`. A missing or expired reflog reads as empty.
    /// `--no-show-signature` stops repo config from making `log` run gpg.
    fn reflog_merged_into(
        &self,
        repo: &Path,
        head: &str,
        branches: &[String],
    ) -> Result<Option<String>> {
        let input: String = branches
            .iter()
            .map(|b| format!("refs/heads/{b}\n"))
            .collect();
        let raw = self.git.output_with_input(
            repo,
            &[
                "log",
                "--walk-reflogs",
                "--no-show-signature",
                "--format=%H%x09%gD%x09%gs",
                "--stdin",
            ],
            input,
        )?;
        let merged: Vec<&str> = raw
            .lines()
            .filter_map(|line| reflog_merge_branch(line, head))
            .collect();
        Ok(branches
            .iter()
            .find(|b| merged.contains(&b.as_str()))
            .cloned())
    }

    /// True if other local branches hold a copy of each commit that no other
    /// branch holds. This rule only adds a way to be done, so an error counts
    /// as no, and the pushed check still runs. Logs why not.
    fn cherry_picked(&self, repo: &Path, state: &RepoState, wt: &WorktreeEntry) -> bool {
        match self.copies(repo, state, wt) {
            Ok(Copies::All) => true,
            Ok(Copies::Missing(why)) => {
                log::debug!("worktree {}: not cherry-picked: {why}", wt.path.display());
                false
            }
            Err(e) => {
                log::warn!(
                    "worktree {}: cannot check for cherry-picked copies: {e:#}",
                    wt.path.display()
                );
                false
            }
        }
    }

    /// Whether other local branches hold a cherry-picked copy of each commit
    /// that no other branch holds. Run only when no other branch holds HEAD.
    ///
    /// It lists those commits, and fails on a merge among them. It fails when
    /// two of them make the same change, because git would match both to one
    /// copy. Then it asks git which of them have a patch-equivalent copy on
    /// another branch, and requires all of them. Computing patch ids runs no
    /// external diff or textconv that the repo names.
    fn copies(&self, repo: &Path, state: &RepoState, wt: &WorktreeEntry) -> Result<Copies> {
        let others = state.others(wt.branch.as_deref());
        if others.is_empty() {
            return Ok(Copies::Missing("there is no other local branch".into()));
        }
        let listed = self.unique_commits(repo, &wt.head, &others)?;
        let ids = match non_merge_commits(&listed) {
            Ok(ids) if ids.is_empty() => {
                return Ok(Copies::Missing(
                    "every commit it has is on another branch".into(),
                ));
            }
            Ok(ids) => ids,
            Err(merge) => return Ok(Copies::Missing(format!("commit {merge} is a merge"))),
        };
        if let Some(why) = self.repeated_change(repo, &ids)? {
            return Ok(Copies::Missing(why));
        }
        let marked = self.copied_commits(repo, &[wt.head.as_str()], &others)?;
        let copied = patch_equivalent(&marked);
        if let Some(id) = ids.iter().find(|id| !copied.contains(*id)) {
            return Ok(Copies::Missing(format!(
                "commit {id} has no copy on another branch"
            )));
        }
        if copied.len() != ids.len() {
            return Ok(Copies::Missing(
                "git marked commits it was not expected to list".into(),
            ));
        }
        Ok(Copies::All)
    }

    /// `git rev-list --parents` for the commits `head` holds and none of
    /// `others` do. Revisions go through stdin, so no number of branches can
    /// overflow the command line, and git never reads one as a path: a ref
    /// that no longer exists is an error.
    fn unique_commits(&self, repo: &Path, head: &str, others: &[&str]) -> Result<String> {
        let mut input = format!("{head}\n");
        for r in others {
            input.push_str(&format!("^{r}\n"));
        }
        self.git
            .output_with_input(repo, &["rev-list", "--parents", "--stdin"], input)
    }

    /// Why two of `ids` count as the same change, if any do. Git marks every
    /// commit whose patch id matches a copy, so one copy would cover a change
    /// made, reverted and made again, and the net change would be lost.
    /// `diff-tree` makes the patches, with no external diff or textconv, and
    /// `patch-id` hashes them as `--cherry-mark` does, ignoring whitespace.
    fn repeated_change(&self, repo: &Path, ids: &[&str]) -> Result<Option<String>> {
        let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
        let patches = self.git.output_with_input(
            repo,
            &[
                "diff-tree",
                "--stdin",
                "-p",
                "--root",
                "--no-ext-diff",
                "--no-textconv",
            ],
            input,
        )?;
        // Moved, not copied: a large diff is held once.
        let hashed = self
            .git
            .output_with_input(repo, &["patch-id", "--stable"], patches)?;
        Ok(repeated_patch(ids, &hashed))
    }

    /// `git rev-list --cherry-mark` output for the commits on the left of
    /// `head...branch`, for each of `heads` and each of `others`. A commit
    /// that some branch holds drops out of the left side, so the left side is
    /// the commits that `heads` reach and no other branch holds. The right
    /// side is what the branches hold and `heads` do not reach. Ref names
    /// cannot contain `..`, so each line is one range. The input grows with
    /// heads times branches.
    fn copied_commits(&self, repo: &Path, heads: &[&str], others: &[&str]) -> Result<String> {
        let mut input = String::new();
        for head in heads {
            for r in others {
                input.push_str(&format!("{head}...{r}\n"));
            }
        }
        self.git.output_with_input(
            repo,
            &[
                "rev-list",
                "--left-only",
                "--cherry-mark",
                "--no-merges",
                "--stdin",
            ],
            input,
        )
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
                "--",
            ],
        )?;
        Ok(ahead.trim() == "0")
    }

    /// Every local branch of `repo`, as full ref names. Symbolic refs are
    /// left out.
    fn local_branches(&self, repo: &Path) -> Result<Vec<String>> {
        let raw = self
            .git
            .output(repo, &["for-each-ref", REF_FORMAT, "refs/heads/"])?;
        Ok(branch_refs(&raw)?.into_iter().map(str::to_string).collect())
    }

    /// The canonical git common dir of the repo or worktree at `dir`.
    fn common_dir(&self, dir: &Path) -> Result<PathBuf> {
        let raw = self.git.output(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let path = Path::new(raw.strip_suffix('\n').unwrap_or(&raw));
        path.canonicalize()
            .with_context(|| format!("resolving git common dir {}", path.display()))
    }

    /// The repo's worktrees. The first entry is always the main worktree.
    fn worktrees(&self, repo: &Path) -> Result<Vec<WorktreeEntry>> {
        let raw = self
            .git
            .output(repo, &["worktree", "list", "--porcelain", "-z"])?;
        Ok(parse_worktree_list(&raw))
    }

    fn repo_state(&self, repo: &Path, worktrees: &[WorktreeEntry]) -> Result<RepoState> {
        Ok(RepoState {
            common_dir: self.common_dir(repo)?,
            local: self.local_branches(repo)?,
            main_checkout: worktrees.first().and_then(|w| w.branch.clone()),
        })
    }

    fn scan_repo(&self, repo: &Path, ctx: &Ctx, in_use: &InUse) -> Result<Vec<(PathBuf, String)>> {
        let worktrees = self.worktrees(repo)?;
        let state = self.repo_state(repo, &worktrees)?;
        let mut found = Vec::new();
        for wt in worktrees.iter().skip(1) {
            if ctx.stopped() {
                break;
            }
            match self.assess(repo, &state, wt, ctx, in_use) {
                Ok(Some(reason)) => found.push((wt.path.clone(), reason)),
                Ok(None) => {}
                Err(e) => log::warn!("worktree {}: {e:#}", wt.path.display()),
            }
        }
        Ok(found)
    }

    /// The main repo a worktree belongs to, from its common git dir.
    fn repo_of(&self, worktree: &Path) -> Result<PathBuf> {
        let common = self.common_dir(worktree)?;
        common
            .parent()
            .map(Path::to_path_buf)
            .with_context(|| format!("git common dir {} has no parent", common.display()))
    }

    /// Judges candidate `c` again, with the repo's worktrees and branches read
    /// afresh. Returns its entry if it is still a candidate of the same kind.
    /// One found done must still be done. One found stale must still have its
    /// commits somewhere. `canon` is the candidate's canonical path. Locks are
    /// left to `git worktree remove`, which refuses a locked worktree.
    fn recheck(&self, repo: &Path, c: &Candidate, canon: &Path) -> Result<WorktreeEntry, String> {
        let worktrees = self.worktrees(repo).map_err(err_text)?;
        let Some(wt) = worktrees
            .iter()
            .skip(1)
            .find(|w| w.path.canonicalize().is_ok_and(|p| p == canon))
        else {
            return Err("is no longer a linked worktree of its repo".to_string());
        };
        let state = self.repo_state(repo, &worktrees).map_err(err_text)?;
        if let Some(why) = self.identity_problem(&state, wt).map_err(err_text)? {
            return Err(why);
        }
        if let Some(op) = self.git.operation_in_progress(&wt.path).map_err(err_text)? {
            return Err(format!("has {op} in progress"));
        }
        match self.standing(repo, &state, wt).map_err(err_text)? {
            Standing::Done(_) => Ok(wt.clone()),
            Standing::Elsewhere(_) if waits_for_stale_days(&c.reason) => Ok(wt.clone()),
            Standing::Elsewhere(_) => {
                Err("is no longer done, so it waits for stale_days".to_string())
            }
            Standing::Unsafe => Err("removing it could now lose commits".to_string()),
        }
    }

    /// True if a file under the worktree was written after the run started.
    /// The checks before removal take seconds on a large repo, and `--force`
    /// skips git's own. A file written since the run started, even an ignored
    /// one that status does not list, means someone is working here. A commit
    /// in a submodule writes only to its repository, so those are walked too
    /// when `force` is about to delete them.
    fn written_during_run(&self, c: &Candidate, ctx: &Ctx, force: bool) -> Result<bool, String> {
        let mut walked = vec![(c.path.clone(), true)];
        if force {
            let modules = self.git.submodule_store_root(&c.path).map_err(err_text)?;
            walked.push((modules, false));
        }
        Ok(walked
            .iter()
            .any(|(dir, skip_build)| written_since(dir, ctx.now, *skip_build)))
    }

    /// Refuses removal if the worktree's HEAD is no longer the commit
    /// `recheck` judged. Run last before `git worktree remove`, because the
    /// checks before it take time and HEAD can move meanwhile.
    fn head_unmoved(&self, wt: &WorktreeEntry) -> Result<(), String> {
        let now = self
            .git
            .output(&wt.path, &["rev-parse", "--verify", "HEAD"])
            .map_err(err_text)?;
        if now.trim_end() != wt.head {
            return Err("HEAD moved since it was checked".to_string());
        }
        Ok(())
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
        let force = self.git.has_submodule_stores(&c.path).map_err(err_text)?;
        if let Some(why) = self.git.unsafe_to_inspect(&c.path).map_err(err_text)? {
            return Err(why);
        }
        let dirty = self.git.output(&c.path, SAFETY_STATUS).map_err(err_text)?;
        if !dirty.trim().is_empty() {
            return Err("now has uncommitted changes".to_string());
        }
        let repo = self.repo_of(&c.path).map_err(err_text)?;
        let wt = self.recheck(&repo, c, &canon)?;
        if self.written_during_run(c, ctx, force)? {
            return Err("changed since the run started".to_string());
        }
        self.head_unmoved(&wt)?;
        let path = c.path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        self.git.output(&repo, &args).map_err(err_text)?;
        if wt.branch.is_none() {
            // No branch holds a detached worktree's own commits. Logging HEAD
            // lets someone find it with `git fsck --lost-found` before gc.
            log::info!(
                "worktree {} had a detached HEAD at {}",
                c.path.display(),
                wt.head
            );
        }
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

/// The commit ids in `git rev-list --parents` output, newest first, or the
/// id of a merge among them. Each line is a commit id followed by its
/// parents' ids.
fn non_merge_commits(raw: &str) -> Result<Vec<&str>, &str> {
    let mut ids = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let Some(id) = fields.next() else { continue };
        if fields.count() > 1 {
            return Err(id);
        }
        ids.push(id);
    }
    Ok(ids)
}

/// The commits a HEAD reflog shows were made in the worktree, newest first,
/// each once. Lines are `<commit>\t<message>`, from `log --walk-reflogs
/// --format=%H%x09%gs`. Each entry's commit is the one HEAD moved to.
fn made_here(raw: &str) -> Result<Vec<&str>> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for line in raw.lines() {
        let Some((id, message)) = line.split_once('\t') else {
            anyhow::bail!("git log --walk-reflogs printed {line:?}");
        };
        if !only_moved(message) && seen.insert(id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// True if a HEAD reflog message shows HEAD only moved to a commit that
/// already existed. `git worktree add` writes an empty message. A merge or
/// pull that fast-forwards made no commit either.
fn only_moved(message: &str) -> bool {
    message.is_empty()
        || ONLY_MOVED.iter().any(|m| message.starts_with(m))
        || ((message.starts_with("merge ") || message.starts_with("pull"))
            && message.ends_with(": Fast-forward"))
}

/// The commits that `git rev-list --cherry-mark` marks `=`, because the other
/// side holds a patch-equivalent copy. Each line is the mark and a commit id.
/// Any other line is skipped, so output this does not expect can only make a
/// match fail.
fn patch_equivalent(raw: &str) -> HashSet<&str> {
    raw.lines().filter_map(|l| l.strip_prefix('=')).collect()
}

/// Why two of `ids` share a patch id in `git patch-id` output, if any do.
/// Each line is a patch id and a commit id. A commit with no line has no
/// diff, so two such commits make the same, empty, change. Output this does
/// not expect is a reason too, so it can only make a match fail.
fn repeated_patch(ids: &[&str], raw: &str) -> Option<String> {
    let given: HashSet<&str> = ids.iter().copied().collect();
    let mut by_patch: HashMap<&str, &str> = HashMap::new();
    let mut patched: HashSet<&str> = HashSet::new();
    for line in raw.lines() {
        let Some((patch, commit)) = line.split_once(' ') else {
            return Some(format!("git patch-id printed {line:?}"));
        };
        if !given.contains(commit) {
            return Some(format!(
                "git patch-id printed a commit it was not given: {commit}"
            ));
        }
        if let Some(first) = by_patch.insert(patch, commit) {
            return Some(format!("commits {first} and {commit} make the same change"));
        }
        patched.insert(commit);
    }
    let mut empty = ids.iter().filter(|id| !patched.contains(*id));
    match (empty.next(), empty.next()) {
        (Some(a), Some(b)) => Some(format!("commits {a} and {b} both change nothing")),
        _ => None,
    }
}

/// The full ref names in `for-each-ref` output made with `REF_FORMAT`,
/// leaving out symbolic refs. A symbolic ref under `refs/heads` is another
/// name for some ref, such as the worktree's own branch, a remote-tracking
/// branch or a worktree's HEAD, so it holds nothing of its own.
fn branch_refs(raw: &str) -> Result<Vec<&str>> {
    let mut refs = Vec::new();
    for line in raw.lines() {
        match line.split_once('\t') {
            Some((name, "")) => refs.push(name),
            Some(_) => {}
            None => return Err(anyhow!("git for-each-ref printed {line:?}")),
        }
    }
    Ok(refs)
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

    /// Git for fixture setup, isolated from the user's global config so
    /// commit signing or hooks cannot interfere.
    fn git_command(dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
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
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd
    }

    /// Runs git for fixture setup and returns whether it succeeded, for a
    /// step that is meant to stop part way, such as a conflicted merge.
    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        git_command(dir, args).output().unwrap().status.success()
    }

    /// Runs git for fixture setup and returns its stdout. Fails the test if
    /// git fails.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_command(dir, args).output().unwrap();
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
    fn detached_head_only_when_held_by_a_local_branch() {
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
        commit_only(repo, &format!("{branch}-work"));
        for id in ids {
            git(repo, &["cherry-pick", id]);
        }
        git(repo, &["checkout", "-q", "main"]);
    }

    /// Commits a new `file` alone. In the main worktree, `git add .` would add
    /// the linked worktrees under `.claude/worktrees` as embedded repositories.
    fn commit_only(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", "--", file]);
        git(dir, &["commit", "-qm", file]);
    }

    /// Adds a bare `origin` outside the roots to `repo`.
    fn add_origin(fx: &Fixture, repo: &Path) {
        let remote = fx.dir.path().join("remote.git");
        git(
            fx.dir.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
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
            Ok(vec![a.as_str(), b.as_str(), c.as_str()])
        );
        let with_merge = format!("{a} {b}\n{b} {c} {d}\n");
        assert_eq!(non_merge_commits(&with_merge), Err(b.as_str()));
        assert_eq!(non_merge_commits(""), Ok(vec![]));
    }

    #[test]
    fn reads_patch_equivalent_commits_from_cherry_mark() {
        let [a, b, c] = ["a", "b", "c"].map(|x| x.repeat(40));
        // A `commit` header, as `--format` would add, is not a mark.
        let raw = format!("={a}\n+{b}\ncommit ={c}\n");
        assert_eq!(patch_equivalent(&raw), HashSet::from([a.as_str()]));
    }

    #[test]
    fn finds_commits_that_make_the_same_change() {
        let [a, b, c, p, q] = ["a", "b", "c", "p", "q"].map(|x| x.repeat(40));
        let ids = [a.as_str(), b.as_str(), c.as_str()];
        let distinct = format!("{p} {a}\n{q} {b}\n");
        assert_eq!(repeated_patch(&ids, &distinct), None, "one empty commit");
        let same = format!("{p} {a}\n{p} {c}\n");
        let why = repeated_patch(&ids, &same).unwrap();
        assert!(why.contains("make the same change"), "{why}");
        let two_empty = format!("{p} {a}\n");
        let why = repeated_patch(&ids, &two_empty).unwrap();
        assert!(why.contains("both change nothing"), "{why}");
        // Output it does not expect can only fail the match.
        assert!(repeated_patch(&ids, "garbage\n").is_some());
        assert!(repeated_patch(&ids, &format!("{p} {q}\n")).is_some());
    }

    #[test]
    fn leaves_symbolic_refs_out_of_the_branches() {
        let raw = "refs/heads/main\t\nrefs/heads/alias\trefs/heads/main\nrefs/heads/x/y\t\n";
        assert_eq!(
            branch_refs(raw).unwrap(),
            vec!["refs/heads/main", "refs/heads/x/y"]
        );
        assert!(branch_refs("refs/heads/no-tab\n").is_err());
    }

    /// Writes a sequence editor that turns the first `pick` into `edit`, so
    /// `rebase -i` stops after its first commit with a clean tree.
    fn edit_first(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("edit-first.sh");
        std::fs::write(&script, "#!/bin/sh\nsed -i.bak '1s/^pick/edit/' \"$1\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Starts `rebase -i main` in `wt`, paused at its first commit.
    fn pause_rebase(fx: &Fixture, wt: &Path) {
        let editor = format!("sequence.editor={}", edit_first(fx.dir.path()).display());
        git(wt, &["-c", &editor, "rebase", "-i", "main"]);
    }

    /// Stopped onto a moved main, HEAD is a reworded copy of the first commit.
    /// The worktree's own branch, not checked out during the rebase, holds
    /// the original, so the copy would match.
    #[test]
    fn paused_rebase_onto_a_moved_base_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "rebasing");
        commit(&wt, "one");
        commit(&wt, "two");
        commit_only(&r, "main-moves");
        pause_rebase(&fx, &wt);
        git(&wt, &["commit", "--amend", "-qm", "one, reworded"]);
        age(&fx, &wt, 1);
        assert_eq!(
            GitCmd::default().operation_in_progress(&wt).unwrap(),
            Some("a rebase")
        );
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// Stopped without moving the base, HEAD is the first commit itself, which
    /// the worktree's own branch holds.
    #[test]
    fn paused_rebase_onto_the_same_base_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "rebasing");
        commit(&wt, "one");
        commit(&wt, "two");
        pause_rebase(&fx, &wt);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn bisect_in_progress_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "bisecting");
        for i in 0..6 {
            commit(&wt, &format!("f{i}"));
        }
        git(&wt, &["bisect", "start", "bisecting", "main"]);
        age(&fx, &wt, 1);
        assert_eq!(
            GitCmd::default().operation_in_progress(&wt).unwrap(),
            Some("a bisect")
        );
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn run_refuses_a_worktree_whose_bisect_started_after_the_scan() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "late-bisect");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["late-bisect"]);
        git(&wt, &["bisect", "start"]);
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "has a bisect in progress");
        assert!(wt.exists());
    }

    /// `cp -R` copies the repo's record of its worktree, which still points at
    /// the original folder, with the HEAD it had then. New work there must
    /// not be judged by that HEAD.
    #[test]
    fn copied_repo_does_not_judge_the_original_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "agent-real");
        let copy = fx.root.join("repo-copy");
        let copied = Command::new("cp")
            .arg("-R")
            .arg(&r)
            .arg(&copy)
            .status()
            .unwrap();
        assert!(copied.success());
        commit(&wt, "precious");
        assert!(!on_a_branch(&r, &head_of(&wt)));
        age(&fx, &wt, 1);

        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert!(found.is_empty(), "{found:?}");
        let listed = t.worktrees(&copy).unwrap();
        let state = t.repo_state(&copy, &listed).unwrap();
        let why = t.identity_problem(&state, &listed[1]).unwrap().unwrap();
        assert!(why.starts_with("belongs to the repo at "), "{why}");

        // Through its own repo, a stale HEAD is refused too.
        let listed = t.worktrees(&r).unwrap();
        let state = t.repo_state(&r, &listed).unwrap();
        let mut stale = listed[1].clone();
        stale.head = head_of(&r);
        let why = t.identity_problem(&state, &stale).unwrap().unwrap();
        assert!(why.starts_with("HEAD is "), "{why}");

        // Removal judges the worktree again, through the repo it points at.
        let offered = Candidate {
            path: wt.clone(),
            bytes: 0,
            reason: "on main, idle 24 h".to_string(),
            estimate: Estimate::Exact,
        };
        let report = t.run(&ctx, &[offered]);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "removing it could now lose commits");
        assert!(wt.exists());
    }

    /// The worktree adds a file, reverts it and adds it again. Another branch
    /// has copies of the add and the revert only, so the file is not there.
    /// Git matches the second add to the first add's copy.
    #[test]
    fn a_change_made_again_after_a_revert_needs_a_copy_of_its_own() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "reapplied");
        commit(&wt, "x");
        let add = head_of(&wt);
        git(&wt, &["revert", "--no-edit", "HEAD"]);
        let revert = head_of(&wt);
        git(&wt, &["revert", "--no-edit", "HEAD"]);
        git(&r, &["branch", "other", "main"]);
        pick_onto(&r, "other", &[&add, &revert]);
        age(&fx, &wt, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// A symbolic ref under `refs/heads` can name the worktree's own branch or
    /// its HEAD. Either holds its HEAD only by pointing at it.
    #[test]
    fn symbolic_ref_to_the_worktrees_own_branch_or_head_does_not_count() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let own = worktree(&r, "feat");
        commit(&own, "wip");
        git(
            &r,
            &["symbolic-ref", "refs/heads/feat-alias", "refs/heads/feat"],
        );
        let agent = detached(&r, "agent-sym");
        commit(&agent, "agent-work");
        git(
            &r,
            &[
                "symbolic-ref",
                "refs/heads/mirror",
                "worktrees/agent-sym/HEAD",
            ],
        );
        age(&fx, &own, 60);
        age(&fx, &agent, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// A symbolic ref under `refs/heads` that names a remote-tracking branch
    /// counts no more than that branch does, for HEAD or for a copy.
    #[test]
    fn symbolic_ref_to_a_remote_tracking_branch_does_not_count() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        add_origin(&fx, &r);
        // HEAD itself pushed, and aliased.
        let held = detached(&r, "agent-held");
        commit(&held, "held-work");
        git(&held, &["push", "-q", "origin", "HEAD:refs/heads/held"]);
        git(
            &r,
            &[
                "symbolic-ref",
                "refs/heads/held-alias",
                "refs/remotes/origin/held",
            ],
        );
        // A copy pushed from a branch since deleted, and aliased.
        let copied = detached(&r, "agent-copied");
        commit(&copied, "copied-work");
        let id = head_of(&copied);
        git(&r, &["branch", "pick", "main"]);
        pick_onto(&r, "pick", &[&id]);
        git(&r, &["push", "-q", "origin", "pick"]);
        git(&r, &["branch", "-q", "-D", "pick"]);
        git(
            &r,
            &[
                "symbolic-ref",
                "refs/heads/pick-alias",
                "refs/remotes/origin/pick",
            ],
        );
        age(&fx, &held, 60);
        age(&fx, &copied, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn copy_held_only_by_a_remote_tracking_branch_does_not_count() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        add_origin(&fx, &r);
        let wt = detached(&r, "remote-copy");
        commit(&wt, "work");
        let id = head_of(&wt);
        git(&r, &["branch", "pick", "main"]);
        pick_onto(&r, "pick", &[&id]);
        git(&r, &["push", "-q", "origin", "pick"]);
        git(&r, &["branch", "-q", "-D", "pick"]);
        age(&fx, &wt, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// A worktree on `feat` with commits one and two, pushed to `origin`.
    fn pushed_two_commits(fx: &Fixture, r: &Path) -> (PathBuf, String, String) {
        add_origin(fx, r);
        let wt = worktree(r, "feat");
        commit(&wt, "one");
        let one = head_of(&wt);
        commit(&wt, "two");
        let two = head_of(&wt);
        git(&wt, &["push", "-q", "-u", "origin", "feat"]);
        (wt, one, two)
    }

    #[test]
    fn pushed_branch_with_only_some_commits_copied_waits_for_stale_days() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let (wt, one, _) = pushed_two_commits(&fx, &r);
        pick_onto(&r, "main", &[&one]);
        age(&fx, &wt, 5);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "not stale yet: {found:?}");
        age(&fx, &wt, 20);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "feat"), "idle 20 days");
    }

    #[test]
    fn pushed_branch_with_every_commit_copied_goes_after_min_idle() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let (wt, one, two) = pushed_two_commits(&fx, &r);
        pick_onto(&r, "main", &[&one, &two]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "feat"),
            "cherry-picked to another branch, idle 24 h"
        );
    }

    /// Points `child`'s first parent at `parent` in a commit-graph file, with
    /// topological level `level`, as a forged graph would. Git does not check
    /// the file's checksum when it reads it.
    fn forge_first_parent(graph: &Path, child: &str, parent: &str, level: u64) {
        use std::os::unix::fs::PermissionsExt;
        let mut data = std::fs::read(graph).unwrap();
        assert_eq!(&data[..4], b"CGPH");
        let word = |at: usize| u64::from_be_bytes(data[at..at + 8].try_into().unwrap());
        let chunk = |id: &[u8]| {
            let at = (0..usize::from(data[6]))
                .map(|i| 8 + 12 * i)
                .find(|&at| &data[at..at + 4] == id)
                .unwrap();
            usize::try_from(word(at + 4)).unwrap()
        };
        let (fanout, oids, commits) = (chunk(b"OIDF"), chunk(b"OIDL"), chunk(b"CDAT"));
        let count = u32::from_be_bytes(data[fanout + 1020..fanout + 1024].try_into().unwrap());
        let index = |hex: &str| {
            (0..count)
                .find(|&i| {
                    let at = oids + 20 * usize::try_from(i).unwrap();
                    let id: String = data[at..at + 20]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    id == hex
                })
                .unwrap()
        };
        let (c, p) = (index(child), index(parent));
        let record = commits + 36 * usize::try_from(c).unwrap();
        let date = word(record + 28) & ((1 << 34) - 1);
        data[record + 20..record + 24].copy_from_slice(&p.to_be_bytes());
        data[record + 28..record + 36].copy_from_slice(&((level << 34) | date).to_be_bytes());
        std::fs::set_permissions(graph, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(graph, data).unwrap();
    }

    /// A forged commit-graph claims main holds a commit that no branch holds.
    /// Git trusts the graph unless told not to.
    #[test]
    fn forged_commit_graph_cannot_make_a_branch_hold_a_commit() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "agent-graph");
        commit(&wt, "unique");
        let x = head_of(&wt);
        commit_only(&r, "m1");
        let m1 = head_of(&r);
        commit_only(&r, "m2");
        let m2 = head_of(&r);
        // A branch puts X in the graph while it is written.
        git(&r, &["branch", "graph-tmp", &x]);
        git(
            &r,
            &[
                "-c",
                "commitGraph.generationVersion=1",
                "commit-graph",
                "write",
                "--reachable",
                "--no-progress",
            ],
        );
        git(&r, &["branch", "-D", "graph-tmp"]);
        // The first commit is level 1 and X level 2, so m1 on X is level 3.
        let graph = r.join(".git/objects/info/commit-graph");
        forge_first_parent(&graph, &m1, &x, 3);
        forge_first_parent(&graph, &m2, &m1, 4);
        assert!(
            git(&r, &["branch", "--contains", &x]).contains("main"),
            "the forged graph must fool git that reads it"
        );
        age(&fx, &wt, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// A blobless clone with lazy fetch off cannot diff a commit whose blobs
    /// it never fetched, so the cherry-pick check fails with an error. That
    /// must not stop the pushed check.
    #[test]
    fn cherry_pick_check_error_still_lets_a_pushed_worktree_wait_for_stale_days() {
        let fx = Fixture::new(&[]);
        let src = fx.dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q"]);
        git(&src, &["config", "uploadpack.allowFilter", "true"]);
        git(&src, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        std::fs::write(src.join("f"), "line1\nline2\n").unwrap();
        git(&src, &["add", "f"]);
        git(&src, &["commit", "-qm", "f"]);
        git(&src, &["checkout", "-q", "-b", "other"]);
        std::fs::write(src.join("f"), "line1\nother\n").unwrap();
        git(&src, &["commit", "-qam", "other edits f"]);
        git(&src, &["checkout", "-q", "main"]);
        let r = fx.root.join("repo");
        let url = format!("file://{}", src.display());
        let file_ok = ["-c", "protocol.file.allow=always"];
        git(
            fx.dir.path(),
            &[
                &file_ok[..],
                &[
                    "clone",
                    "-q",
                    "--filter=blob:none",
                    &url,
                    r.to_str().unwrap(),
                ],
            ]
            .concat(),
        );
        git(&r, &["branch", "-q", "other", "origin/other"]);
        let wt = worktree(&r, "feat");
        std::fs::write(wt.join("f"), "line1\nmine\n").unwrap();
        git(&wt, &["commit", "-qam", "mine edits f"]);
        git(
            &wt,
            &[&file_ok[..], &["push", "-q", "-u", "origin", "feat"]].concat(),
        );
        age(&fx, &wt, 20);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "feat"), "idle 20 days");
    }

    /// Far more branches than fit on one command line. The cherry-pick check
    /// passes revisions through stdin, so it still works.
    #[test]
    fn cherry_pick_check_works_past_the_command_line_limit() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let base = head_of(&r);
        let wt = detached(&r, "agent-many");
        commit(&wt, "agent-work");
        let id = head_of(&wt);
        pick_onto(&r, "main", &[&id]);
        let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
        for i in 0..20_000 {
            packed.push_str(&format!("{base} refs/heads/many/{i:05}\n"));
        }
        std::fs::write(r.join(".git/packed-refs"), packed).unwrap();
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "agent-many"),
            "cherry-picked to another branch, idle 24 h"
        );
    }

    /// The branch holding HEAD merged in a commit whose object is gone, so
    /// reading merge records fails. The worktree is still done.
    #[test]
    fn unreadable_merge_records_fall_back_to_naming_the_branch() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "feat");
        commit(&wt, "work");
        git(&r, &["checkout", "-q", "-b", "side", "main"]);
        commit_only(&r, "side-work");
        let side = head_of(&r);
        git(&r, &["checkout", "-q", "-b", "stacked", "feat"]);
        git(&r, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        git(&r, &["checkout", "-q", "main"]);
        git(&r, &["branch", "-q", "-D", "side"]);
        std::fs::remove_file(r.join(".git/objects").join(&side[..2]).join(&side[2..])).unwrap();
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "feat"), "on stacked, idle 24 h");
    }

    /// Without `--`, git reads a ref that does not exist as a path when a file
    /// of that name exists, and answers as if the ref held nothing.
    #[test]
    fn a_missing_branch_is_an_error_rather_than_a_path() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let head = head_of(&r);
        std::fs::create_dir_all(r.join("refs/heads")).unwrap();
        std::fs::write(r.join("refs/heads/gone"), "a file, not a branch").unwrap();
        let t = task(14, vec![]);
        let gone = ["gone".to_string()];
        assert!(t.merge_commit_of(&r, &head, &gone).is_err());
        assert!(t.reflog_merged_into(&r, &head, &gone).is_err());
        assert!(t.unique_commits(&r, &head, &["refs/heads/gone"]).is_err());
        assert!(
            t.copied_commits(&r, &[head.as_str()], &["refs/heads/gone"])
                .is_err()
        );
    }

    /// Two worktrees found done because other branches held their HEADs.
    /// Those branches are deleted before the run. One worktree is pushed.
    #[test]
    fn run_refuses_a_worktree_that_is_no_longer_done() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        add_origin(&fx, &r);
        let local = worktree(&r, "local");
        commit(&local, "local-work");
        git(&r, &["branch", "local-copy", "local"]);
        let pushed = worktree(&r, "pushed");
        commit(&pushed, "pushed-work");
        git(&pushed, &["push", "-q", "-u", "origin", "pushed"]);
        git(&r, &["branch", "pushed-copy", "pushed"]);
        age(&fx, &local, 1);
        age(&fx, &pushed, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["local", "pushed"]);
        git(&r, &["branch", "-q", "-D", "local-copy", "pushed-copy"]);
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        let why = |name: &str| {
            report
                .failed
                .iter()
                .find(|(p, _)| p.ends_with(name))
                .map(|(_, why)| why.as_str())
                .unwrap()
        };
        assert_eq!(why("local"), "removing it could now lose commits");
        assert_eq!(
            why("pushed"),
            "is no longer done, so it waits for stale_days"
        );
    }

    /// An empty commit writes only to the repo's git dir, so the check for
    /// files written during the run cannot see it. On a detached HEAD, no
    /// branch holds it. Judging the worktree again keeps it.
    #[test]
    fn run_refuses_a_worktree_that_gained_a_commit_after_the_scan() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "late-commit");
        age(&fx, &wt, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(reason_of(&found, "late-commit"), "on main, idle 24 h");
        git(&wt, &["commit", "-q", "--allow-empty", "-m", "late"]);
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.failed[0].1, "removing it could now lose commits");
        assert!(wt.exists());
    }

    /// A commit on top of main that no branch holds. The main worktree makes
    /// it on a detached HEAD, then returns to main.
    fn unbranched_commit(repo: &Path) -> String {
        git(repo, &["checkout", "-q", "--detach", "main"]);
        commit_only(repo, "elsewhere");
        let id = head_of(repo);
        git(repo, &["checkout", "-q", "main"]);
        assert!(!on_a_branch(repo, &id));
        id
    }

    #[test]
    fn reads_commits_made_here_from_a_head_reflog() {
        let [a, b, c, d, e, f, g] = ["a", "b", "c", "d", "e", "f", "g"].map(|x| x.repeat(40));
        let raw = [
            format!("{a}\tcheckout: moving from {b} to main"),
            format!("{b}\tcommit (amend): x"),
            format!("{c}\tcommit: x"),
            format!("{d}\treset: moving to HEAD"),
            format!("{e}\tmerge topic: Fast-forward"),
            format!("{f}\tpull origin main: Fast-forward"),
            format!("{g}\trebase (finish): returning to refs/heads/feat"),
            format!("{g}\trebase (pick): x"),
            format!("{d}\trebase -i (start): checkout main"),
            format!("{b}\tcherry-pick: x"),
            format!("{a}\tfrobnicate: something git may add later"),
            format!("{d}\t"),
        ]
        .join("\n");
        assert_eq!(
            made_here(&raw).unwrap(),
            vec![b.as_str(), c.as_str(), g.as_str(), a.as_str()]
        );
        assert!(made_here("no tab here\n").is_err());
    }

    /// A detached worktree makes a commit, then checks out main. Only the
    /// HEAD reflog reaches the commit, and removal deletes that reflog.
    #[test]
    fn commit_left_behind_by_a_checkout_waits_for_stale_days() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "left-behind");
        commit(&wt, "draft");
        git(&wt, &["checkout", "-q", "--detach", "main"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "not stale yet: {found:?}");
        age(&fx, &wt, 20);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(
            reason_of(&found, "left-behind"),
            "has commits on no branch, idle 20 days"
        );
        // Judged again, it still waited for stale_days, so it goes.
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(!wt.exists());
    }

    /// The shape of a real agent worktree: added, reset, then two checkouts,
    /// the first of a commit no branch holds. It made nothing, so it is done.
    #[test]
    fn commits_only_checked_out_do_not_count() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let visited = unbranched_commit(&r);
        let wt = detached(&r, "visitor");
        git(&wt, &["reset", "-q", "--hard", "HEAD"]);
        git(&wt, &["checkout", "-q", "--detach", &visited]);
        git(&wt, &["checkout", "-q", "--detach", "main"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "visitor"), "on main, idle 24 h");
    }

    /// Rewording a commit changes no content, so the draft's patch id matches
    /// the reworded commit, which a branch holds.
    #[test]
    fn message_only_amend_counts_as_a_copy_of_its_draft() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "reworded");
        commit(&wt, "x");
        git(&wt, &["commit", "--amend", "-qm", "x, reworded"]);
        git(&r, &["branch", "keep", &head_of(&wt)]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "reworded"), "on keep, idle 24 h");
    }

    #[test]
    fn content_amend_leaves_a_draft_that_waits_for_stale_days() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "redrafted");
        commit(&wt, "x");
        std::fs::write(wt.join("x"), "x, changed").unwrap();
        git(&wt, &["commit", "--amend", "-qam", "x"]);
        git(&r, &["branch", "keep", &head_of(&wt)]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "not stale yet: {found:?}");
        age(&fx, &wt, 20);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "redrafted"),
            "has commits on no branch, idle 20 days"
        );
    }

    /// The commits made before a rebase are on no branch, but the rebased
    /// copies that main now holds match them.
    #[test]
    fn commits_replaced_by_a_rebase_count_when_their_copies_are_on_a_branch() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "rebased");
        commit(&wt, "f1");
        commit(&wt, "f2");
        commit_only(&r, "main-moves");
        git(&wt, &["rebase", "-q", "main"]);
        git(&r, &["merge", "-q", "--ff-only", "rebased"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        let reason = reason_of(&found, "rebased");
        assert!(reason.starts_with("merged into main"), "{reason}");
    }

    /// A reflog message Picard does not know counts as a commit made here.
    #[test]
    fn unknown_reflog_message_counts_as_a_commit_made_here() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let other = unbranched_commit(&r);
        let wt = detached(&r, "odd");
        git(
            &wt,
            &["update-ref", "-m", "frobnicate: moved", "HEAD", &other],
        );
        git(&wt, &["checkout", "-q", "--detach", "main"]);
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "not stale yet: {found:?}");
        age(&fx, &wt, 20);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(
            reason_of(&found, "odd"),
            "has commits on no branch, idle 20 days"
        );
    }

    /// With no reflog there is nothing to read, so the rules apply as they
    /// would without this check. Such a commit is then lost on removal.
    #[test]
    fn worktree_without_a_head_reflog_is_judged_by_its_head_alone() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = detached(&r, "no-reflog");
        commit(&wt, "draft");
        git(&wt, &["checkout", "-q", "--detach", "main"]);
        std::fs::remove_file(r.join(".git/worktrees/no-reflog/logs/HEAD")).unwrap();
        age(&fx, &wt, 1);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(reason_of(&found, "no-reflog"), "on main, idle 24 h");
    }

    /// `refs/worktree/` lives in the worktree's git dir, so removal deletes
    /// it. A ref there to a commit no branch holds keeps the worktree. One to
    /// a commit main holds does not.
    #[test]
    fn worktree_ref_holding_an_unbranched_commit_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let other = unbranched_commit(&r);
        let keeper = detached(&r, "keeper");
        git(&keeper, &["update-ref", "refs/worktree/keep", &other]);
        let fine = detached(&r, "fine");
        git(&fine, &["update-ref", "refs/worktree/on-main", "main"]);
        age(&fx, &keeper, 60);
        age(&fx, &fine, 60);
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found), vec!["fine"]);
    }

    #[test]
    fn run_judges_the_reflog_and_worktree_refs_again() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let other = unbranched_commit(&r);
        let drafted = detached(&r, "late-draft");
        let referenced = detached(&r, "late-ref");
        age(&fx, &drafted, 1);
        age(&fx, &referenced, 1);
        let t = task(14, vec![]);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found), vec!["late-draft", "late-ref"]);
        // Neither step writes to the worktree folder.
        git(&drafted, &["commit", "-q", "--allow-empty", "-m", "draft"]);
        git(&drafted, &["checkout", "-q", "--detach", "main"]);
        git(&referenced, &["update-ref", "refs/worktree/keep", &other]);
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        let why = |name: &str| {
            report
                .failed
                .iter()
                .find(|(p, _)| p.ends_with(name))
                .map(|(_, why)| why.as_str())
                .unwrap()
        };
        assert_eq!(
            why("late-draft"),
            "is no longer done, so it waits for stale_days"
        );
        assert_eq!(why("late-ref"), "removing it could now lose commits");
    }

    #[test]
    fn paused_notes_merge_keeps_the_worktree() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "noter");
        git(&wt, &["notes", "add", "-m", "base note", "HEAD"]);
        git(
            &wt,
            &["notes", "--ref=other", "add", "-m", "other note", "HEAD"],
        );
        git(
            &wt,
            &["notes", "--ref=other", "append", "-m", "more", "HEAD"],
        );
        git(&wt, &["notes", "append", "-m", "mine", "HEAD"]);
        assert!(
            !git_ok(&wt, &["notes", "merge", "-s", "manual", "other"]),
            "the notes merge must stop on its conflict"
        );
        age(&fx, &wt, 1);
        assert_eq!(
            GitCmd::default().operation_in_progress(&wt).unwrap(),
            Some("a notes merge")
        );
        let found = task(14, vec![]).scan(&fx.ctx()).unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    /// The last check before `git worktree remove`. Nothing between the
    /// re-check and removal can be paused in a test, so this calls the check
    /// directly, after HEAD moved past the entry it was given.
    #[test]
    fn removal_refuses_a_head_that_moved_after_it_was_checked() {
        let fx = Fixture::new(&[]);
        let r = repo(&fx);
        let wt = worktree(&r, "moving");
        let t = task(14, vec![]);
        let listed = t.worktrees(&r).unwrap();
        let entry = listed[1].clone();
        assert_eq!(t.head_unmoved(&entry), Ok(()));
        commit(&wt, "moved");
        assert_eq!(
            t.head_unmoved(&entry),
            Err("HEAD moved since it was checked".to_string())
        );
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
