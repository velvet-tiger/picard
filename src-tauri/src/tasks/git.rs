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
        // over repo config, so these two cannot be set by a repo. Filters are
        // handled by `untrusted_filters`, because their names are not known ahead.
        cmd.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ]);
        cmd.arg("-C").arg(dir).args(args);
        // Picard only reads, apart from `worktree remove`. Skipping optional
        // locks stops `git status` from contending with a live session's git.
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
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

    fn exec(&self, dir: &Path, args: &[&str]) -> Result<Output> {
        let out =
            proc::run(self.command(dir, args), GIT_TIMEOUT, GIT_OUTPUT_CAP).with_context(|| {
                format!(
                    "running {} {} in {}",
                    self.program.display(),
                    args.join(" "),
                    dir.display()
                )
            })?;
        if out.timed_out || out.truncated {
            bail!(
                "git {} in {}: {}",
                args.join(" "),
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
        let out = self.exec(dir, args)?;
        if !out.success() {
            bail!(
                "git {} in {} failed: {}",
                args.join(" "),
                dir.display(),
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    /// Runs a git command whose exit status is the answer, like
    /// `merge-base --is-ancestor`: 0 is yes, 1 is no, anything else is an error.
    pub fn check(&self, dir: &Path, args: &[&str]) -> Result<bool> {
        let out = self.exec(dir, args)?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git {} in {} failed: {}",
                args.join(" "),
                dir.display(),
                out.stderr.trim()
            ),
        }
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
    /// config are refused. Checked-out submodules are refused too, because
    /// `status` recurses into them and each has its own config that this check
    /// does not read. `git worktree remove` refuses those anyway.
    ///
    /// A submodule that was never checked out is only an empty folder, with no
    /// config of its own, and git removes such a worktree. Claude Code's agent
    /// worktrees look like this, so they are not refused. A gitlink folder that
    /// holds anything at all is refused, because git would delete those files
    /// without listing them in `status`. Every check here only reads, and runs
    /// nothing.
    pub fn unsafe_to_inspect(&self, dir: &Path) -> Result<Option<String>> {
        let filters = self.untrusted_filters(dir)?;
        if !filters.is_empty() {
            return Ok(Some(format!(
                "repo config defines filters that git could run: {}",
                filters.join(", ")
            )));
        }
        let git_dir = self.output(dir, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
        let modules = Path::new(git_dir.trim()).join("modules");
        if std::fs::symlink_metadata(&modules).is_ok() {
            return Ok(Some(format!(
                "contains checked-out submodules ({} exists)",
                modules.display()
            )));
        }
        let index = self.output(dir, &["ls-files", "--stage", "-z"])?;
        for gitlink in index_gitlinks(&index) {
            if let Some(why) = submodule_folder_problem(&dir.join(gitlink)) {
                return Ok(Some(format!("submodule {gitlink} {why}")));
            }
        }
        Ok(None)
    }
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
    fn parses_bare_main_repo() {
        let e = parse_worktree_list("worktree /r.git\0bare\0\0");
        assert!(e[0].bare);
    }
}
