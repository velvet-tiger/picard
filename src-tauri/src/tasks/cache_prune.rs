//! Asks package managers to shrink their global caches.
//!
//! Optional (`prune_package_caches` on the `node_modules` task, off by default).
//! The caches live outside the roots, so Picard never deletes them itself: it runs
//! `npm cache clean --force` and `pnpm store prune` and lets each tool decide.
//! Space freed is measured from the volume's free space.

use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use super::{Ctx, Removed, Report};
use crate::disk;
use crate::proc;

const PRUNE_TIMEOUT: Duration = Duration::from_secs(300);
/// A step that fails with ENOTEMPTY runs once more after this pause. Another
/// process (a running npm, or one writing `.DS_Store` files) can add a file to
/// the cache while the tool deletes it, and a second pass usually succeeds.
const RETRY_DELAY: Duration = Duration::from_secs(2);
const RETRYABLE: &str = "ENOTEMPTY";

/// One cache-shrinking command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneStep {
    /// Shown in history and reports, for example "npm cache".
    pub label: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePrune {
    steps: Vec<PruneStep>,
    path_var: String,
    /// Working folder for each step. See `run_step`.
    cwd: PathBuf,
    timeout: Duration,
    retry_delay: Duration,
}

impl CachePrune {
    /// Builds the steps for whichever of npm and pnpm were found.
    pub fn new(
        npm: Option<PathBuf>,
        pnpm: Option<PathBuf>,
        path_var: String,
        home: PathBuf,
    ) -> Self {
        let mut steps = Vec::new();
        if let Some(program) = npm {
            steps.push(PruneStep {
                label: "npm cache".into(),
                program,
                args: vec!["cache".into(), "clean".into(), "--force".into()],
            });
        }
        if let Some(program) = pnpm {
            steps.push(PruneStep {
                label: "pnpm store".into(),
                program,
                args: vec!["store".into(), "prune".into()],
            });
        }
        Self {
            steps,
            path_var,
            cwd: home,
            timeout: PRUNE_TIMEOUT,
            retry_delay: RETRY_DELAY,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Runs each step, adding what it freed (or why it failed) to `report`.
    pub fn run(&self, ctx: &Ctx, report: &mut Report) {
        let Some(root) = ctx.roots.first() else {
            return;
        };
        for step in &self.steps {
            if !ctx.volume_present() {
                report.halted = Some("the volume is no longer mounted".to_string());
                return;
            }
            let before = disk::query(root).map(|s| s.free_bytes).unwrap_or(0);
            let label = PathBuf::from(&step.label);
            match self.run_with_retry(step) {
                Ok(()) => {
                    let after = disk::query(root).map_or(before, |s| s.free_bytes);
                    let freed = after.saturating_sub(before);
                    log::info!("{} freed {freed} bytes", step.label);
                    report.removed.push(Removed {
                        path: label,
                        bytes: freed,
                    });
                }
                Err(why) => {
                    log::error!("{} failed: {why}", step.label);
                    report.failed.push((label, why));
                }
            }
        }
    }

    /// Runs `step`, and once more if the first run failed with ENOTEMPTY.
    fn run_with_retry(&self, step: &PruneStep) -> Result<(), String> {
        let first = self.run_step(step);
        match &first {
            Err(why) if why.contains(RETRYABLE) => {
                log::warn!(
                    "{} failed ({why}), retrying in {}s",
                    step.label,
                    self.retry_delay.as_secs()
                );
                thread::sleep(self.retry_delay);
                self.run_step(step)
            }
            _ => first,
        }
    }

    /// Runs `step` once. The error is a short summary for history and reports.
    fn run_step(&self, step: &PruneStep) -> Result<(), String> {
        let mut cmd = Command::new(&step.program);
        // Run from the home folder. Not a root: npm and pnpm read `.npmrc` from
        // the working folder, and a folder under the roots may hold one Picard
        // did not write. Not `/` either: it is read-only on macOS, and pnpm
        // writes a temporary file in the working folder and picks its store by
        // that folder's volume.
        cmd.args(&step.args)
            .current_dir(&self.cwd)
            .env("PATH", &self.path_var);
        match proc::run(cmd, self.timeout, proc::SCRIPT_OUTPUT_CAP) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(out.failure_summary(self.timeout)),
            Err(e) => Err(format!("could not start: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake tool that records its arguments, then exits with `code`.
    fn fake(dir: &std::path::Path, name: &str, log: &std::path::Path, code: i32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\necho \"{name} $*\" >> '{}'\nexit {code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn runs_found_tools_with_the_right_arguments() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let npm = fake(fx.dir.path(), "npm", &log, 0);
        let pnpm = fake(fx.dir.path(), "pnpm", &log, 0);
        let prune = CachePrune::new(
            Some(npm),
            Some(pnpm),
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        );
        let mut report = Report::default();
        prune.run(&fx.ctx(), &mut report);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "npm cache clean --force\npnpm store prune\n"
        );
        let labels: Vec<_> = report.removed.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            labels,
            vec![PathBuf::from("npm cache"), PathBuf::from("pnpm store")]
        );
    }

    #[test]
    fn missing_tool_is_skipped_and_failure_is_reported() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let npm = fake(fx.dir.path(), "npm", &log, 1);
        let prune = CachePrune::new(
            Some(npm),
            None,
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        );
        let mut report = Report::default();
        prune.run(&fx.ctx(), &mut report);
        assert!(report.removed.is_empty());
        assert_eq!(report.failed[0].0, PathBuf::from("npm cache"));
        assert_eq!(report.failed[0].1, "exit 1");
        assert!(CachePrune::new(None, None, String::new(), PathBuf::from("/")).is_empty());
    }

    /// A fake tool that writes `body` as its script after the shebang.
    fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn runs_in_the_home_folder() {
        let fx = Fixture::new(&[]);
        let home = fx.dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let log = fx.dir.path().join("cwd.log");
        let pnpm = script(
            fx.dir.path(),
            "pnpm",
            &format!("pwd -P > '{}'", log.display()),
        );
        let prune = CachePrune::new(None, Some(pnpm), "/usr/bin:/bin".into(), home.clone());
        let mut report = Report::default();
        prune.run(&fx.ctx(), &mut report);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            home.canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn retries_once_after_enotempty() {
        let fx = Fixture::new(&[]);
        let marker = fx.dir.path().join("ran-once");
        // Fails with ENOTEMPTY the first time, succeeds the second.
        let npm = script(
            fx.dir.path(),
            "npm",
            &format!(
                "if [ -e '{m}' ]; then exit 0; fi\ntouch '{m}'\necho 'npm error ENOTEMPTY: directory not empty' >&2\nexit 190",
                m = marker.display()
            ),
        );
        let mut prune = CachePrune::new(
            Some(npm),
            None,
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        );
        prune.retry_delay = Duration::ZERO;
        let mut report = Report::default();
        prune.run(&fx.ctx(), &mut report);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.removed.len(), 1);
    }

    #[test]
    fn other_failures_are_not_retried() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let npm = script(
            fx.dir.path(),
            "npm",
            &format!(
                "echo run >> '{}'\necho 'npm error EACCES' >&2\nexit 1",
                log.display()
            ),
        );
        let mut prune = CachePrune::new(
            Some(npm),
            None,
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        );
        prune.retry_delay = Duration::ZERO;
        let mut report = Report::default();
        prune.run(&fx.ctx(), &mut report);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "run\n");
        assert_eq!(report.failed[0].1, "exit 1: npm error EACCES");
    }
}
