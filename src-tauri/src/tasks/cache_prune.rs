//! Asks tools to shrink their own caches.
//!
//! Used in two places. `prune_package_caches` on the `node_modules` task runs
//! npm and pnpm, and the `tool_caches` task runs uv, Homebrew, simctl and bun.
//! The caches live outside the roots, so Picard never deletes them itself: it
//! runs each tool's own prune command and lets the tool decide.
//!
//! A step runs only when its cache sits on the volume being cleaned. Pruning a
//! cache on another drive frees nothing where space is short. Space freed is
//! measured from the volume's free space.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use super::{Ctx, Removed, Report};
use crate::disk;
use crate::proc;

const PRUNE_TIMEOUT: Duration = Duration::from_secs(300);
/// Asking a tool where its cache is should be quick. A tool that hangs here is
/// treated as a failure rather than holding up the scan.
const LOCATE_TIMEOUT: Duration = Duration::from_secs(30);
/// A step that fails with ENOTEMPTY runs once more after this pause. Another
/// process (a running npm, or one writing `.DS_Store` files) can add a file to
/// the cache while the tool deletes it, and a second pass usually succeeds.
const RETRY_DELAY: Duration = Duration::from_secs(2);
const RETRYABLE: &str = "ENOTEMPTY";

/// Where a tool keeps its cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDir {
    /// A folder that does not depend on the tool's settings.
    Fixed(PathBuf),
    /// Running the tool with these arguments prints the folder. Asking the
    /// tool follows its own settings, such as `UV_CACHE_DIR` or an `.npmrc`.
    Ask(Vec<String>),
}

/// One cache-shrinking command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneStep {
    /// Shown in previews, history and reports, for example "npm cache".
    pub label: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cache: CacheDir,
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

impl PruneStep {
    fn new(label: &str, program: PathBuf, args: &[&str], cache: CacheDir) -> Self {
        Self {
            label: label.to_string(),
            program,
            args: strings(args),
            cache,
        }
    }

    pub fn npm(program: PathBuf) -> Self {
        let cache = CacheDir::Ask(strings(&["config", "get", "cache"]));
        Self::new("npm cache", program, &["cache", "clean", "--force"], cache)
    }

    pub fn pnpm(program: PathBuf) -> Self {
        let cache = CacheDir::Ask(strings(&["store", "path"]));
        Self::new("pnpm store", program, &["store", "prune"], cache)
    }

    /// Removes cache entries no installed environment can reach.
    pub fn uv(program: PathBuf) -> Self {
        let cache = CacheDir::Ask(strings(&["cache", "dir"]));
        Self::new("uv cache", program, &["cache", "prune"], cache)
    }

    /// Removes downloads and old versions of installed formulae and casks.
    pub fn homebrew(program: PathBuf) -> Self {
        let cache = CacheDir::Ask(strings(&["--cache"]));
        Self::new(
            "Homebrew cache",
            program,
            &["cleanup", "--prune=all"],
            cache,
        )
    }

    /// Deletes simulators whose runtime the installed Xcode no longer supports.
    pub fn simulators(xcrun: PathBuf, home: &Path) -> Self {
        let cache = CacheDir::Fixed(home.join("Library/Developer/CoreSimulator/Devices"));
        Self::new(
            "iOS Simulators",
            xcrun,
            &["simctl", "delete", "unavailable"],
            cache,
        )
    }

    pub fn bun(program: PathBuf) -> Self {
        let cache = CacheDir::Ask(strings(&["pm", "cache"]));
        Self::new("bun cache", program, &["pm", "cache", "rm"], cache)
    }

