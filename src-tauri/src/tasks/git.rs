//! Runs `git` and parses the bits of its output Picard needs.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{NEVER_STOP, Stop};
use crate::proc::{self, Output};

/// Generous, because `git status` on a large repo with a cold cache is slow,
/// but bounded so a hung git (a stuck network filesystem) cannot stall a scan.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// `git worktree remove` deletes every file in the worktree, which takes many
/// minutes on a large `node_modules`. Stopping it part way leaves a broken
/// worktree, so it gets far longer than a read. It still has a limit, so a
/// stuck disk cannot hold the run lock for ever.
const WORKTREE_REMOVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Picard parses git listings to make safety decisions, so it must see all of
/// them. Output past this size is treated as an error rather than trusted.
const GIT_OUTPUT_CAP: usize = 256 * 1024 * 1024;

/// What git prints, with `LC_ALL=C`, once it has searched every parent folder
/// for a repo. The second is for a search that stopped at a mount point.
const NO_REPO_FOUND: &[&str] = &[
    "not a git repository (or any of the parent directories)",
    "not a git repository (or any parent up to mount point",
];

/// `git status` for deciding whether removing a checkout loses work. Untracked
/// files are listed explicitly, because `status.showUntrackedFiles=no` in repo
/// or user config would otherwise hide files that removal deletes.
pub const SAFETY_STATUS: &[&str] = &[
    "status",
    "--porcelain",
    "--untracked-files=all",
    "--ignore-submodules=all",
];

/// How long git may run before Picard stops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GitLimits {
    /// Every command except `worktree remove`.
    normal: Duration,
    /// `worktree remove` only.
    removal: Duration,
}

const GIT_LIMITS: GitLimits = GitLimits {
    normal: GIT_TIMEOUT,
    removal: WORKTREE_REMOVE_TIMEOUT,
};

/// How to invoke git: which binary, and the PATH it runs with.
#[derive(Debug, Clone)]
pub struct GitCmd {
    program: PathBuf,
    path_var: Option<String>,
    limits: GitLimits,
}

impl Default for GitCmd {
    /// `git` from the current PATH. Used by tests and as a last resort.
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            path_var: None,
            limits: GIT_LIMITS,
        }
    }
}

impl GitCmd {
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.program);
        // Picard runs git unattended in every repo under the roots, including
        // repos it did not create (an unpacked archive can carry a .git folder).
        // Repo config can name programs for git to run. Command-line config wins
        // over repo config, so these cannot be set by a repo. With
        // `log.showSignature`, `git log` runs `gpg.program`. Filters are
        // handled by `untrusted_filters`, because their names are not known ahead.
        cmd.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "log.showSignature=false",
            // See the note on GIT_NO_REPLACE_OBJECTS below.
            "-c",
            "core.commitGraph=false",
            // `log` re-encodes commit and reflog text to this. Picard parses
            // that text, so repo config such as UTF-16 must not change it.
            "-c",
            "i18n.logOutputEncoding=UTF-8",
            // A stale untracked cache can list an empty folder as untracked.
            // `worktree remove` checks in the mode that reads the cache, so it
            // refuses a worktree Picard found clean. `-c` reaches that check.
            "-c",
            "core.untrackedCache=false",
        ]);
        cmd.arg("-C").arg(dir).args(args);
        // Picard only reads, apart from `worktree remove`. Skipping optional
        // locks stops `git status` from contending with a live session's git.
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        // A repo can claim to be a partial clone with missing objects. `status`
        // then fetches them, and the fetch runs whatever program the repo
        // config names, such as `core.sshCommand` or `remote.*.uploadpack`.
        // Picard never needs the network. The first variable stops lazy fetch
        // (git 2.45 and later). The second refuses every transport, which also
        // covers older git, and environment beats repo `protocol.*.allow`.
        cmd.env("GIT_NO_LAZY_FETCH", "1");
        cmd.env("GIT_ALLOW_PROTOCOL", "none");
        // A replace ref can make a commit look contained in, or merged into, a
        // branch that does not hold it. Picard decides safety from history, so
        // it reads the real one. Grafts do the same and are refused in
        // `unsafe_to_inspect`, because no variable turns them off. A
        // commit-graph file caches each commit's parents, and git trusts it
        // over the commits themselves, so a forged one can do the same.
        // `core.commitGraph=false` above makes git read the commits.
        cmd.env("GIT_NO_REPLACE_OBJECTS", "1");
        if let Some(path) = &self.path_var {
            cmd.env("PATH", path);
        }
        cmd
    }

    pub fn new(program: PathBuf, path_var: String) -> Self {
        Self {
            program,
            path_var: Some(path_var),
            limits: GIT_LIMITS,
        }
    }

    /// Replaces the time limits, so a test need not wait for the real ones.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, normal: Duration, removal: Duration) -> Self {
        self.limits = GitLimits { normal, removal };
        self
    }

    fn exec(
        &self,
        dir: &Path,
        args: &[&str],
        input: Option<String>,
        timeout: Duration,
    ) -> Result<Output> {
        let cmd = self.command(dir, args);
        let out = match input {
            None => proc::run(cmd, timeout, GIT_OUTPUT_CAP),
            Some(text) => proc::run_with_input(cmd, text.into_bytes(), timeout, GIT_OUTPUT_CAP),
        }
        .with_context(|| {
            format!(
                "running {} {} in {}",
                self.program.display(),
                shown_args(args),
                dir.display()
            )
        })?;
        if out.timed_out || out.truncated {
            bail!(
                "git {} in {}: {}",
                shown_args(args),
                dir.display(),
                if out.timed_out {
                    "timed out"
                } else {
                    "output too large to check"
                }
            );
        }
        Ok(out)
    }

    /// Runs git in `dir` and returns stdout. Fails with stderr on a non-zero exit.
    pub fn output(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self.exec(dir, args, None, self.limits.normal)?;
        stdout_of(out, dir, args)
    }

    /// Runs `git worktree remove` on `worktree` from `repo`, under the long
    /// limit that only this command gets. `--force` is the caller's decision.
    pub fn remove_worktree(&self, repo: &Path, worktree: &Path, force: bool) -> Result<()> {
        let path = worktree.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        let out = self.exec(repo, &args, None, self.limits.removal)?;
        stdout_of(out, repo, &args).map(drop)
    }

    /// Like `output`, but writes `input` to git's stdin, as `--stdin` options
    /// read it. A list of revisions passed this way cannot overflow the
    /// command line, and git never reads one of them as a path. It takes the
    /// input by value, so a large one is not held twice.
    pub fn output_with_input(&self, dir: &Path, args: &[&str], input: String) -> Result<String> {
        let out = self.exec(dir, args, Some(input), self.limits.normal)?;
        stdout_of(out, dir, args)
    }

    /// True if `dir` is in a git repo, false if git says plainly that it is
    /// not. Any other failure is an error, so the caller can keep a folder it
    /// cannot judge. Git prints a similar line for a broken `.git` file, so
    /// only the wording for "searched every parent" counts. On an external
    /// drive that search ends at the mount point, with wording of its own. A
    /// `.git` entry
    /// in `dir` or any parent means something is there that git cannot read.
    pub fn in_repo(&self, dir: &Path) -> Result<bool> {
        let args = ["rev-parse", "--git-dir"];
        let mut cmd = self.command(dir, &args);
        // Git translates this message, and its wording is the answer here.
        cmd.env("LC_ALL", "C");
        let out = proc::run(cmd, self.limits.normal, GIT_OUTPUT_CAP)
            .with_context(|| format!("running git rev-parse in {}", dir.display()))?;
        if out.success() {
            return Ok(true);
        }
        let searched =
            out.code == Some(128) && NO_REPO_FOUND.iter().any(|m| out.stderr.contains(m));
        let dot_git = dir
            .ancestors()
            .map(|a| a.join(".git"))
            .find(|p| std::fs::symlink_metadata(p).is_ok());
        if searched && dot_git.is_none() {
            return Ok(false);
        }
        bail!(
            "cannot tell whether {} is in a git repo: {}{}",
            dir.display(),
            shown_stderr(&out.stderr),
            dot_git.map_or(String::new(), |p| format!(" ({} exists)", p.display()))
        )
    }

    /// Runs a git command whose exit status is the answer, like
    /// `merge-base --is-ancestor`: 0 is yes, 1 is no, anything else is an error.
    pub fn check(&self, dir: &Path, args: &[&str]) -> Result<bool> {
        let out = self.exec(dir, args, None, self.limits.normal)?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git {} in {} failed: {}",
                shown_args(args),
                dir.display(),
                shown_stderr(&out.stderr)
            ),
        }
    }
}

