//! Runs `git` and parses the bits of its output Picard needs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::proc::{self, Output};

/// Generous, because `git status` on a large repo with a cold cache is slow,
/// but bounded so a hung git (a stuck network filesystem) cannot stall a scan.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Picard parses git listings to make safety decisions, so it must see all of
/// them. Output past this size is treated as an error rather than trusted.
const GIT_OUTPUT_CAP: usize = 256 * 1024 * 1024;

/// `git status` for deciding whether removing a checkout loses work. Untracked
/// files are listed explicitly, because `status.showUntrackedFiles=no` in repo
/// or user config would otherwise hide files that removal deletes.
pub const SAFETY_STATUS: &[&str] = &[
    "status",
    "--porcelain",
    "--untracked-files=all",
    "--ignore-submodules=all",
];

/// How to invoke git: which binary, and the PATH it runs with.
#[derive(Debug, Clone)]
pub struct GitCmd {
    program: PathBuf,
    path_var: Option<String>,
}

impl Default for GitCmd {
    /// `git` from the current PATH. Used by tests and as a last resort.
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
            path_var: None,
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
        }
    }

    fn exec(&self, dir: &Path, args: &[&str], input: Option<String>) -> Result<Output> {
        let cmd = self.command(dir, args);
        let out = match input {
            None => proc::run(cmd, GIT_TIMEOUT, GIT_OUTPUT_CAP),
            Some(text) => proc::run_with_input(cmd, text.into_bytes(), GIT_TIMEOUT, GIT_OUTPUT_CAP),
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
        let out = self.exec(dir, args, None)?;
        stdout_of(out, dir, args)
    }

    /// Like `output`, but writes `input` to git's stdin, as `--stdin` options
    /// read it. A list of revisions passed this way cannot overflow the
    /// command line, and git never reads one of them as a path. It takes the
    /// input by value, so a large one is not held twice.
    pub fn output_with_input(&self, dir: &Path, args: &[&str], input: String) -> Result<String> {
        let out = self.exec(dir, args, Some(input))?;
        stdout_of(out, dir, args)
    }

    /// Runs a git command whose exit status is the answer, like
    /// `merge-base --is-ancestor`: 0 is yes, 1 is no, anything else is an error.
    pub fn check(&self, dir: &Path, args: &[&str]) -> Result<bool> {
        let out = self.exec(dir, args, None)?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git {} in {} failed: {}",
                shown_args(args),
                dir.display(),
                out.stderr.trim()
            ),
        }
    }
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
            out.stderr.trim()
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
        if let Some(other) = other_checkout(dir, &submodules)? {
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
        let status = self.output(folder, SAFETY_STATUS)?;
        if !status.trim().is_empty() {
            return Ok(Some("has uncommitted changes".to_string()));
        }
        Ok(None)
    }

    /// Why deleting the submodule repository at `store` would lose work, if it
    /// would. Every commit reachable from its HEAD, branches, tags, stash and
    /// reflogs must also be on one of its remote-tracking branches. The reflog
    /// counts because `submodule update` moves HEAD off work done on a
    /// detached HEAD, and the reflog is then the only way back to it. A repo
    /// with no remote therefore always fails. Nothing is fetched.
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
}

/// Folders never searched for nested checkouts: regenerable build output and
/// dependencies, which can be huge, and which Picard removes anyway.
const NOT_SEARCHED: &[&str] = &["node_modules", "target", ".next"];

/// The first git checkout inside worktree `dir` other than its submodule
/// checkouts, which `unsafe_to_inspect` verifies. Claude Code puts worktrees
/// in `.claude/worktrees/`, which git ignores, so `status` never lists their
/// untracked files, yet removing `dir` deletes them. No depth limit. An
/// unreadable folder is an error, so the worktree is kept.
pub fn other_checkout(dir: &Path, submodules: &[PathBuf]) -> Result<Option<PathBuf>> {
    let mut walk = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walk.next() {
        let e = entry.with_context(|| format!("looking for checkouts in {}", dir.display()))?;
        let name = e.file_name();
        if name == ".git" {
            if e.file_type().is_dir() {
                walk.skip_current_dir();
            }
            // The worktree's own `.git` file sits at the top.
            if e.depth() == 1 {
                continue;
            }
            let checkout = e.path().parent().unwrap_or(dir);
            if !submodules.iter().any(|s| s == checkout) {
                return Ok(Some(checkout.to_path_buf()));
            }
            continue;
        }
        if e.file_type().is_dir() && name.to_str().is_some_and(|n| NOT_SEARCHED.contains(&n)) {
            // A checkout can itself be called `target`, so look before skipping.
            let is_checkout = std::fs::symlink_metadata(e.path().join(".git")).is_ok();
            if is_checkout && !submodules.iter().any(|s| s == e.path()) {
                return Ok(Some(e.path().to_path_buf()));
            }
            walk.skip_current_dir();
        }
    }
    Ok(None)
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

fn is_git_store(dir: &Path) -> bool {
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
