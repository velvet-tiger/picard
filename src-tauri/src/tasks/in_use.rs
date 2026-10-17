//! Which folders running processes are working in.
//!
//! A worktree that is the working folder of a running process (a Claude Code
//! session, a terminal, a dev server) is in use and must not be removed, however
//! old its last commit is.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// `lsof` ships with macOS at this path, so no PATH lookup is needed.
const LSOF: &str = "/usr/sbin/lsof";

/// The working folders of all running processes visible to this user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InUse {
    cwds: Vec<PathBuf>,
}

impl InUse {
    #[cfg(test)]
    pub fn from_cwds(cwds: Vec<PathBuf>) -> Self {
        Self { cwds }
    }

    /// Asks `lsof` for every process's working folder.
    ///
    /// `lsof` exits non-zero when it cannot inspect some processes (for example
    /// root-owned ones), so its output is used whenever it lists any folder. An
    /// empty listing is an error, because every session has at least Picard's
    /// own working folder and silence would wrongly mean "nothing in use".
    pub fn load() -> Result<Self> {
        let out = Command::new(LSOF)
            .args(["-nP", "-d", "cwd", "-Fn"])
            .output()
            .with_context(|| format!("running {LSOF}"))?;
        let parsed = parse_lsof(&String::from_utf8_lossy(&out.stdout));
        if parsed.cwds.is_empty() {
            bail!(
                "{LSOF} listed no working folders (exit {:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(parsed)
    }

    /// True if any process is working in `dir` or a folder inside it.
    pub fn contains(&self, dir: &Path) -> bool {
        let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.cwds
            .iter()
            .any(|cwd| cwd.starts_with(&canon) || cwd.starts_with(dir))
    }
}

/// Parses `lsof -Fn` field output: lines starting with `n` hold a path.
pub fn parse_lsof(raw: &str) -> InUse {
    let mut cwds: Vec<PathBuf> = raw
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    cwds.sort();
    cwds.dedup();
    InUse { cwds }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_field_output_and_matches_nested_cwds() {
        let raw = "p100\nfcwd\nn/Users/me/repo/.claude/worktrees/a\np200\nfcwd\nn/Users/me/repo/.claude/worktrees/b/src\np300\nfcwd\nn/\n";
        let used = parse_lsof(raw);
        assert!(used.contains(Path::new("/Users/me/repo/.claude/worktrees/a")));
        assert!(used.contains(Path::new("/Users/me/repo/.claude/worktrees/b")));
        assert!(!used.contains(Path::new("/Users/me/repo/.claude/worktrees/c")));
        // A sibling whose name shares a prefix is not in use.
        assert!(!used.contains(Path::new("/Users/me/repo/.claude/worktrees/a2")));
    }

    #[test]
    fn real_lsof_sees_this_process() {
        let used = InUse::load().expect("lsof should list working folders");
        let here = std::env::current_dir().unwrap();
        assert!(used.contains(&here), "{here:?} not in {used:?}");
    }
}