    /// The command as a user would type it, for preview reasons.
    pub fn command_text(&self) -> String {
        let name = self.program.file_name().map_or_else(
            || self.program.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        std::iter::once(name)
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Where a step's cache sits relative to the volume being cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Here(PathBuf),
    /// On another volume. Pruning it would free nothing here.
    Elsewhere(PathBuf),
    /// Not created yet, so there is nothing to prune.
    Missing(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePrune {
    steps: Vec<PruneStep>,
    path_var: String,
    /// Working folder for each step. See `command`.
    cwd: PathBuf,
    timeout: Duration,
    retry_delay: Duration,
}

impl CachePrune {
    pub fn new(steps: Vec<PruneStep>, path_var: String, home: PathBuf) -> Self {
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

    /// The steps whose cache is on this volume, with each cache folder.
    ///
    /// Runs each tool's location query but changes nothing. A step whose cache
    /// cannot be found is logged and left out, so it never runs.
    pub fn steps_here(&self, ctx: &Ctx) -> Vec<(&PruneStep, PathBuf)> {
        self.steps
            .iter()
            .filter_map(|step| match self.placement(step, ctx.dev) {
                Ok(Placement::Here(dir)) => Some((step, dir)),
                Ok(other) => {
                    log::info!("{}: not on this volume ({other:?})", step.label);
                    None
                }
                Err(why) => {
                    log::warn!("{}: {why}", step.label);
                    None
                }
            })
            .collect()
    }

    /// Runs every step whose cache is on this volume.
    pub fn run(&self, ctx: &Ctx, report: &mut Report) {
        self.run_matching(ctx, report, |_| true);
    }

    /// Runs the steps named in `labels` whose cache is on this volume.
    pub fn run_only(&self, ctx: &Ctx, labels: &[PathBuf], report: &mut Report) {
        self.run_matching(ctx, report, |step| {
            labels.iter().any(|l| l.as_os_str() == step.label.as_str())
        });
    }

    /// Runs each chosen step, adding what it freed (or why it failed) to `report`.
    fn run_matching(&self, ctx: &Ctx, report: &mut Report, chosen: impl Fn(&PruneStep) -> bool) {
        let Some(root) = ctx.roots.first() else {
            return;
        };
        for step in self.steps.iter().filter(|s| chosen(s)) {
            if !ctx.volume_present() {
                report.halted = Some("the volume is no longer mounted".to_string());
                return;
            }
            let label = PathBuf::from(&step.label);
            // Checked again at run time: a preview may be minutes old, and
            // the user can move a cache in between.
            match self.placement(step, ctx.dev) {
                Ok(Placement::Here(_)) => {}
                Ok(other) => {
                    log::info!("{}: skipped, not on this volume ({other:?})", step.label);
                    continue;
                }
                Err(why) => {
                    log::error!("{}: {why}", step.label);
                    report.failed.push((label, why));
                    continue;
                }
            }
            let before = disk::query(root).map(|s| s.free_bytes).unwrap_or(0);
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

    /// Finds `step`'s cache and compares its device with `dev`. Only reads.
    fn placement(&self, step: &PruneStep, dev: u64) -> Result<Placement, String> {
        let dir = match &step.cache {
            CacheDir::Fixed(dir) => dir.clone(),
            CacheDir::Ask(args) => self.ask_cache_dir(step, args)?,
        };
        match std::fs::metadata(&dir) {
            Ok(m) if m.dev() == dev => Ok(Placement::Here(dir)),
            Ok(_) => Ok(Placement::Elsewhere(dir)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Placement::Missing(dir)),
            Err(e) => Err(format!("cannot read its cache at {}: {e}", dir.display())),
        }
    }

    /// Runs the tool's location query and parses the folder it prints.
    fn ask_cache_dir(&self, step: &PruneStep, args: &[String]) -> Result<PathBuf, String> {
        let mut cmd = self.command(&step.program);
        cmd.args(args);
        let out = proc::run(cmd, LOCATE_TIMEOUT, proc::SCRIPT_OUTPUT_CAP)
            .map_err(|e| format!("could not ask where its cache is: {e}"))?;
        if !out.success() {
            return Err(format!(
                "could not ask where its cache is: {}",
                out.failure_summary(LOCATE_TIMEOUT)
            ));
        }
        parse_cache_dir(&out.stdout)
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
        let mut cmd = self.command(&step.program);
        cmd.args(&step.args);
        match proc::run(cmd, self.timeout, proc::SCRIPT_OUTPUT_CAP) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(out.failure_summary(self.timeout)),
            Err(e) => Err(format!("could not start: {e}")),
        }
    }

    fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        // Run from the home folder. Not a root: npm and pnpm read `.npmrc` from
        // the working folder, and a folder under the roots may hold one Picard
        // did not write. Not `/` either: it is read-only on macOS, and pnpm
        // writes a temporary file in the working folder and picks its store by
        // that folder's volume.
        cmd.current_dir(&self.cwd).env("PATH", &self.path_var);
        cmd
    }
}

/// The folder a location query printed: its last non-empty line, which must be
/// an absolute path. Some tools print notices before the answer.
fn parse_cache_dir(stdout: &str) -> Result<PathBuf, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or_else(|| "printed no cache folder".to_string())?;
    let dir = PathBuf::from(line);
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(format!("printed {line:?}, which is not an absolute path"))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A fake tool. Called with `locate` as its arguments, it prints
    /// `cache_dir`. Called any other way, it appends `<name> <args>` to `log`,
    /// then runs `body` (which decides the exit status).
    pub fn fake_tool(
        dir: &Path,
        name: &str,
        locate: &str,
        cache_dir: &Path,
        log: &Path,
        body: &str,
    ) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nif [ \"$*\" = '{locate}' ]; then echo '{cache}'; exit 0; fi\necho \"{name} $*\" >> '{log}'\n{body}\n",
                cache = cache_dir.display(),
                log = log.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::Fixture;
    use super::test_support::fake_tool;
    use super::*;

    /// A cache folder on the fixture's volume.
    fn cache(fx: &Fixture, name: &str) -> PathBuf {
        let d = fx.dir.path().join("caches").join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn npm(fx: &Fixture, log: &Path, body: &str) -> PathBuf {
        fake_tool(
            fx.dir.path(),
            "npm",
            "config get cache",
            &cache(fx, "npm"),
            log,
            body,
        )
    }

    fn pnpm(fx: &Fixture, log: &Path, body: &str) -> PathBuf {
        fake_tool(
            fx.dir.path(),
            "pnpm",
            "store path",
            &cache(fx, "pnpm"),
            log,
            body,
        )
    }

    fn prune(steps: Vec<PruneStep>, fx: &Fixture) -> CachePrune {
        let mut p = CachePrune::new(steps, "/usr/bin:/bin".into(), fx.dir.path().to_path_buf());
        p.retry_delay = Duration::ZERO;
        p
    }

    #[test]
    fn runs_found_tools_with_the_right_arguments() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let steps = vec![
            PruneStep::npm(npm(&fx, &log, "exit 0")),
            PruneStep::pnpm(pnpm(&fx, &log, "exit 0")),
        ];
        let mut report = Report::default();
        prune(steps, &fx).run(&fx.ctx(), &mut report);
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
    fn a_failing_step_is_reported() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let mut report = Report::default();
        prune(vec![PruneStep::npm(npm(&fx, &log, "exit 1"))], &fx).run(&fx.ctx(), &mut report);
        assert!(report.removed.is_empty());
        assert_eq!(report.failed[0].0, PathBuf::from("npm cache"));
        assert_eq!(report.failed[0].1, "exit 1");
        assert!(CachePrune::new(vec![], String::new(), PathBuf::from("/")).is_empty());
    }

    #[test]
    fn runs_in_the_home_folder() {
        let fx = Fixture::new(&[]);
        let home = fx.dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let log = fx.dir.path().join("calls.log");
        let cwd = fx.dir.path().join("cwd.log");
        let pnpm = pnpm(&fx, &log, &format!("pwd -P > '{}'", cwd.display()));
        let p = CachePrune::new(
            vec![PruneStep::pnpm(pnpm)],
            "/usr/bin:/bin".into(),
            home.clone(),
        );
        let mut report = Report::default();
        p.run(&fx.ctx(), &mut report);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(
            std::fs::read_to_string(&cwd).unwrap().trim(),
            home.canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn retries_once_after_enotempty() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let marker = fx.dir.path().join("ran-once");
        // Fails with ENOTEMPTY the first time, succeeds the second.
        let npm = npm(
            &fx,
            &log,
            &format!(
                "if [ -e '{m}' ]; then exit 0; fi\ntouch '{m}'\necho 'npm error ENOTEMPTY: directory not empty' >&2\nexit 190",
                m = marker.display()
            ),
        );
        let mut report = Report::default();
        prune(vec![PruneStep::npm(npm)], &fx).run(&fx.ctx(), &mut report);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.removed.len(), 1);
    }

    #[test]
    fn other_failures_are_not_retried() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let npm = npm(&fx, &log, "echo 'npm error EACCES' >&2\nexit 1");
        let mut report = Report::default();
        prune(vec![PruneStep::npm(npm)], &fx).run(&fx.ctx(), &mut report);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "npm cache clean --force\n"
        );
        assert_eq!(report.failed[0].1, "exit 1: npm error EACCES");
    }

    #[test]
    fn a_cache_on_another_volume_is_not_pruned() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let ctx = fx.ctx();
        let p = prune(vec![PruneStep::npm(npm(&fx, &log, "exit 0"))], &fx);
        assert_eq!(p.steps_here(&ctx).len(), 1);
        // The same cache, judged against a volume on another device.
        let placement = p.placement(&p.steps[0], ctx.dev.wrapping_add(1)).unwrap();
        assert!(
            matches!(placement, Placement::Elsewhere(_)),
            "{placement:?}"
        );
    }

    #[test]
    fn a_missing_cache_is_not_pruned() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let gone = fx.dir.path().join("never-created");
        let npm = fake_tool(
            fx.dir.path(),
            "npm",
            "config get cache",
            &gone,
            &log,
            "exit 0",
        );
        let p = prune(vec![PruneStep::npm(npm)], &fx);
        assert!(p.steps_here(&fx.ctx()).is_empty());
        let mut report = Report::default();
        p.run(&fx.ctx(), &mut report);
        assert!(report.removed.is_empty() && report.failed.is_empty());
        assert!(!log.exists(), "the prune command must not run");
    }

    #[test]
    fn a_tool_that_cannot_say_where_its_cache_is_does_not_run() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        // Answers the location query with a relative path.
        let npm = fake_tool(
            fx.dir.path(),
            "npm",
            "config get cache",
            Path::new("relative/cache"),
            &log,
            "exit 0",
        );
        let p = prune(vec![PruneStep::npm(npm)], &fx);
        assert!(p.steps_here(&fx.ctx()).is_empty());
        let mut report = Report::default();
        p.run(&fx.ctx(), &mut report);
        assert!(
            report.failed[0].1.contains("not an absolute path"),
            "{:?}",
            report.failed
        );
        assert!(!log.exists(), "the prune command must not run");
    }

