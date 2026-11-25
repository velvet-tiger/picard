//! Asks developer tools to prune their own caches: uv, Homebrew, the iOS
//! Simulator, bun and Go.
//!
//! Off unless added to the config, because it runs other programs. Each tool
//! decides what to remove, so `scan` cannot know how much a prune will free.
//! Preview counts each step as 0 bytes and shows the cache's current size as
//! a ceiling ("up to 2.6 GB"). Counting the whole cache would overstate the
//! total, since a prune keeps whatever is still in use.

use anyhow::Result;

use super::cache_prune::CachePrune;
use super::{Candidate, CleanupTask, Ctx, Estimate, Report};

pub struct ToolCaches {
    prune: CachePrune,
}

impl ToolCaches {
    pub fn new(prune: CachePrune) -> Self {
        Self { prune }
    }
}

impl CleanupTask for ToolCaches {
    fn id(&self) -> &'static str {
        "tool_caches"
    }

    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>> {
        let here = self.prune.steps_here(ctx);
        let dirs: Vec<_> = here.iter().map(|(_, dir)| dir.clone()).collect();
        let sizes = ctx.measure_all(&dirs);
        Ok(here
            .into_iter()
            .zip(sizes)
            .map(|((step, _), size)| Candidate {
                path: step.label.clone().into(),
                bytes: 0,
                reason: format!("runs {}", step.command_text()),
                estimate: Estimate::UpTo(size),
            })
            .collect())
    }

    fn run(&self, ctx: &Ctx, candidates: &[Candidate]) -> Report {
        let labels: Vec<_> = candidates.iter().map(|c| c.path.clone()).collect();
        let mut report = Report::default();
        if !labels.is_empty() {
            self.prune.run_only(ctx, &labels, &mut report);
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache_prune::PruneStep;
    use super::super::cache_prune::test_support::fake_tool;
    use super::super::test_support::Fixture;
    use super::*;
    use std::path::PathBuf;

    fn task(fx: &Fixture, log: &std::path::Path) -> ToolCaches {
        let cache = fx.dir.path().join("uv-cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("wheel"), vec![b'x'; 40_000]).unwrap();
        let uv = fake_tool(fx.dir.path(), "uv", "cache dir", &cache, log, "exit 0");
        let brew_cache = fx.dir.path().join("brew-cache");
        std::fs::create_dir_all(&brew_cache).unwrap();
        let brew = fake_tool(fx.dir.path(), "brew", "--cache", &brew_cache, log, "exit 0");
        ToolCaches::new(CachePrune::new(
            vec![PruneStep::uv(uv), PruneStep::homebrew(brew)],
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        ))
    }

    #[test]
    fn scan_lists_each_step_without_running_it() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let found = task(&fx, &log).scan(&fx.ctx()).unwrap();
        let labels: Vec<_> = found.iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            labels,
            vec![PathBuf::from("uv cache"), PathBuf::from("Homebrew cache")]
        );
        assert!(found.iter().all(|c| c.bytes == 0));
        assert_eq!(found[0].reason, "runs uv cache prune");
        assert_eq!(found[1].reason, "runs brew cleanup --prune=all");
        match found[0].estimate {
            Estimate::UpTo(ceiling) => assert!(ceiling >= 40_000, "{ceiling}"),
            other => panic!("uv cache should be a ceiling, got {other:?}"),
        }
        assert_eq!(found[1].estimate, Estimate::UpTo(0));
        assert!(!log.exists(), "scan must not run a prune");
    }

    #[test]
    fn run_prunes_only_the_selected_caches() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let t = task(&fx, &log);
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        let report = t.run(&ctx, &found[1..]);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "brew cleanup --prune=all\n"
        );
        assert_eq!(report.removed[0].path, PathBuf::from("Homebrew cache"));
        assert!(t.run(&ctx, &[]).removed.is_empty());
    }

    /// A fake `go` that reports a different folder for each `go env` query,
    /// as the real one does for GOCACHE and GOMODCACHE.
    fn fake_go(
        dir: &std::path::Path,
        build: &std::path::Path,
        modules: &std::path::Path,
        log: &std::path::Path,
    ) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("go");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh
case \"$*\" in
'env GOCACHE') echo '{}'; exit 0;;
'env GOMODCACHE') echo '{}'; exit 0;;
esac
echo \"go $*\" >> '{}'
",
                build.display(),
                modules.display(),
                log.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn go_steps_find_their_own_caches_and_run_separately() {
        let fx = Fixture::new(&[]);
        let log = fx.dir.path().join("calls.log");
        let build = fx.dir.path().join("go-build");
        let modules = fx.dir.path().join("go-mod");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(modules.join("module.zip"), vec![b'x'; 40_000]).unwrap();
        let go = fake_go(fx.dir.path(), &build, &modules, &log);
        let t = ToolCaches::new(CachePrune::new(
            vec![PruneStep::go_build(go.clone()), PruneStep::go_modules(go)],
            "/usr/bin:/bin".into(),
            fx.dir.path().to_path_buf(),
        ));
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found[0].reason, "runs go clean -cache");
        assert_eq!(found[1].reason, "runs go clean -modcache");
        assert_eq!(found[0].estimate, Estimate::UpTo(0));
        match found[1].estimate {
            Estimate::UpTo(ceiling) => assert!(ceiling >= 40_000, "{ceiling}"),
            other => panic!("module cache should be a ceiling, got {other:?}"),
        }
        assert!(!log.exists(), "scan must not run a prune");

        let report = t.run(&ctx, &found[..1]);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "go clean -cache\n");
        assert_eq!(report.removed[0].path, PathBuf::from("Go build cache"));
    }
}