/// About how many bytes of git's stderr an error message shows.
const STDERR_SHOWN: usize = 2_000;

/// The end of `stderr`, cut to about `STDERR_SHOWN` bytes, for an error
/// message. Git writes the fatal line last, and a failure with a huge
/// stderr must not put megabytes into the log.
fn shown_stderr(stderr: &str) -> String {
    let text = stderr.trim();
    if text.len() <= STDERR_SHOWN {
        return text.to_string();
    }
    let mut start = text.len() - STDERR_SHOWN;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("… ({start} bytes cut) {}", &text[start..])
}

/// About how much of a command's arguments an error message shows.
const ARGS_SHOWN: usize = 500;

/// `args` joined by spaces for an error message, cut to about `ARGS_SHOWN`
/// characters. A command with a huge argument list must not put megabytes
/// into the log when it fails.
fn shown_args(args: &[&str]) -> String {
    let mut text = String::new();
    for (i, arg) in args.iter().enumerate() {
        let sep = if i == 0 { "" } else { " " };
        let room = ARGS_SHOWN.saturating_sub(text.len() + sep.len());
        text.push_str(sep);
        if arg.len() <= room {
            text.push_str(arg);
            continue;
        }
        text.extend(arg.chars().take(room));
        return format!("{text}… ({} more args)", args.len() - i - 1);
    }
    text
}

/// The stdout of a finished git command, or an error with its stderr.
fn stdout_of(out: Output, dir: &Path, args: &[&str]) -> Result<String> {
    if !out.success() {
        bail!(
            "git {} in {} failed: {}",
            shown_args(args),
            dir.display(),
            shown_stderr(&out.stderr)
        );
    }
    Ok(out.stdout)
}

/// What git keeps in a worktree's git dir while an operation is paused, and a
/// name for each operation.
const IN_PROGRESS: &[(&str, &str)] = &[
    ("rebase-merge", "a rebase"),
    ("rebase-apply", "a rebase or `git am`"),
    ("MERGE_HEAD", "a merge"),
    ("CHERRY_PICK_HEAD", "a cherry-pick"),
    ("REVERT_HEAD", "a revert"),
    ("sequencer", "a cherry-pick or revert of several commits"),
    ("BISECT_LOG", "a bisect"),
    ("BISECT_START", "a bisect"),
    ("NOTES_MERGE_WORKTREE", "a notes merge"),
    ("NOTES_MERGE_PARTIAL", "a notes merge"),
    ("NOTES_MERGE_REF", "a notes merge"),
];

impl GitCmd {
    /// The git operation paused in worktree `dir`, such as "a rebase", if any.
    /// A paused rebase or bisect can leave a clean worktree on a detached HEAD
    /// that another branch holds, while its state lives in the worktree's git
    /// dir. Removing the worktree deletes that state. `--git-path` resolves
    /// each name for this worktree. A path that cannot be checked is an error,
    /// so the worktree is kept. Reads only; runs nothing the repo names.
    pub fn operation_in_progress(&self, dir: &Path) -> Result<Option<&'static str>> {
        let mut args = vec!["rev-parse", "--path-format=absolute"];
        for (name, _) in IN_PROGRESS {
            args.extend(["--git-path", name]);
        }
        let raw = self.output(dir, &args)?;
        let paths: Vec<&str> = raw.lines().collect();
        if paths.len() != IN_PROGRESS.len() {
            bail!(
                "git rev-parse --git-path in {}: expected {} paths, got {raw:?}",
                dir.display(),
                IN_PROGRESS.len()
            );
        }
        for ((_, what), path) in IN_PROGRESS.iter().zip(paths) {
            match std::fs::symlink_metadata(path) {
                Ok(_) => return Ok(Some(what)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("checking {path}")),
            }
        }
        Ok(None)
    }
}

impl GitCmd {
    /// Filter drivers defined in the repo's own config (or the worktree's) rather
    /// than the user's global or system config. Commands like `git status` and
    /// `git worktree remove` can run a filter's program, so a worktree that has
    /// any is skipped. Reading config runs nothing.
    pub fn untrusted_filters(&self, dir: &Path) -> Result<Vec<String>> {
        let raw = self.output(dir, &["config", "--list", "--show-scope", "-z"])?;
        Ok(repo_scoped_filters(&raw))
    }
}