    #[test]
    fn run_only_runs_the_chosen_steps() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let steps = vec![
            PruneStep::npm(npm(&fx, &log, "exit 0")),
            PruneStep::pnpm(pnpm(&fx, &log, "exit 0")),
        ];
        let mut report = Report::default();
        prune(steps, &fx).run_only(&fx.ctx(), &[PathBuf::from("pnpm store")], &mut report);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "pnpm store prune\n");
    }

    #[test]
    fn parses_the_last_line_as_the_cache_folder() {
        assert_eq!(
            parse_cache_dir("notice: update available\n/Users/me/.npm\n\n"),
            Ok(PathBuf::from("/Users/me/.npm"))
        );
        assert!(parse_cache_dir("").is_err());
        assert!(parse_cache_dir("~/.npm\n").is_err());
    }

    #[test]
    fn command_text_names_the_tool_and_its_arguments() {
        let step = PruneStep::uv(PathBuf::from("/opt/homebrew/bin/uv"));
        assert_eq!(step.command_text(), "uv cache prune");
        let step = PruneStep::simulators(PathBuf::from("/usr/bin/xcrun"), Path::new("/Users/me"));
        assert_eq!(step.command_text(), "xcrun simctl delete unavailable");
        assert_eq!(
            step.cache,
            CacheDir::Fixed(PathBuf::from(
                "/Users/me/Library/Developer/CoreSimulator/Devices"
            ))
        );
    }
}
