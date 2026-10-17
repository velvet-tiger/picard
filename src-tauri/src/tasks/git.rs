//! Runs `git` and parses the bits of its output Picard needs.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// How to invoke git.
///
/// TODO(M5): build this from the resolved login-shell PATH so a Finder- or
/// login-launched app finds Homebrew's git. Until then `git` must be on PATH.
#[derive(Debug, Clone)]
pub struct GitCmd {
    program: PathBuf,
}

impl Default for GitCmd {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
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
        cmd
    }

    /// Runs git in `dir` and returns stdout. Fails with stderr on a non-zero exit.
    pub fn output(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self
            .command(dir, args)
            .output()
            .with_context(|| format!("running git {} in {}", args.join(" "), dir.display()))?;
        if !out.status.success() {
            bail!(
                "git {} in {} failed: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs a git command whose exit status is the answer, like
    /// `merge-base --is-ancestor`: 0 is yes, 1 is no, anything else is an error.
    pub fn check(&self, dir: &Path, args: &[&str]) -> Result<bool> {
        let out = self
            .command(dir, args)
            .output()
            .with_context(|| format!("running git {} in {}", args.join(" "), dir.display()))?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git {} in {} failed: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
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
    fn parses_bare_main_repo() {
        let e = parse_worktree_list("worktree /r.git\0bare\0\0");
        assert!(e[0].bare);
    }
}