impl GitCmd {
    /// Why git must not run `status` or `worktree remove` in `dir`, if anything.
    ///
    /// Both commands can run a clean filter. Filters defined in the repo's own
    /// config are refused.
    ///
    /// A submodule that was never checked out is only an empty folder, with no
    /// config of its own. Claude Code's agent worktrees look like this. A
    /// gitlink folder that holds anything other than a submodule checkout is
    /// refused, because git would delete those files without listing them in
    /// `status`.
    ///
    /// A checked-out submodule keeps its repository in `<git dir>/modules`,
    /// which removing the worktree deletes. So each checkout must have no
    /// filters, no submodules of its own and no uncommitted changes, and each
    /// repository there must hold no commit missing from its remote branches.
    /// Only then is nothing lost. The checkout is checked before git runs
    /// `status` in it, and nothing here runs a program the repos name.
    pub fn unsafe_to_inspect(&self, dir: &Path) -> Result<Option<String>> {
        let common = self.output(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        if std::fs::symlink_metadata(Path::new(common.trim()).join("info/grafts")).is_ok() {
            return Ok(Some(
                "repo has info/grafts, which can make history look merged".to_string(),
            ));
        }
        let filters = self.untrusted_filters(dir)?;
        if !filters.is_empty() {
            return Ok(Some(format!(
                "repo config defines filters that git could run: {}",
                filters.join(", ")
            )));
        }
        let modules = self.submodule_store_root(dir)?;
        let has_stores = std::fs::symlink_metadata(&modules).is_ok();
        let index = self.output(dir, &["ls-files", "--stage", "-z"])?;
        let submodules: Vec<PathBuf> = index_gitlinks(&index).iter().map(|g| dir.join(g)).collect();
        if let Some(why) = self.hidden_changes(dir)? {
            return Ok(Some(why));
        }
        let others = find_checkouts(dir, &submodules, Search::Worktree, NEVER_STOP)?;
        if let Some((other, why)) = self.first_refused(dir, &others.git, NEVER_STOP)? {
            log::debug!("nested checkout {}: {why}", other.display());
            return Ok(Some(format!(
                "holds another git checkout at {}",
                other.display()
            )));
        }
        for gitlink in index_gitlinks(&index) {
            let folder = dir.join(gitlink);
            let Some(why) = submodule_folder_problem(&folder) else {
                continue;
            };
            if !has_stores {
                return Ok(Some(format!("submodule {gitlink} {why}")));
            }
            if let Some(why) = self.checkout_problem(&folder, &modules)? {
                return Ok(Some(format!("submodule {gitlink} {why}")));
            }
        }
        if has_stores {
            for store in submodule_stores(&modules)? {
                if let Some(why) = self.store_problem(&store)? {
                    return Ok(Some(format!("submodule repo {}: {why}", store.display())));
                }
            }
        }
        Ok(None)
    }

    /// Why `git status` cannot be trusted in `dir`, if it cannot. It does not
    /// look at a file marked skip-worktree or assume-unchanged, so an edit
    /// there stays hidden, and removal would delete it. `ls-files -v` tags any
    /// such file with something other than `H`. A sparse checkout is refused
    /// too, because its skipped files carry the same mark.
    pub fn hidden_changes(&self, dir: &Path) -> Result<Option<String>> {
        let raw = self.output(dir, &["ls-files", "-v", "-z"])?;
        Ok(unverified_entry(&raw)
            .map(|path| format!("git status does not check {path}, so it could hide an edit")))
    }

    /// The first of `checkouts` inside `outer` that must be kept, with why.
    /// A stopped scan gets an error, so it keeps the folder and asks no more.
    fn first_refused(
        &self,
        outer: &Path,
        checkouts: &[PathBuf],
        stop: Stop,
    ) -> Result<Option<(PathBuf, String)>> {
        for checkout in checkouts {
            if stop() {
                bail!("stopped while checking {}", outer.display());
            }
            if let Some(why) = self.nested_checkout_problem(outer, checkout)? {
                return Ok(Some((checkout.clone(), why)));
            }
        }
        Ok(None)
    }

    /// The first repo inside build output folder `output` of `project` that
    /// must be kept, with why. A folder that cannot be read is an error, so
    /// the output is kept.
    pub fn output_checkout_problem(
        &self,
        project: &Path,
        output: &Path,
        stop: Stop,
    ) -> Result<Option<(PathBuf, String)>> {
        let found = find_checkouts(output, &[], Search::Output, stop)?;
        if let Some(other) = found.other {
            return Ok(Some(other));
        }
        self.first_refused(project, &found.git, stop)
    }

    /// The `.git` folders of the checkouts in worktree `dir`, other than
    /// submodules. `written_since` skips `.git`, so a commit in one of these
    /// shows only here.
    pub fn nested_git_dirs(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let index = self.output(dir, &["ls-files", "--stage", "-z"])?;
        let submodules: Vec<PathBuf> = index_gitlinks(&index).iter().map(|g| dir.join(g)).collect();
        let found = find_checkouts(dir, &submodules, Search::Worktree, NEVER_STOP)?;
        Ok(found.git.iter().map(|c| c.join(".git")).collect())
    }

    /// Why deleting the git checkout at `checkout`, inside `outer`, could lose
    /// work, if it could. A package manager that installs from source, such as
    /// Composer with `--prefer-source` or Terraform's module installer, leaves
    /// a real clone in its package folder. That clone is safe to delete only
    /// when all of this holds:
    /// - its `.git` is a folder. A `.git` file is a linked worktree or a
    ///   submodule, whose repository lives somewhere else.
    /// - it sits in a package folder (`in_package_folder`)
    /// - `outer` ignores it, so `outer` never meant to keep it
    /// - git run there really reads this `.git` folder and this folder
    /// - it has no `info/grafts`, which can make history look pushed
    /// - it has no linked worktrees, whose repository is this one
    /// - no operation is paused in it, whose state removal would delete
    /// - no repo filters, no submodules and nothing uncommitted
    ///   (`contents_problem`), and no commit missing from what it fetched from
    ///   a real remote (`clone_store_problem`)
    ///
    /// Ignored files inside it are not checked, as for a worktree. A git error
    /// is returned, so the caller keeps the folder.
    pub fn nested_checkout_problem(&self, outer: &Path, checkout: &Path) -> Result<Option<String>> {
        let inside = checkout
            .strip_prefix(outer)
            .with_context(|| format!("{} is outside {}", checkout.display(), outer.display()))?;
        let git_dir = checkout.join(".git");
        if !std::fs::symlink_metadata(&git_dir).is_ok_and(|m| m.is_dir()) {
            return Ok(Some("its .git is not a folder".to_string()));
        }
        if !in_package_folder(inside) {
            return Ok(Some(
                "is not in a vendor or .terraform/modules folder".to_string(),
            ));
        }
        let path = format!("./{}", inside.display());
        if !self.check(outer, &["check-ignore", "-q", "--", &path])? {
            return Ok(Some("the repo around it does not ignore it".to_string()));
        }
        if !self.reads_its_own_git_dir(checkout, &git_dir)? {
            return Ok(Some(
                "git there does not use its own .git folder".to_string(),
            ));
        }
        if std::fs::symlink_metadata(git_dir.join("info/grafts")).is_ok() {
            return Ok(Some(
                "has info/grafts, which can make history look pushed".to_string(),
            ));
        }
        if has_entries(&git_dir.join("worktrees"))? {
            return Ok(Some("has linked worktrees of its own".to_string()));
        }
        if let Some(op) = self.operation_in_progress(checkout)? {
            return Ok(Some(format!("has {op} in progress")));
        }
        if let Some(why) = self.contents_problem(checkout)? {
            return Ok(Some(why));
        }
        self.clone_store_problem(&git_dir)
    }

    /// True if git run in `folder` works on `folder`, with `git_dir` as both
    /// its git dir and its common dir. An invalid `.git` folder makes git use
    /// the repo around it, `core.worktree` can name another folder, and a
    /// `commondir` file makes git read another repo's refs. Each would make
    /// the checks that follow judge the wrong thing.
    fn reads_its_own_git_dir(&self, folder: &Path, git_dir: &Path) -> Result<bool> {
        let raw = self.output(
            folder,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
                "--show-toplevel",
            ],
        )?;
        let same = |seen: &str, want: &Path| {
            Ok::<bool, anyhow::Error>(
                Path::new(seen)
                    .canonicalize()
                    .with_context(|| format!("resolving {seen}"))?
                    == want
                        .canonicalize()
                        .with_context(|| format!("resolving {}", want.display()))?,
            )
        };
        let mut lines = raw.lines();
        let (Some(dir), Some(common), Some(top), None) =
            (lines.next(), lines.next(), lines.next(), lines.next())
        else {
            bail!(
                "git rev-parse in {}: unexpected output {raw:?}",
                folder.display()
            );
        };
        Ok(same(dir, git_dir)? && same(common, git_dir)? && same(top, folder)?)
    }

    /// True if `dir` has submodule repositories that `git worktree remove`
    /// would delete. Git only removes such a worktree with `--force`.
    pub fn has_submodule_stores(&self, dir: &Path) -> Result<bool> {
        let modules = self.submodule_store_root(dir)?;
        Ok(std::fs::symlink_metadata(modules).is_ok())
    }

