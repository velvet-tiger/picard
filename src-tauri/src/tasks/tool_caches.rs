//! Asks developer tools to prune their own caches: uv, Homebrew, the iOS
//! Simulator and bun.
//!
//! Off unless added to the config, because it runs other programs. Each tool
//! decides what to remove, so `scan` cannot know how much a prune will free.
//! Preview lists each step with 0 bytes, and the reason shows the cache's
//! current size. Counting the whole cache would overstate the estimate, since
//! a prune keeps whatever is still in use.

use anyhow::Result;

use super::cache_prune::CachePrune;
use super::{Candidate, CleanupTask, Ctx, Report};
use crate::disk::format_bytes;

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
        let sizes = ctx.sizes.measure_all(&dirs);
        Ok(here
            .into_iter()
            .zip(sizes)
            .map(|((step, _), size)| Candidate {
                path: step.label.clone().into(),
                bytes: 0,
                reason: format!(
                    "runs {}; its folder uses {} now",
                    step.command_text(),
                    format_bytes(size)
                ),
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
        assert_eq!(
            found[0].reason,
            "runs uv cache prune; its folder uses 0 MB now"
        );
        assert_eq!(
            found[1].reason,
            "runs brew cleanup --prune=all; its folder uses 0 MB now"
        );
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
}
