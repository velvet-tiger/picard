//! Runs a user-defined cleanup script, such as `docker system prune -f`.
//!
//! Picard cannot see what a script will delete, so `scan` never runs it: preview
//! lists it as "will run" with no size. Space freed is measured from the volume's
//! free space before and after, or taken from a `PICARD_FREED=<bytes>` line the
//! script prints, whichever is larger.
//!
//! The script gets:
//! - `PATH`: the resolved tool PATH
//! - `PICARD_ROOTS`: this volume's roots, separated by `:`
//! - `PICARD_DRY_RUN`: always `0` here, since preview does not run scripts
//! - `PICARD_FREE_BYTES`: free space before it started

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use super::{Candidate, CleanupTask, Ctx, Estimate, Removed, Report};
use crate::disk;
use crate::proc;

/// The program and leading arguments a script string is passed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Shell {
    /// A login zsh, so the script sees the same environment as a Terminal window.
    pub fn login_zsh() -> Self {
        Self {
            program: PathBuf::from("/bin/zsh"),
            args: vec!["-lc".to_string()],
        }
    }
}

pub struct CommandTask {
    name: String,
    script: String,
    timeout: Duration,
    shell: Shell,
    path_var: String,
    workdir: PathBuf,
}

impl CommandTask {
    pub fn new(
        name: String,
        script: String,
        timeout: Duration,
        shell: Shell,
        path_var: String,
        workdir: PathBuf,
    ) -> Self {
        Self {
            name,
            script,
            timeout,
            shell,
            path_var,
            workdir,
        }
    }

    fn label(&self) -> PathBuf {
        PathBuf::from(format!("command: {}", self.name))
    }
}

impl CleanupTask for CommandTask {
    fn id(&self) -> &'static str {
        "command"
    }

    fn scan(&self, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        Ok(vec![Candidate {
            path: self.label(),
            bytes: 0,
            reason: "will run; space freed is known only after it runs".to_string(),
            estimate: Estimate::Unknown,
        }])
    }

    fn run(&self, ctx: &Ctx, candidates: &[Candidate]) -> Report {
        let mut report = Report::default();
        if candidates.is_empty() {
            return report;
        }
        let Some(first_root) = ctx.roots.first() else {
            report.halted = Some("no roots on this volume".to_string());
            return report;
        };
        let before = match disk::query(first_root) {
            Ok(s) => s.free_bytes,
            Err(e) => {
                report.halted = Some(format!("cannot read free space: {e}"));
                return report;
            }
        };
        let roots: Vec<String> = ctx.roots.iter().map(|r| r.display().to_string()).collect();
        let mut cmd = Command::new(&self.shell.program);
        cmd.args(&self.shell.args)
            .arg(&self.script)
            .current_dir(&self.workdir)
            .env("PATH", &self.path_var)
            .env("PICARD_ROOTS", roots.join(":"))
            .env("PICARD_DRY_RUN", "0")
            .env("PICARD_FREE_BYTES", before.to_string());

        log::info!("running command {:?}: {}", self.name, self.script);
        let out = match proc::run(cmd, self.timeout, proc::SCRIPT_OUTPUT_CAP) {
            Ok(o) => o,
            Err(e) => {
                report
                    .failed
                    .push((self.label(), format!("could not start: {e}")));
                return report;
            }
        };
        let after = disk::query(first_root).map_or(before, |s| s.free_bytes);
        let measured = after.saturating_sub(before);
        let freed = measured.max(reported_freed(&out.stdout).unwrap_or(0));
        if !out.stdout.trim().is_empty() {
            log::info!("command {:?} output: {}", self.name, out.stdout.trim());
        }
        if out.success() {
            report.removed.push(Removed {
                path: self.label(),
                bytes: freed,
            });
        } else {
            let why = out.failure_summary(self.timeout);
            log::error!("command {:?} failed: {why}", self.name);
            report.failed.push((self.label(), why));
        }
        report
    }
}

/// The last `PICARD_FREED=<bytes>` line in the script's output, if any.
fn reported_freed(stdout: &str) -> Option<u64> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("PICARD_FREED=")?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::*;

    fn task(fx: &Fixture, script: &str, timeout_secs: u64) -> CommandTask {
        CommandTask::new(
            "test".into(),
            script.into(),
            Duration::from_secs(timeout_secs),
            Shell {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".into()],
            },
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        )
    }

    fn run_task(fx: &Fixture, t: &CommandTask) -> Report {
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        t.run(&ctx, &found)
    }

    #[test]
    fn scan_never_runs_the_script() {
        let fx = Fixture::new(&[]);
        let marker = fx.dir.path().join("ran");
        let t = task(&fx, &format!("touch '{}'", marker.display()), 5);
        let found = t.scan(&fx.ctx()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bytes, 0);
        assert!(!marker.exists());
    }

    #[test]
    fn script_gets_environment_and_workdir() {
        let fx = Fixture::new(&[]);
        let out = fx.dir.path().join("env.txt");
        let script = format!(
            "{{ pwd -P; echo \"$PATH\"; echo \"$PICARD_ROOTS\"; echo \"$PICARD_DRY_RUN\"; echo \"$PICARD_FREE_BYTES\"; }} > '{}'",
            out.display()
        );
        let report = run_task(&fx, &task(&fx, &script, 5));
        assert_eq!(report.removed.len(), 1, "{report:?}");
        let text = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            fx.dir.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(lines[1], "/usr/bin:/bin");
        assert_eq!(lines[2], fx.root.canonicalize().unwrap().to_str().unwrap());
        assert_eq!(lines[3], "0");
        assert!(lines[4].parse::<u64>().unwrap() > 0);
    }

    #[test]
    fn reported_freed_counts_when_larger() {
        let fx = Fixture::new(&[]);
        let report = run_task(
            &fx,
            &task(&fx, "echo working; echo PICARD_FREED=5000000000", 5),
        );
        assert!(report.removed[0].bytes >= 5_000_000_000);
        assert_eq!(report.removed[0].path, PathBuf::from("command: test"));
    }

    #[test]
    fn failure_reports_exit_code_and_stderr() {
        let fx = Fixture::new(&[]);
        let report = run_task(&fx, &task(&fx, "echo 'docker not running' >&2; exit 2", 5));
        assert!(report.removed.is_empty());
        assert_eq!(report.failed[0].1, "exit 2: docker not running");
    }

    #[test]
    fn timeout_is_reported() {
        let fx = Fixture::new(&[]);
        let report = run_task(&fx, &task(&fx, "sleep 30", 1));
        assert_eq!(report.failed[0].1, "timed out after 1s");
    }

    #[test]
    fn parses_last_freed_line() {
        assert_eq!(
            reported_freed("PICARD_FREED=1\nx\nPICARD_FREED= 42 \n"),
            Some(42)
        );
        assert_eq!(reported_freed("PICARD_FREED=lots"), None);
        assert_eq!(reported_freed(""), None);
    }
}
