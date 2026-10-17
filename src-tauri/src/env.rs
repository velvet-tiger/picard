//! Finds the tools a login-launched app cannot see.
//!
//! Apps started from Finder or at login get launchd's PATH,
//! `/usr/bin:/bin:/usr/sbin:/sbin`, so Homebrew's git, nvm's npm and pnpm are
//! invisible. Picard asks the user's login shell for its PATH once at startup,
//! the same way a new Terminal window gets it, and uses that for every child.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::ToolOverrides;
use crate::proc;

const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_SHELL: &str = "/bin/zsh";

/// Folders added after the login shell's PATH, so common tools are found even
/// if the shell gave nothing usable. `~` is the user's home folder.
const STANDARD_DIRS: &[&str] = &[
    "~/.cargo/bin",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// The PATH Picard gives its children, and how to find a tool on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEnv {
    home: PathBuf,
    dirs: Vec<PathBuf>,
}

impl ToolEnv {
    /// Asks the login shell for its PATH. Never fails: if the shell cannot be
    /// run, the standard folders are used and the reason is logged.
    pub fn detect(home: &Path, shell_var: Option<OsString>) -> Self {
        let shell = shell_from_env(shell_var);
        let login = match login_shell_path(&shell) {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("using standard folders only; login shell PATH unavailable: {e:#}");
                None
            }
        };
        let env = Self {
            home: home.to_path_buf(),
            dirs: build_dirs(login.as_deref(), home),
        };
        log::info!("tool PATH: {}", env.path_var());
        env
    }

    #[cfg(test)]
    pub fn from_dirs(home: &Path, dirs: Vec<PathBuf>) -> Self {
        Self {
            home: home.to_path_buf(),
            dirs,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The value to set as PATH for child processes.
    pub fn path_var(&self) -> String {
        self.dirs
            .iter()
            .map(|d| d.to_string_lossy())
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Where `tool` lives: the config override if it is executable, otherwise
    /// the first executable named `tool` on the PATH.
    pub fn resolve(&self, tool: &str, overrides: &ToolOverrides) -> Option<PathBuf> {
        if let Some(path) = overrides.get(tool) {
            if is_executable(path) {
                return Some(path.to_path_buf());
            }
            log::warn!(
                "[tools] {tool} = {} is not an executable file; searching PATH instead",
                path.display()
            );
        }
        self.dirs
            .iter()
            .map(|d| d.join(tool))
            .find(|p| is_executable(p))
    }
}

/// `$SHELL` if it is an absolute path to an existing file, else `/bin/zsh`.
/// A login-launched app may have `SHELL` unset or set to a bare name.
fn shell_from_env(value: Option<OsString>) -> PathBuf {
    value
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_file())
        .unwrap_or_else(|| PathBuf::from(FALLBACK_SHELL))
}

/// Runs `shell -lc 'printf %s "$PATH"'`. A login shell reads the user's profile
/// files, exactly as opening a Terminal window would.
fn login_shell_path(shell: &Path) -> Result<String> {
    let mut cmd = Command::new(shell);
    cmd.args(["-lc", "printf %s \"$PATH\""]);
    let out = proc::run(cmd, SHELL_TIMEOUT, proc::SCRIPT_OUTPUT_CAP)
        .with_context(|| format!("running {}", shell.display()))?;
    if !out.success() {
        bail!(
            "{} -lc failed: {}",
            shell.display(),
            out.failure_summary(SHELL_TIMEOUT)
        );
    }
    let path = out.stdout.trim().to_string();
    if path.is_empty() {
        bail!("{} printed an empty PATH", shell.display());
    }
    Ok(path)
}

/// Builds the folder list: the login shell's PATH first, then the standard
/// folders. Expands `~`, drops relative and empty entries, removes duplicates.
fn build_dirs(login: Option<&str>, home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let entries = login
        .unwrap_or("")
        .split(':')
        .chain(STANDARD_DIRS.iter().copied());
    for entry in entries {
        let dir = if entry == "~" {
            home.to_path_buf()
        } else if let Some(rest) = entry.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(entry)
        };
        // A relative entry would resolve against whatever folder a child runs in.
        if dir.is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_drops_relative_and_dedupes() {
        let dirs = build_dirs(
            Some("/opt/homebrew/bin:~/.dotnet/tools:relative/bin::/usr/bin:/opt/homebrew/bin"),
            Path::new("/Users/me"),
        );
        assert_eq!(dirs[0], PathBuf::from("/opt/homebrew/bin"));
        assert_eq!(dirs[1], PathBuf::from("/Users/me/.dotnet/tools"));
        assert_eq!(dirs[2], PathBuf::from("/usr/bin"));
        assert!(dirs.contains(&PathBuf::from("/Users/me/.cargo/bin")));
        assert!(!dirs.iter().any(|d| d.is_relative()));
        let unique: std::collections::HashSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len());
    }

    #[test]
    fn falls_back_to_standard_dirs() {
        let dirs = build_dirs(None, Path::new("/Users/me"));
        assert_eq!(dirs[0], PathBuf::from("/Users/me/.cargo/bin"));
        assert!(dirs.contains(&PathBuf::from("/usr/bin")));
    }

    #[test]
    fn shell_must_be_an_absolute_existing_file() {
        assert_eq!(
            shell_from_env(Some("zsh".into())),
            PathBuf::from("/bin/zsh")
        );
        assert_eq!(shell_from_env(None), PathBuf::from("/bin/zsh"));
        assert_eq!(
            shell_from_env(Some("/bin/bash".into())),
            PathBuf::from("/bin/bash")
        );
        assert_eq!(
            shell_from_env(Some("/no/such/shell".into())),
            PathBuf::from("/bin/zsh")
        );
    }

    fn fake_tool(dir: &Path, name: &str, executable: bool) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn resolves_first_executable_on_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fake_tool(a.path(), "npm", false);
        let real = fake_tool(b.path(), "npm", true);
        let env = ToolEnv::from_dirs(
            Path::new("/Users/me"),
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
        );
        assert_eq!(env.resolve("npm", &ToolOverrides::default()), Some(real));
        assert_eq!(env.resolve("pnpm", &ToolOverrides::default()), None);
    }

    #[test]
    fn override_wins_when_executable_and_is_ignored_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let on_path = fake_tool(dir.path(), "git", true);
        let custom_dir = tempfile::tempdir().unwrap();
        let custom = fake_tool(custom_dir.path(), "my-git", true);
        let env = ToolEnv::from_dirs(Path::new("/Users/me"), vec![dir.path().to_path_buf()]);

        let o = ToolOverrides {
            git: Some(custom.clone()),
            ..ToolOverrides::default()
        };
        assert_eq!(env.resolve("git", &o), Some(custom));

        let o = ToolOverrides {
            git: Some(custom_dir.path().join("missing")),
            ..ToolOverrides::default()
        };
        assert_eq!(env.resolve("git", &o), Some(on_path));
    }

    #[test]
    fn real_login_shell_reports_a_path() {
        let home = std::env::home_dir().unwrap();
        let env = ToolEnv::detect(&home, Some("/bin/zsh".into()));
        assert!(env.path_var().contains("/usr/bin"), "{}", env.path_var());
        assert!(env.resolve("sh", &ToolOverrides::default()).is_some());
    }
}