    /// `<git dir>/modules`, where git keeps the repositories of `dir`'s submodules.
    pub fn submodule_store_root(&self, dir: &Path) -> Result<PathBuf> {
        let git_dir = self.output(dir, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
        Ok(Path::new(git_dir.trim()).join("modules"))
    }

    /// Why a non-empty gitlink `folder` is unsafe, if it is. It must be a
    /// submodule checkout whose repository is inside `modules`, with no repo
    /// filters, no submodules of its own and nothing uncommitted.
    ///
    /// The checkout's top level must be `folder` itself. A `.git` file can
    /// point at another submodule's repository, whose `core.worktree` names a
    /// different folder. `status` would then check that folder, not this one.
    fn checkout_problem(&self, folder: &Path, modules: &Path) -> Result<Option<String>> {
        let git_dir = self.output(
            folder,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )?;
        let top = self.output(
            folder,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )?;
        let in_store = match (
            Path::new(git_dir.trim()).canonicalize(),
            modules.canonicalize(),
            Path::new(top.trim()).canonicalize(),
            folder.canonicalize(),
        ) {
            (Ok(repo), Ok(store), Ok(top), Ok(folder)) => {
                repo.starts_with(&store) && repo != store && top == folder
            }
            _ => false,
        };
        if !in_store {
            return Ok(Some(
                "is not empty and is not a submodule checkout".to_string(),
            ));
        }
        self.contents_problem(folder)
    }

    /// Why git cannot vouch for the files in checkout `folder`, if it cannot:
    /// repo filters, submodules of its own, files `status` does not check, or
    /// anything uncommitted. Call it
    /// only once `folder` is known to be the checkout git will read, because
    /// `status` otherwise reports on another folder.
    fn contents_problem(&self, folder: &Path) -> Result<Option<String>> {
        let filters = self.untrusted_filters(folder)?;
        if !filters.is_empty() {
            return Ok(Some(format!(
                "config defines filters that git could run: {}",
                filters.join(", ")
            )));
        }
        let index = self.output(folder, &["ls-files", "--stage", "-z"])?;
        if !index_gitlinks(&index).is_empty() {
            return Ok(Some("has submodules of its own".to_string()));
        }
        if let Some(why) = self.hidden_changes(folder)? {
            return Ok(Some(why));
        }
        let status = self.output(folder, SAFETY_STATUS)?;
        if !status.trim().is_empty() {
            return Ok(Some("has uncommitted changes".to_string()));
        }
        Ok(None)
    }

    /// Why deleting the submodule repository at `store` would lose work, if it
    /// would. Every commit reachable from its HEAD, branches, tags, stash and
    /// reflogs must also be reachable from a ref under `refs/remotes/`. The
    /// reflog counts because `submodule update` moves HEAD off work done on a
    /// detached HEAD, and the reflog is then the only way back to it. A repo
    /// with no such ref fails unless it has no commits.
    ///
    /// Nothing is fetched, so this shows what the last fetch saw. A branch
    /// deleted or force-pushed on the remote since then still counts, and no
    /// offline check can tell. Any ref there counts, even one made by hand.
    /// `clone_store_problem` is stricter, for repos that nothing else vouches
    /// for. A submodule's commit is named by its parent repo, and its remote
    /// can be a local path, so this keeps accepting those.
    fn store_problem(&self, store: &Path) -> Result<Option<String>> {
        if std::fs::symlink_metadata(store.join("modules")).is_ok() {
            return Ok(Some("has submodules of its own".to_string()));
        }
        let unique = self.output(
            store,
            &[
                "--git-dir=.",
                "rev-list",
                "--count",
                "--all",
                "--reflog",
                "--not",
                "--remotes",
            ],
        )?;
        match unique.trim() {
            "0" => Ok(None),
            n => Ok(Some(format!(
                "has {n} commit(s) that are on no remote branch"
            ))),
        }
    }

    /// Why deleting the nested clone's repository at `git_dir` would lose
    /// work, if it would. It works like `store_problem`, but a ref counts only
    /// when the fetch refspec of a configured remote writes it, and that
    /// remote's URLs are all on the network. A local path can vanish with
    /// the folder, and `git update-ref` can make any ref by hand. The limit
    /// described at `store_problem` still applies: nothing here is fetched.
    fn clone_store_problem(&self, git_dir: &Path) -> Result<Option<String>> {
        if std::fs::symlink_metadata(git_dir.join("modules")).is_ok() {
            return Ok(Some("has submodules of its own".to_string()));
        }
        let config = self.output(
            git_dir,
            &["--git-dir=.", "config", "--local", "--list", "-z"],
        )?;
        let patterns = fetched_ref_patterns(&config);
        let refs = self.output(
            git_dir,
            &[
                "--git-dir=.",
                "for-each-ref",
                "--format=%(refname)",
                "refs/remotes/",
            ],
        )?;
        let exclusions: String = refs
            .lines()
            .filter(|r| patterns.iter().any(|p| ref_matches(p, r)))
            .map(|r| format!("^{r}\n"))
            .collect();
        let unique = self.output_with_input(
            git_dir,
            &[
                "--git-dir=.",
                "rev-list",
                "--count",
                "--all",
                "--reflog",
                "--stdin",
            ],
            exclusions,
        )?;
        match unique.trim() {
            "0" => Ok(None),
            n => Ok(Some(format!(
                "has {n} commit(s) that no fetch from a network remote brought in"
            ))),
        }
    }
}

/// True if `dir` holds anything. A missing folder holds nothing. Anything
/// unreadable is an error, so the caller keeps what it was judging.
fn has_entries(dir: &Path) -> Result<bool> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// The path of the first entry in `git ls-files -v -z` output whose tag is not
/// `H`, which is a cached file that `status` checks. Lowercase means
/// assume-unchanged and `S` means skip-worktree. A line this cannot read
/// counts too, so it can only refuse.
fn unverified_entry(raw: &str) -> Option<&str> {
    raw.split('\0')
        .filter(|entry| !entry.is_empty())
        .find_map(|entry| match entry.split_once(' ') {
            Some(("H", _)) => None,
            Some((_, path)) => Some(path),
            None => Some(entry),
        })
}

/// True if `url` reaches another machine. Local paths, `file:` URLs and
/// transport helpers such as `ext::` do not count, nor does any scheme not
/// listed. An scp-style `host:path` counts.
fn is_network_url(url: &str) -> bool {
    const SCHEMES: &[&str] = &["https", "http", "ssh", "git", "git+ssh", "ssh+git"];
    if let Some((scheme, _)) = url.split_once("://") {
        return SCHEMES.iter().any(|s| s.eq_ignore_ascii_case(scheme));
    }
    match url.split_once(':') {
        Some((host, path)) => {
            !host.is_empty()
                && !host.contains('/')
                && !host.eq_ignore_ascii_case("file")
                && !path.is_empty()
                && !path.starts_with(':')
        }
        None => false,
    }
}

/// The destinations under `refs/remotes/` of the fetch refspecs of remotes
/// whose URLs are all on the network, from `git config --list -z` output.
/// Each record is `key NEWLINE value NUL`. `url.*.insteadOf` can rewrite a
/// network URL to a local path, so a config with one trusts nothing.
fn fetched_ref_patterns(raw: &str) -> Vec<&str> {
    let mut urls: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut fetches: Vec<(&str, &str)> = Vec::new();
    for record in raw.split('\0') {
        let (key, value) = record.split_once('\n').unwrap_or((record, ""));
        if key.starts_with("url.") {
            return Vec::new();
        }
        let Some(rest) = key.strip_prefix("remote.") else {
            continue;
        };
        if let Some(name) = rest.strip_suffix(".url") {
            urls.entry(name).or_default().push(value);
        } else if let Some(name) = rest.strip_suffix(".fetch") {
            fetches.push((name, value));
        }
    }
    fetches
        .into_iter()
        .filter(|(name, _)| {
            urls.get(name)
                .is_some_and(|list| list.iter().all(|u| is_network_url(u)))
        })
        .filter_map(|(_, spec)| {
            let spec = spec.strip_prefix('+').unwrap_or(spec);
            let (source, dest) = spec.split_once(':')?;
            (!source.starts_with('^') && dest.starts_with("refs/remotes/")).then_some(dest)
        })
        .collect()
}

/// True if ref `name` matches refspec side `pattern`, which holds at most one `*`.
fn ref_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((start, end)) => {
            name.len() >= start.len() + end.len() && name.starts_with(start) && name.ends_with(end)
        }
        None => pattern == name,
    }
}

