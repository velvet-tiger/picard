//! Asks package managers to shrink their global caches.
//!
//! Optional (`prune_package_caches` on the `node_modules` task, off by default).
//! The caches live outside the roots, so Picard never deletes them itself: it runs
//! `npm cache clean --force` and `pnpm store prune` and lets each tool decide.
//! Space freed is measured from the volume's free space.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::{Ctx, Removed, Report};
use crate::disk;
use crate::proc;

const PRUNE_TIMEOUT: Duration = Duration::from_secs(300);

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
    timeout: Duration,
}

impl CachePrune {
    /// Builds the steps for whichever of npm and pnpm were found.
    pub fn new(npm: Option<PathBuf>, pnpm: Option<PathBuf>, path_var: String) -> Self {
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
            timeout: PRUNE_TIMEOUT,
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
            let mut cmd = Command::new(&step.program);
            // Run from `/`, not a root: npm and pnpm read `.npmrc` from the
            // working folder, and a folder under the roots may hold one Picard
            // did not write.
            cmd.args(&step.args)
                .current_dir("/")
                .env("PATH", &self.path_var);
            let label = PathBuf::from(&step.label);
            match proc::run(cmd, self.timeout, proc::SCRIPT_OUTPUT_CAP) {
                Ok(out) if out.success() => {
                    let after = disk::query(root).map_or(before, |s| s.free_bytes);
                    let freed = after.saturating_sub(before);
                    log::info!("{} freed {freed} bytes", step.label);
                    report.removed.push(Removed {
                        path: label,
                        bytes: freed,
                    });
                }
                Ok(out) => {
                    let why = out.failure_summary(self.timeout);
                    log::error!("{} failed: {why}", step.label);
                    report.failed.push((label, why));
                }
                Err(e) => {
                    log::error!("{} could not start: {e}", step.label);
                    report.failed.push((label, format!("could not start: {e}")));
                }
            }
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
        let prune = CachePrune::new(Some(npm), Some(pnpm), "/usr/bin:/bin".into());
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
        let prune = CachePrune::new(Some(npm), None, "/usr/bin:/bin".into());
        let mut report = Report::default();
        prune.run(&fx.ctx(), &mut report);
        assert!(report.removed.is_empty());
        assert_eq!(report.failed[0].0, PathBuf::from("npm cache"));
        assert_eq!(report.failed[0].1, "exit 1");
        assert!(CachePrune::new(None, None, String::new()).is_empty());
    }
}