/// Folders never searched for nested checkouts: regenerable build output and
/// dependencies, which can be huge, and which Picard removes anyway.
const NOT_SEARCHED: &[&str] = &["node_modules", "target", ".next"];

/// What `find_checkouts` is searching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Search {
    /// A worktree. It is a checkout itself, so a `.git` at its top is its own,
    /// and `NOT_SEARCHED` folders are left out.
    Worktree,
    /// A build output folder. Everything is searched, and a `.git` at the top
    /// makes the folder itself a checkout.
    Output,
}

/// True if `path`, relative to the repo around it, is below a folder that
/// package managers install into: a `vendor` folder at any depth, or
/// `.terraform/modules`. Anything in `.claude/worktrees` is an agent's working
/// copy rather than a package, so it never counts.
fn in_package_folder(path: &Path) -> bool {
    let names: Vec<_> = path.iter().collect();
    if names.windows(2).any(|w| w == [".claude", "worktrees"]) {
        return false;
    }
    let above = &names[..names.len().saturating_sub(1)];
    above.iter().any(|name| *name == "vendor")
        || above.windows(2).any(|w| w == [".terraform", "modules"])
}

/// What `find_checkouts` found.
#[derive(Debug, Default)]
struct Checkouts {
    /// Folders holding a `.git`.
    git: Vec<PathBuf>,
    /// The first repo that no check here can judge, with why it must be kept.
    /// Only `Search::Output` looks for these.
    other: Option<(PathBuf, String)>,
}

/// Why folder `dir` is a repo Picard cannot judge, if it is. A bare repo holds
/// its commits with no `.git`, and Mercurial and Subversion keep theirs in
/// folders of their own.
fn unjudged_repo(dir: &Path, name: &OsStr) -> Option<String> {
    if name == ".hg" || name == ".svn" {
        return Some(format!("holds a {} folder", name.to_string_lossy()));
    }
    is_git_store(dir).then(|| "is a bare git repository".to_string())
}

/// Every git checkout inside `dir` other than its submodule checkouts, which
/// `unsafe_to_inspect` verifies. Claude Code puts worktrees in
/// `.claude/worktrees/`, which git ignores, so `status` never lists their
/// untracked files, yet removing `dir` deletes them. No depth limit. An
/// unreadable folder is an error, so `dir` is kept, and so is a stopped scan.
fn find_checkouts(
    dir: &Path,
    submodules: &[PathBuf],
    search: Search,
    stop: Stop,
) -> Result<Checkouts> {
    let mut found = Vec::new();
    if search == Search::Output
        && let Some(why) = unjudged_repo(dir, OsStr::new(""))
    {
        return Ok(Checkouts {
            git: found,
            other: Some((dir.to_path_buf(), why)),
        });
    }
    let mut walk = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walk.next() {
        if stop() {
            bail!("stopped while looking for checkouts in {}", dir.display());
        }
        let e = entry.with_context(|| format!("looking for checkouts in {}", dir.display()))?;
        let name = e.file_name();
        if name == ".git" {
            if e.file_type().is_dir() {
                walk.skip_current_dir();
            }
            // The worktree's own `.git` file sits at the top.
            if e.depth() == 1 && search == Search::Worktree {
                continue;
            }
            let checkout = e.path().parent().unwrap_or(dir);
            if !submodules.iter().any(|s| s == checkout) {
                found.push(checkout.to_path_buf());
            }
            continue;
        }
        if search == Search::Output
            && e.file_type().is_dir()
            && let Some(why) = unjudged_repo(e.path(), name)
        {
            return Ok(Checkouts {
                git: found,
                other: Some((e.path().to_path_buf(), why)),
            });
        }
        if search == Search::Worktree
            && e.file_type().is_dir()
            && name.to_str().is_some_and(|n| NOT_SEARCHED.contains(&n))
        {
            // A checkout can itself be called `target`, so look before skipping.
            let is_checkout = std::fs::symlink_metadata(e.path().join(".git")).is_ok();
            if is_checkout && !submodules.iter().any(|s| s == e.path()) {
                found.push(e.path().to_path_buf());
            }
            walk.skip_current_dir();
        }
    }
    Ok(Checkouts {
        git: found,
        other: None,
    })
}

/// The `.git` folders of the checkouts inside build output folder `output`.
pub fn output_git_dirs(output: &Path) -> Result<Vec<PathBuf>> {
    let found = find_checkouts(output, &[], Search::Output, NEVER_STOP)?;
    Ok(found.git.iter().map(|c| c.join(".git")).collect())
}

/// The submodule repositories under `modules`. A submodule's name can contain
/// `/`, so a repository can sit several folders down. A folder holding a
/// regular `HEAD` file and an `objects` folder is a repository and is not
/// searched further. A symlink, or a folder with only some of a repository's
/// parts, is an error. The caller then keeps the worktree, because a
/// repository it cannot check would be deleted unchecked.
pub fn submodule_stores(modules: &Path) -> Result<Vec<PathBuf>> {
    let mut stores = Vec::new();
    let mut pending = vec![modules.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if dir != modules {
            if is_git_store(&dir) {
                stores.push(dir);
                continue;
            }
            if has_store_parts(&dir) {
                bail!(
                    "{} looks like a git repository but cannot be checked",
                    dir.display()
                );
            }
        }
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let kind = entry
                .file_type()
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if kind.is_symlink() {
                bail!("{} is a symlink", entry.path().display());
            }
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    stores.sort();
    Ok(stores)
}

pub(super) fn is_git_store(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join("HEAD")).is_ok_and(|m| m.is_file())
        && std::fs::symlink_metadata(dir.join("objects")).is_ok_and(|m| m.is_dir())
}

/// True if `dir` holds any of the entries a git repository has.
fn has_store_parts(dir: &Path) -> bool {
    ["HEAD", "objects", "refs"]
        .iter()
        .any(|part| std::fs::symlink_metadata(dir.join(part)).is_ok())
}

/// Paths of the gitlinks (mode 160000) in `git ls-files --stage -z` output.
/// A gitlink is how a submodule appears in the index. Each record is
/// `mode SP sha SP stage TAB path NUL`.
pub fn index_gitlinks(raw: &str) -> Vec<&str> {
    raw.split('\0')
        .filter(|entry| entry.starts_with("160000 "))
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path))
        .collect()
}

/// Why a submodule's folder makes its worktree unsafe, if it does.
/// Missing or empty is safe: nothing is checked out and nothing would be lost.
fn submodule_folder_problem(folder: &Path) -> Option<String> {
    let meta = match std::fs::symlink_metadata(folder) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("cannot be read: {e}")),
    };
    if !meta.is_dir() {
        return Some("is not a folder".to_string());
    }
    match std::fs::read_dir(folder).map(|mut entries| entries.next().is_none()) {
        Ok(true) => None,
        Ok(false) => Some("is checked out or not empty".to_string()),
        Err(e) => Some(format!("cannot be read: {e}")),
    }
}

/// Parses `git config --list --show-scope -z` and returns `filter.*` keys set in
/// local or worktree scope. Each record is `scope NUL key NEWLINE value NUL`.
pub fn repo_scoped_filters(raw: &str) -> Vec<String> {
    let mut parts = raw.split('\0');
    let mut keys = Vec::new();
    while let (Some(scope), Some(entry)) = (parts.next(), parts.next()) {
        let key = entry.split_once('\n').map_or(entry, |(k, _)| k);
        if matches!(scope, "local" | "worktree") && key.starts_with("filter.") {
            keys.push(key.to_string());
        }
    }
    keys
}

/// One entry from `git worktree list --porcelain -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    /// Short branch name, or `None` for a detached HEAD.
    pub branch: Option<String>,
    pub locked: bool,
    pub prunable: bool,
    pub bare: bool,
}

/// Parses `git worktree list --porcelain -z`. The first entry is the main worktree.
///
/// With `-z`, each attribute ends in NUL and each entry ends in an extra NUL,
/// so paths with spaces or newlines parse correctly.
pub fn parse_worktree_list(raw: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in raw.split('\0') {
        if field.is_empty() {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(value),
                head: String::new(),
                branch: None,
                locked: false,
                prunable: false,
                bare: false,
            });
            continue;
        }
        let Some(e) = current.as_mut() else { continue };
        match key {
            "HEAD" => e.head = value.to_string(),
            "branch" => {
                e.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "locked" => e.locked = true,
            "prunable" => e.prunable = true,
            "bare" => e.bare = true,
            _ => {}
        }
    }
    if let Some(e) = current {
        entries.push(e);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repo config cannot turn on `log.showSignature`, which makes `git log`
    /// run the repo's `gpg.program`.
    #[test]
    fn repo_config_cannot_turn_on_signature_checks() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitCmd::default();
        git.output(dir.path(), &["init", "-q"]).unwrap();
        git.output(dir.path(), &["config", "log.showSignature", "true"])
            .unwrap();
        let seen = git
            .output(dir.path(), &["config", "--bool", "log.showSignature"])
            .unwrap();
        assert_eq!(seen.trim(), "false");
    }

    /// Repo config cannot turn the commit-graph back on, so a forged graph
    /// cannot change which parents git sees.
    #[test]
    fn repo_config_cannot_turn_on_the_commit_graph() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitCmd::default();
        git.output(dir.path(), &["init", "-q"]).unwrap();
        git.output(dir.path(), &["config", "core.commitGraph", "true"])
            .unwrap();
        let seen = git
            .output(dir.path(), &["config", "--bool", "core.commitGraph"])
            .unwrap();
        assert_eq!(seen.trim(), "false");
    }

    /// Repo config cannot turn on the untracked cache, which can go stale and
    /// make `worktree remove` refuse a clean worktree.
    #[test]
    fn repo_config_cannot_turn_on_the_untracked_cache() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitCmd::default();
        git.output(dir.path(), &["init", "-q"]).unwrap();
        git.output(dir.path(), &["config", "core.untrackedCache", "true"])
            .unwrap();
        let seen = git
            .output(dir.path(), &["config", "--bool", "core.untrackedCache"])
            .unwrap();
        assert_eq!(seen.trim(), "false");
    }

    /// Repo config cannot change the encoding of `log` output, which Picard
    /// parses.
    #[test]
    fn repo_config_cannot_change_the_log_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitCmd::default();
        git.output(dir.path(), &["init", "-q"]).unwrap();
        git.output(dir.path(), &["config", "i18n.logOutputEncoding", "UTF-16"])
            .unwrap();
        let seen = git
            .output(dir.path(), &["config", "i18n.logOutputEncoding"])
            .unwrap();
        assert_eq!(seen.trim(), "UTF-8");
    }

    /// A stand-in for git that takes a second over anything and then succeeds.
    fn slow_git(dir: &Path) -> GitCmd {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-git");
        std::fs::write(&path, "#!/bin/sh\nsleep 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        GitCmd::new(path, std::env::var("PATH").unwrap())
    }

    #[test]
    fn worktree_removal_gets_a_longer_limit_than_a_read() {
        assert_eq!(GIT_LIMITS.normal, Duration::from_secs(60));
        assert_eq!(GIT_LIMITS.removal, Duration::from_secs(30 * 60));
    }

    /// The bug this guards against: a removal slower than the read limit was
    /// killed part way, which left a half-deleted worktree.
    #[test]
    fn removal_is_not_held_to_the_short_limit() {
        let dir = tempfile::tempdir().unwrap();
        let git =
            slow_git(dir.path()).with_limits(Duration::from_millis(100), Duration::from_secs(30));
        git.remove_worktree(dir.path(), Path::new("/wt"), false)
            .unwrap();
    }

    #[test]
    fn removal_stops_at_its_own_limit() {
        let dir = tempfile::tempdir().unwrap();
        let git =
            slow_git(dir.path()).with_limits(Duration::from_secs(30), Duration::from_millis(100));
        let err = git
            .remove_worktree(dir.path(), Path::new("/wt"), false)
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    }

    #[test]
    fn prune_and_reads_keep_the_short_limit() {
        let dir = tempfile::tempdir().unwrap();
        let git =
            slow_git(dir.path()).with_limits(Duration::from_millis(100), Duration::from_secs(30));
        for args in [&["worktree", "prune"][..], SAFETY_STATUS] {
            let err = git.output(dir.path(), args).unwrap_err();
            assert!(
                format!("{err:#}").contains("timed out"),
                "{args:?}: {err:#}"
            );
        }
    }

    #[test]
    fn error_messages_show_only_the_end_of_a_long_stderr() {
        assert_eq!(
            shown_stderr("  fatal: bad revision\n"),
            "fatal: bad revision"
        );
        let long = format!(
            "{}fatal: the last line",
            "warning: noise é\n".repeat(10_000)
        );
        let shown = shown_stderr(&long);
        assert!(shown.len() < STDERR_SHOWN + 40, "{} bytes", shown.len());
        assert!(shown.starts_with("… ("), "{shown}");
        assert!(shown.ends_with("fatal: the last line"), "{shown}");
    }

    #[test]
    fn error_messages_show_only_the_start_of_a_long_argument_list() {
        assert_eq!(shown_args(&["rev-parse", "HEAD"]), "rev-parse HEAD");
        let many: Vec<String> = (0..20_000).map(|i| format!("refs/heads/b{i:05}")).collect();
        let mut args = vec!["rev-list"];
        args.extend(many.iter().map(String::as_str));
        let shown = shown_args(&args);
        assert!(shown.len() < ARGS_SHOWN + 40, "{} chars", shown.len());
        assert!(shown.starts_with("rev-list refs/heads/b00000 "), "{shown}");
        assert!(shown.ends_with(" more args)"), "{shown}");
        // One huge argument is cut too.
        let huge = "x".repeat(100_000);
        assert!(shown_args(&[huge.as_str()]).len() < ARGS_SHOWN + 40);

        let dir = tempfile::tempdir().unwrap();
        let git = GitCmd::default();
        git.output(dir.path(), &["init", "-q"]).unwrap();
        let err = git.output(dir.path(), &args).unwrap_err().to_string();
        assert!(err.len() < 2_000, "{} chars: {err}", err.len());
    }

    #[test]
    fn in_repo_tells_a_repo_from_no_repo() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitCmd::default();
        assert!(!git.in_repo(dir.path()).unwrap());
        git.output(dir.path(), &["init", "-q"]).unwrap();
        let inner = dir.path().join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        assert!(git.in_repo(&inner).unwrap());
    }

    /// Each of these is a folder Picard cannot judge, not one outside a repo.
    #[test]
    fn in_repo_treats_anything_unclear_as_an_error() {
        let git = GitCmd::default();
        let broken_file = tempfile::tempdir().unwrap();
        std::fs::write(
            broken_file.path().join(".git"),
            "gitdir: /gone/.git/worktrees/x\n",
        )
        .unwrap();
        assert!(git.in_repo(broken_file.path()).is_err());

        // Git skips an invalid `.git` folder and reports no repo at all.
        let broken_folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(broken_folder.path().join(".git")).unwrap();
        let inner = broken_folder.path().join("a");
        std::fs::create_dir(&inner).unwrap();
        assert!(git.in_repo(&inner).is_err());

        assert!(git.in_repo(Path::new("/no/such/folder")).is_err());
    }

    /// A stand-in for git that always fails with `stderr`.
    fn git_failing_with(dir: &Path, stderr: &str) -> GitCmd {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-git");
        let script = format!("#!/bin/sh\ncat >&2 <<'E'\n{stderr}\nE\nexit 128\n");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        GitCmd::new(path, std::env::var("PATH").unwrap())
    }

    /// What git prints on an external drive, where the search ends at the
    /// mount point. Proven build output there must not be kept for ever.
    #[test]
    fn in_repo_reads_the_search_that_stopped_at_a_mount_point() {
        let dir = tempfile::tempdir().unwrap();
        let git = git_failing_with(
            dir.path(),
            "fatal: not a git repository (or any parent up to mount point /Volumes)\n\
             Stopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set).",
        );
        assert!(!git.in_repo(dir.path()).unwrap());

        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(
            git.in_repo(dir.path()).is_err(),
            "a .git above still counts"
        );
    }

    #[test]
    fn in_repo_treats_another_git_failure_as_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-git");
        std::fs::write(
            &path,
            "#!/bin/sh\necho 'fatal: out of memory' >&2\nexit 128\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let git = GitCmd::new(path, std::env::var("PATH").unwrap());
        assert!(git.in_repo(dir.path()).is_err());
    }

    #[test]
    fn only_a_network_url_counts_as_a_remote() {
        for yes in [
            "https://github.com/a/b.git",
            "HTTPS://github.com/a/b.git",
            "ssh://git@github.com/a/b",
            "git://example.com/a",
            "git@github.com:a/b.git",
            "github.com:a/b.git",
        ] {
            assert!(is_network_url(yes), "{yes}");
        }
        for no in [
            "",
            ".",
            "..",
            "/srv/git/a.git",
            "../a",
            "./a",
            "a/b",
            "a/b:c",
            "file:///srv/a",
            "FILE:///srv/a",
            "file:/srv/a",
            "ext::sh -c x",
            "fd::3",
            "ftp://example.com/a",
        ] {
            assert!(!is_network_url(no), "{no}");
        }
    }

    #[test]
    fn only_fetch_refspecs_of_network_remotes_are_trusted() {
        let config = "core.bare\nfalse\0\
            remote.origin.url\nhttps://example.com/a\0\
            remote.origin.fetch\n+refs/heads/*:refs/remotes/origin/*\0\
            remote.up.url\ngit@example.com:a\0\
            remote.up.fetch\nrefs/heads/main:refs/remotes/up/main\0\
            remote.up.fetch\n^refs/heads/x:refs/remotes/up/x\0\
            remote.up.fetch\nrefs/tags/*:refs/tags/*\0\
            remote.disk.url\n/srv/a\0\
            remote.disk.fetch\n+refs/heads/*:refs/remotes/disk/*\0\
            remote.both.url\nhttps://example.com/a\0\
            remote.both.url\n/srv/a\0\
            remote.both.fetch\n+refs/heads/*:refs/remotes/both/*\0\
            remote.nourl.fetch\n+refs/heads/*:refs/remotes/nourl/*\0";
        assert_eq!(
            fetched_ref_patterns(config),
            vec!["refs/remotes/origin/*", "refs/remotes/up/main"]
        );
        let rewritten = format!("{config}url./srv/.insteadof\nhttps://example.com/\0");
        assert!(fetched_ref_patterns(&rewritten).is_empty());
    }

    #[test]
    fn matches_a_ref_against_a_refspec_side() {
        let p = "refs/remotes/origin/*";
        assert!(ref_matches(p, "refs/remotes/origin/main"));
        assert!(ref_matches(p, "refs/remotes/origin/a/b"));
        assert!(!ref_matches(p, "refs/remotes/originals/main"));
        assert!(ref_matches("refs/remotes/o/main", "refs/remotes/o/main"));
        assert!(!ref_matches("refs/remotes/o/main", "refs/remotes/o/mai"));
        assert!(!ref_matches("refs/remotes/a*a", "refs/remotes/a"));
    }

    #[test]
    fn only_tag_h_counts_as_a_file_status_checks() {
        assert_eq!(unverified_entry(""), None);
        assert_eq!(unverified_entry("H a\0H b c\0"), None);
        assert_eq!(unverified_entry("H a\0S b c\0"), Some("b c"));
        assert_eq!(unverified_entry("h a\0"), Some("a"));
        assert_eq!(unverified_entry("M a\0"), Some("a"));
        assert_eq!(unverified_entry("odd\0"), Some("odd"));
    }

    #[test]
    fn a_folder_that_cannot_be_read_as_a_folder_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_entries(&dir.path().join("missing")).unwrap());
        assert!(!has_entries(dir.path()).unwrap());
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(has_entries(dir.path()).unwrap());
        assert!(has_entries(&file).is_err());
    }

    #[test]
    fn a_stopped_search_is_an_error_and_runs_no_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/.git")).unwrap();
        let stopped: Stop = &|| true;
        // This git fails with a message of its own, so an error naming the
        // stop shows that git never ran.
        let git = git_failing_with(dir.path(), "fatal: git ran");
        let output = dir.path().join("a");
        let err = git
            .output_checkout_problem(dir.path(), dir.path(), stopped)
            .unwrap_err();
        assert!(err.to_string().contains("stopped"), "{err:#}");
        let err = git
            .first_refused(dir.path(), &[output], stopped)
            .unwrap_err();
        assert!(err.to_string().contains("stopped"), "{err:#}");
    }

    #[test]
    fn package_folders_are_vendor_and_terraform_modules_only() {
        for yes in [
            "vendor/acme/lib",
            "vendor/lib",
            "app/vendor/acme/lib",
            ".terraform/modules/vpc",
            "a/.terraform/modules/vpc/inner",
        ] {
            assert!(in_package_folder(Path::new(yes)), "{yes}");
        }
        for no in [
            "vendor",
            "lib",
            "src/lib",
            ".terraform/modules",
            ".terraform/vpc",
            "modules/vpc",
            "vendored/lib",
            ".claude/worktrees/x/vendor/lib",
            "vendor/.claude/worktrees/x",
            "vendor/a/.claude/worktrees/x/y",
        ] {
            assert!(!in_package_folder(Path::new(no)), "{no}");
        }
    }

    /// `.git` in a linked worktree is a file, which must never pass, and the
    /// repo around a clone must ignore it. A build output folder is already
    /// ignored, so only a direct call shows this second condition.
    #[test]
    fn a_nested_checkout_needs_a_git_folder_and_an_ignoring_repo() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().canonicalize().unwrap();
        let git = GitCmd::default();
        git.output(&outer, &["init", "-q"]).unwrap();
        let lib = outer.join("vendor/lib");
        std::fs::create_dir_all(&lib).unwrap();
        git.output(&lib, &["init", "-q"]).unwrap();
        let why = git.nested_checkout_problem(&outer, &lib).unwrap().unwrap();
        assert_eq!(why, "the repo around it does not ignore it");

        std::fs::remove_dir_all(lib.join(".git")).unwrap();
        std::fs::write(lib.join(".git"), "gitdir: /x/.git/worktrees/y\n").unwrap();
        let why = git.nested_checkout_problem(&outer, &lib).unwrap().unwrap();
        assert_eq!(why, "its .git is not a folder");
    }

    #[test]
    fn a_git_error_on_a_nested_checkout_is_an_error_not_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().canonicalize().unwrap();
        let lib = outer.join("vendor/lib");
        std::fs::create_dir_all(lib.join(".git")).unwrap();
        // `outer` is no repo, so `check-ignore` fails.
        assert!(
            GitCmd::default()
                .nested_checkout_problem(&outer, &lib)
                .is_err()
        );
    }

    #[test]
    fn parses_main_branch_detached_locked_and_prunable() {
        let raw = "worktree /r\0HEAD aaa\0branch refs/heads/main\0\0\
                   worktree /r/.claude/worktrees/with space\0HEAD bbb\0detached\0\0\
                   worktree /r/wt-locked\0HEAD ccc\0branch refs/heads/feature/x\0locked reason here\0\0\
                   worktree /gone\0HEAD ddd\0branch refs/heads/old\0prunable gitdir file points to non-existent location\0\0";
        let e = parse_worktree_list(raw);
        assert_eq!(e.len(), 4);
        assert_eq!(e[0].branch.as_deref(), Some("main"));
        assert_eq!(e[1].path, PathBuf::from("/r/.claude/worktrees/with space"));
        assert_eq!(e[1].branch, None);
        assert_eq!(e[1].head, "bbb");
        assert!(e[2].locked);
        assert_eq!(e[2].branch.as_deref(), Some("feature/x"));
        assert!(e[3].prunable);
    }

    #[test]
    fn finds_only_repo_scoped_filters() {
        let raw = "global\0filter.lfs.clean\ngit-lfs clean -- %f\0\
                   local\0core.bare\nfalse\0\
                   local\0filter.evil.clean\ntouch /tmp/pwned\0\
                   worktree\0filter.wt.smudge\nx\0";
        assert_eq!(
            repo_scoped_filters(raw),
            vec!["filter.evil.clean", "filter.wt.smudge"]
        );
    }

    #[test]
    fn lists_gitlinks_in_the_index() {
        let plain = "100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 0\tREADME\0";
        let with_sub =
            "100644 aaa 0\tREADME\x00160000 bbb 0\tvendor/lib\x00160000 ccc 0\twith space\0";
        assert!(index_gitlinks(plain).is_empty());
        assert_eq!(index_gitlinks(with_sub), vec!["vendor/lib", "with space"]);
    }

    #[test]
    fn submodule_folder_is_safe_only_when_missing_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(submodule_folder_problem(&dir.path().join("missing")), None);
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(submodule_folder_problem(&empty), None);
        let stray = dir.path().join("stray");
        std::fs::create_dir(&stray).unwrap();
        std::fs::write(stray.join("notes.txt"), "mine").unwrap();
        assert!(submodule_folder_problem(&stray).is_some());
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(submodule_folder_problem(&file).is_some());
    }

    #[test]
    fn finds_submodule_repos_at_any_depth_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        for repo in ["lib", "vendor/deep/lib"] {
            let r = modules.join(repo);
            std::fs::create_dir_all(r.join("objects")).unwrap();
            std::fs::write(r.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        }
        // A repository's own folders are not searched for more repositories.
        std::fs::create_dir_all(modules.join("lib/objects/HEAD")).unwrap();
        assert_eq!(
            submodule_stores(&modules).unwrap(),
            vec![modules.join("lib"), modules.join("vendor/deep/lib")]
        );
        std::os::unix::fs::symlink(dir.path(), modules.join("vendor/link")).unwrap();
        assert!(submodule_stores(&modules).is_err());
    }

    #[test]
    fn a_repository_that_cannot_be_checked_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        // HEAD as a symlink, as `core.preferSymlinkRefs` writes it.
        let odd = modules.join("odd");
        std::fs::create_dir_all(odd.join("objects")).unwrap();
        std::os::unix::fs::symlink("refs/heads/main", odd.join("HEAD")).unwrap();
        let err = submodule_stores(&modules).unwrap_err().to_string();
        assert!(err.contains("cannot be checked"), "{err}");
    }

    /// A repo claiming to be a partial clone with a missing tree. Without the
    /// guards in `command`, `status` lazily fetches the tree over ssh and runs
    /// the repo's `core.sshCommand`.
    #[test]
    fn status_never_lazily_fetches_through_a_repo_configured_program() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path().join("r");
        let marker = dir.path().join("ssh-ran");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&r)
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        std::fs::create_dir_all(r.join("d")).unwrap();
        git(&["init", "-q"]);
        std::fs::write(r.join("d/f"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "a"]);
        git(&["config", "extensions.partialClone", "origin"]);
        git(&["config", "remote.origin.promisor", "true"]);
        git(&["config", "remote.origin.url", "ssh://example.invalid/x"]);
        let ssh = format!("touch '{}'", marker.display());
        git(&["config", "core.sshCommand", &ssh]);
        let tree = git(&["rev-parse", "HEAD:d"]);
        std::fs::remove_file(r.join(".git/objects").join(&tree[..2]).join(&tree[2..])).unwrap();
        std::fs::remove_file(r.join(".git/index")).unwrap();

        // Status cannot succeed without the tree, and must fail without fetching.
        assert!(GitCmd::default().output(&r, SAFETY_STATUS).is_err());
        assert!(!marker.exists(), "repo-configured ssh command was executed");
    }

    #[test]
    fn parses_bare_main_repo() {
        let e = parse_worktree_list("worktree /r.git\0bare\0\0");
        assert!(e[0].bare);
    }
}
