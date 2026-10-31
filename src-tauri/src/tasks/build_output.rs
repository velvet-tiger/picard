//! Removes build output folders, such as `dist`, `build`, `.next` and
//! `vendor`, from projects nobody has touched lately.
//!
//! Those names are not always build output. Some repos commit `dist`, Go
//! projects often commit `vendor`, and `build` is sometimes source. So a
//! folder is a candidate only when git vouches for it: git must ignore the
//! folder and track nothing inside it.
//!
//! Outside any git repo, only a file the tool itself writes can vouch for a
//! folder. `.next` must hold `BUILD_ID`, and `vendor` must hold
//! `composer/installed.json`. Every other name is kept there, including `dist`
//! and `build`, because nothing shows what is in them. When git fails for any
//! other reason, the folder is kept too.
//!
//! A folder that holds a git repo of its own is kept, in or out of a repo,
//! because that repo may hold work that exists nowhere else. The
//! exception is a clean clone that a package manager installed from source,
//! such as Composer's `--prefer-source` installs, whose commits are all on its
//! remote branches. `GitCmd::nested_checkout_problem` lists the conditions.
//!
//! Each folder is judged again just before it is removed.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::git::{GitCmd, output_git_dirs};
use super::git_worktrees::written_since;
use super::stale_dirs::{find_pairs, idle_days, is_file};
use super::{Candidate, CleanupTask, Ctx, Estimate, Report, Stop, check_target, remove_dirs};

/// How deep to search under each root for projects.
const SEARCH_DEPTH: usize = 8;

/// The folder names that can be vouched for outside a git repo: the name, the
/// file inside it that only the tool writes, and what that file shows.
const OUTSIDE_A_REPO: &[(&str, &[&str], &str)] = &[
    (".next", &["BUILD_ID"], "Next.js build"),
    (
        "vendor",
        &["composer", "installed.json"],
        "Composer packages",
    ),
];

/// What shows that a folder is regenerable build output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Voucher {
    /// Git ignores it and tracks nothing in it.
    Git,
    /// It is outside any repo, and holds a file only this tool writes.
    ToolFile(&'static str),
}

impl Voucher {
    /// The candidate reason for a folder with this voucher.
    fn reason(self, idle: u64) -> String {
        match self {
            Voucher::Git => format!("ignored by git, untouched for {idle} days"),
            Voucher::ToolFile(what) => {
                format!("{what}, not in a git repo, untouched for {idle} days")
            }
        }
    }
}

/// True if the path `parts` below `dir` is a regular file reached through real
/// folders. A symlink anywhere on the way could point at something else.
fn holds_file(dir: &Path, parts: &[&str]) -> bool {
    let Some((file, folders)) = parts.split_last() else {
        return false;
    };
    let mut at = dir.to_path_buf();
    for folder in folders {
        at.push(folder);
        if !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
            return false;
        }
    }
    is_file(&at.join(file))
}

pub struct BuildOutput {
    git: GitCmd,
    dirs: Vec<String>,
    stale_days: u32,
}

impl BuildOutput {
    /// `dirs` are plain folder names. Config validation has checked them.
    pub fn new(git: GitCmd, dirs: Vec<String>, stale_days: u32) -> Self {
        Self {
            git,
            dirs,
            stale_days,
        }
    }

    /// What vouches for `project/name`, or why it must be kept. Only reads.
    fn judge(&self, project: &Path, name: &str, stop: Stop) -> Result<Voucher, String> {
        let voucher = match self.git.in_repo(project) {
            Ok(true) => self.git_vouches(project, name)?,
            Ok(false) => tool_file_vouches(project, name)?,
            Err(e) => return Err(format!("{e:#}")),
        };
        match self
            .git
            .output_checkout_problem(project, &project.join(name), stop)
        {
            Ok(None) => Ok(voucher),
            Ok(Some((repo, why))) => Err(format!("holds a git repo at {}: {why}", repo.display())),
            Err(e) => Err(format!("cannot check it for git repos: {e:#}")),
        }
    }

    /// Checks that git ignores `project/name` and tracks nothing in it.
    ///
    /// `check-ignore` and `ls-files` read the index and ignore files. Neither
    /// runs filters or hooks, and `GitCmd` turns off fsmonitor.
    ///
    /// Both must read `name` as that one folder. Git reads its arguments as
    /// pathspecs, where `*` and a leading `:` have meaning. Config validation
    /// refuses names with those characters. As a second guard, `ls-files` gets
    /// `--literal-pathspecs`, and the `./` prefix stops a leading `:` acting as
    /// magic for `check-ignore`, which refuses that flag.
    fn git_vouches(&self, project: &Path, name: &str) -> Result<Voucher, String> {
        let path = format!("./{name}");
        match self
            .git
            .check(project, &["check-ignore", "-q", "--", &path])
        {
            Ok(true) => {}
            Ok(false) => return Err("git does not ignore it".to_string()),
            Err(e) => return Err(format!("cannot ask git about it: {e:#}")),
        }
        match self.git.output(
            project,
            &["--literal-pathspecs", "ls-files", "-z", "--", name],
        ) {
            Ok(tracked) if tracked.is_empty() => Ok(Voucher::Git),
            Ok(_) => Err("git tracks files inside it".to_string()),
            Err(e) => Err(format!("cannot list its tracked files: {e:#}")),
        }
    }

    /// Judges candidate `c` again. A clone in it can have gained work since
    /// the scan.
    fn judge_again(&self, c: &Candidate, stop: Stop) -> Result<Voucher, String> {
        let (Some(project), Some(name)) = (c.path.parent(), c.path.file_name()) else {
            return Err("has no parent folder".to_string());
        };
        let Some(name) = name.to_str() else {
            return Err("has a name that is not UTF-8".to_string());
        };
        self.judge(project, name, stop)
    }
}

/// True if a clone in candidate `c` changed after the run started, or if that
/// cannot be told. A commit writes only to `.git`, and the check just before
/// this one can have passed a moment too soon to see it.
fn clone_written_during_run(c: &Candidate, ctx: &Ctx) -> bool {
    match output_git_dirs(&c.path) {
        Ok(dirs) => dirs.iter().any(|dir| written_since(dir, ctx.now, false)),
        Err(e) => {
            log::warn!("{}: {e:#}", c.path.display());
            true
        }
    }
}

/// Checks that `project/name`, outside any git repo, holds the file that shows
/// what it is.
fn tool_file_vouches(project: &Path, name: &str) -> Result<Voucher, String> {
    OUTSIDE_A_REPO
        .iter()
        .find(|(known, parts, _)| *known == name && holds_file(&project.join(name), parts))
        .map(|(_, _, what)| Voucher::ToolFile(what))
        .ok_or_else(|| "outside any git repo, and nothing shows it is build output".to_string())
}

impl CleanupTask for BuildOutput {
    fn id(&self) -> &'static str {
        "build_output"
    }

    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>> {
        let names: Vec<&str> = self.dirs.iter().map(String::as_str).collect();
        let mut found = Vec::new();
        for (project, target) in find_pairs(ctx, &[], &names, SEARCH_DEPTH) {
            if ctx.stopped() {
                break;
            }
            if let Err(why) = check_target(&target, ctx) {
                log::debug!("build_output: skipping {}: {why}", target.display());
                continue;
            }
            // Staleness first: it is cheap, and it spares running git for
            // every project that is still in use.
            let idle = match idle_days(&project, ctx, &names) {
                Some(idle) if idle >= u64::from(self.stale_days) => idle,
                _ => continue,
            };
            let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            match self.judge(&project, name, ctx.stop) {
                Ok(voucher) => found.push((target, voucher.reason(idle))),
                Err(why) => log::debug!("build_output: keeping {}: {why}", target.display()),
            }
        }
        // Per-folder reasons are at debug level. With many projects they
        // would otherwise flood the log on every scan.
        log::info!("build_output: {} candidate(s)", found.len());
        let paths: Vec<PathBuf> = found.iter().map(|(p, _)| p.clone()).collect();
        let sizes = ctx.measure_all(&paths);
        Ok(found
            .into_iter()
            .zip(sizes)
            .map(|((path, reason), bytes)| Candidate {
                path,
                bytes,
                reason,
                estimate: Estimate::Exact,
            })
            .collect())
    }

    fn run(&self, ctx: &Ctx, candidates: &[Candidate]) -> Report {
        let mut report = Report::default();
        for c in candidates {
            if !ctx.volume_present() {
                report.halted = Some("the volume is no longer mounted".to_string());
                break;
            }
            let why = match self.judge_again(c, ctx.stop) {
                Err(why) => Some(why),
                Ok(_) if clone_written_during_run(c, ctx) => {
                    Some("a repo in it changed since the run started".to_string())
                }
                Ok(_) => None,
            };
            if let Some(why) = why {
                log::warn!("not removing {}: {why}", c.path.display());
                report.failed.push((c.path.clone(), why));
                continue;
            }
            // One at a time, so each folder is removed right after its check.
            let done = remove_dirs(ctx, std::slice::from_ref(c));
            report.removed.extend(done.removed);
            report.failed.extend(done.failed);
            report.halted = done.halted;
            if report.halted.is_some() {
                break;
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::super::NEVER_STOP;
    use super::super::test_support::{Fixture, git_with_hook};
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            // A commit otherwise starts a detached `git maintenance` that can
            // add and remove files in .git while Fixture::age walks the repo.
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        GitCmd::default().output(dir, args).unwrap()
    }

    fn task() -> BuildOutput {
        BuildOutput::new(
            GitCmd::default(),
            ["dist", "build", ".next", "vendor"]
                .map(String::from)
                .to_vec(),
            30,
        )
    }

    fn names(c: &[Candidate], root: &Path) -> Vec<String> {
        let root = root.canonicalize().unwrap();
        let mut v: Vec<String> = c
            .iter()
            .map(|c| c.path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        v.sort();
        v
    }

    /// A git repo at `rel` that ignores `dist/` and `build/`, with a commit.
    fn repo(fx: &Fixture, rel: &str) -> PathBuf {
        fx.file(&format!("{rel}/src/index.js"), 10);
        let dir = fx.root.join(rel);
        std::fs::write(dir.join(".gitignore"), "dist/\nbuild/\n").unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn takes_ignored_output_from_stale_projects_only() {
        let fx = Fixture::new(&[]);
        repo(&fx, "old");
        fx.file("old/dist/app.js", 20_000);
        fx.file("old/build/out.o", 1_000);
        fx.age("old", 60);
        repo(&fx, "fresh");
        fx.file("fresh/dist/app.js", 1);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["old/build", "old/dist"]);
        let dist = found.iter().find(|c| c.path.ends_with("dist")).unwrap();
        assert!(dist.bytes >= 20_000, "{}", dist.bytes);
        assert_eq!(dist.reason, "ignored by git, untouched for 60 days");
    }

    /// Records this thread's log lines, so tests running at the same time
    /// cannot add to them. Only this module's test installs a logger.
    struct ThreadLog;

    thread_local! {
        static LINES: std::cell::RefCell<Vec<(log::Level, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    impl log::Log for ThreadLog {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            LINES.with_borrow_mut(|l| l.push((record.level(), record.args().to_string())));
        }
        fn flush(&self) {}
    }

    /// The per-folder lines run for every project on every scan, so only one
    /// summary line may reach the log at the info level.
    #[test]
    fn a_scan_logs_one_info_summary_and_keeps_folder_lines_at_debug() {
        static LOGGER: ThreadLog = ThreadLog;
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            log::set_logger(&LOGGER).unwrap();
            log::set_max_level(log::LevelFilter::Debug);
        });
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), "!dist/\n").unwrap();
        fx.file("p/dist/app.js", 1);
        fx.age("p", 60);
        LINES.with_borrow_mut(Vec::clear);
        task().scan(&fx.ctx()).unwrap();
        let lines = LINES.with_borrow(Clone::clone);
        let at = |level| -> Vec<&str> {
            lines
                .iter()
                .filter(|(l, _)| *l == level)
                .map(|(_, m)| m.as_str())
                .collect()
        };
        assert_eq!(at(log::Level::Info), vec!["build_output: 0 candidate(s)"]);
        assert!(
            at(log::Level::Debug).iter().any(|m| m.contains("keeping")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_folder_git_does_not_ignore_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        // Negate the pattern so a global excludes file cannot change the answer.
        std::fs::write(dir.join(".gitignore"), "!dist/\n").unwrap();
        fx.file("p/dist/app.js", 1);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_folder_with_tracked_files_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        fx.file("p/dist/committed.js", 1);
        git(&dir, &["add", "-f", "dist/committed.js"]);
        git(&dir, &["commit", "-q", "-m", "ship dist"]);
        fx.file("p/dist/extra.js", 1);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_folder_outside_any_repo_is_kept() {
        let fx = Fixture::new(&[]);
        fx.file("loose/src/a.js", 1);
        fx.file("loose/dist/a.js", 1);
        fx.age("loose", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_folder_holding_a_git_repo_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        fx.file("p/composer.json", 1);
        fx.file("p/vendor/acme/lib/src/A.php", 1);
        git(&dir.join("vendor/acme/lib"), &["init", "-q"]);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    /// A git repo at `rel` that ignores `vendor/`, whose `vendor/acme/lib` is
    /// a clean clone with every commit on a remote branch. This is the one
    /// accepted shape. Each tripwire below breaks one thing about it.
    fn vendored(fx: &Fixture, rel: &str) -> PathBuf {
        let dir = repo(fx, rel);
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        fx.file(&format!("{rel}/vendor/acme/lib/src/A.php"), 1);
        let lib = dir.join("vendor/acme/lib");
        clone_like(&lib);
        dir
    }

    /// Makes `dir` a repo with one commit, which a remote branch also holds.
    fn clone_like(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "release"]);
        git(
            dir,
            &["remote", "add", "origin", "https://example.invalid/a"],
        );
        git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    /// True if a stale project built by `vendored` still offers its `vendor`
    /// after `change` runs in the clone.
    fn offered_after(change: impl Fn(&Fixture, &Path)) -> bool {
        let fx = Fixture::new(&[]);
        let dir = vendored(&fx, "p");
        change(&fx, &dir.join("vendor/acme/lib"));
        fx.age("p", 60);
        !task().scan(&fx.ctx()).unwrap().is_empty()
    }

    #[test]
    fn a_clean_pushed_clone_in_vendor_is_accepted_and_removed() {
        let fx = Fixture::new(&[]);
        let dir = vendored(&fx, "p");
        fx.age("p", 60);
        let ctx = fx.ctx();
        let t = task();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/vendor"]);
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(!dir.join("vendor").exists());
    }

    #[test]
    fn a_clone_in_terraform_modules_is_accepted() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), ".terraform/\n").unwrap();
        fx.file("p/.terraform/modules/vpc/main.tf", 1);
        clone_like(&dir.join(".terraform/modules/vpc"));
        fx.age("p", 60);
        let t = BuildOutput::new(GitCmd::default(), vec![".terraform".into()], 30);
        let found = t.scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/.terraform"]);
    }

    #[test]
    fn a_clone_elsewhere_is_kept() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/acme/lib/a.js", 1);
        clone_like(&fx.root.join("p/dist/acme/lib"));
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_vendor_folder_that_is_itself_a_clone_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        fx.file("p/vendor/a.php", 1);
        clone_like(&dir.join("vendor"));
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_clone_with_a_git_file_is_kept() {
        assert!(!offered_after(|_, lib| {
            std::fs::remove_dir_all(lib.join(".git")).unwrap();
            std::fs::write(lib.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
        }));
    }

    #[test]
    fn a_clone_with_repo_filters_is_kept() {
        assert!(!offered_after(|_, lib| {
            git(lib, &["config", "filter.x.clean", "cat"]);
        }));
    }

    #[test]
    fn a_clone_with_a_gitlink_is_kept() {
        assert!(!offered_after(|_, lib| {
            let id = "1111111111111111111111111111111111111111";
            git(
                lib,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{id},sub"),
                ],
            );
            git(lib, &["commit", "-q", "-m", "submodule"]);
            git(lib, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        }));
    }

    #[test]
    fn a_clone_with_an_edit_or_a_new_file_is_kept() {
        assert!(!offered_after(|fx, _| {
            fx.file("p/vendor/acme/lib/src/A.php", 2);
        }));
        assert!(!offered_after(|fx, _| {
            fx.file("p/vendor/acme/lib/notes.txt", 1);
        }));
    }

    #[test]
    fn a_clone_with_a_commit_on_no_remote_branch_is_kept() {
        assert!(!offered_after(|fx, lib| {
            fx.file("p/vendor/acme/lib/fix.php", 1);
            git(lib, &["add", "."]);
            git(lib, &["commit", "-q", "-m", "local fix"]);
        }));
    }

    /// `--reflog` and `--all` count these though no branch holds them.
    #[test]
    fn a_clone_with_work_only_in_a_stash_or_the_reflog_is_kept() {
        assert!(!offered_after(|fx, lib| {
            fx.file("p/vendor/acme/lib/src/A.php", 2);
            git(lib, &["stash", "-q"]);
        }));
        assert!(!offered_after(|fx, lib| {
            fx.file("p/vendor/acme/lib/fix.php", 1);
            git(lib, &["add", "."]);
            git(lib, &["commit", "-q", "-m", "local fix"]);
            git(lib, &["reset", "-q", "--hard", "refs/remotes/origin/main"]);
        }));
    }

    /// `update-ref` can make a ref under `refs/remotes/` by hand. Only the
    /// fetch refspec of a configured remote makes one count.
    #[test]
    fn a_remote_ref_made_by_hand_does_not_count() {
        assert!(!offered_after(|_, lib| {
            git(lib, &["remote", "remove", "origin"]);
            git(lib, &["update-ref", "refs/remotes/x/y", "HEAD"]);
        }));
        assert!(!offered_after(|_, lib| {
            git(lib, &["update-ref", "-d", "refs/remotes/origin/main"]);
            git(lib, &["update-ref", "refs/remotes/x/y", "HEAD"]);
        }));
    }

    #[test]
    fn a_remote_on_a_local_path_does_not_count() {
        for url in [".", "/srv/git/a.git", "../a.git", "file:///srv/git/a.git"] {
            assert!(
                !offered_after(|_, lib| {
                    git(lib, &["remote", "set-url", "origin", url]);
                }),
                "{url}"
            );
        }
    }

    #[test]
    fn a_clone_with_a_linked_worktree_is_kept() {
        assert!(!offered_after(|fx, lib| {
            let wt = fx.dir.path().join("linked");
            git(
                lib,
                &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
            );
        }));
    }

    #[test]
    fn a_clone_whose_worktrees_folder_cannot_be_read_is_kept() {
        assert!(!offered_after(|_, lib| {
            std::fs::write(lib.join(".git/worktrees"), "").unwrap();
        }));
    }

    #[test]
    fn a_clone_with_a_paused_operation_is_kept() {
        assert!(!offered_after(|_, lib| {
            let head = git_stdout(lib, &["rev-parse", "HEAD"]);
            std::fs::write(lib.join(".git/MERGE_HEAD"), head).unwrap();
        }));
    }

    #[test]
    fn a_clone_with_a_hidden_edit_is_kept() {
        for flag in ["--skip-worktree", "--assume-unchanged"] {
            assert!(
                !offered_after(|fx, lib| {
                    git(lib, &["update-index", flag, "src/A.php"]);
                    fx.file("p/vendor/acme/lib/src/A.php", 2);
                }),
                "{flag}"
            );
        }
    }

    #[test]
    fn a_folder_holding_a_repo_git_cannot_judge_is_kept() {
        for make in [".hg", ".svn"] {
            let fx = Fixture::new(&[]);
            repo(&fx, "p");
            fx.file(&format!("p/dist/acme/{make}/store"), 1);
            fx.age("p", 60);
            assert!(task().scan(&fx.ctx()).unwrap().is_empty(), "{make}");
        }
    }

    #[test]
    fn a_bare_repo_inside_output_is_kept() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/a.js", 1);
        std::fs::create_dir_all(fx.root.join("p/dist/mirror")).unwrap();
        git(&fx.root.join("p/dist/mirror"), &["init", "-q", "--bare"]);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn output_that_is_itself_a_bare_repo_is_kept() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        std::fs::create_dir_all(fx.root.join("p/dist")).unwrap();
        git(&fx.root.join("p/dist"), &["init", "-q", "--bare"]);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    /// `rev-list` is the last thing asked of the clone, so this write lands
    /// after every check of it has passed.
    #[test]
    fn a_commit_in_a_clone_after_its_check_is_refused() {
        let fx = Fixture::new(&[]);
        let dir = vendored(&fx, "p");
        fx.age("p", 60);
        let armed = fx.dir.path().join("armed");
        let lib = dir.join("vendor/acme/lib");
        let write = format!("echo commit > '{}/.git/new-ref'", lib.display());
        let t = BuildOutput::new(
            git_with_hook(fx.dir.path(), "rev-list", 1, &armed, &write),
            vec!["vendor".into()],
            30,
        );
        let ctx = fx.ctx();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        std::fs::write(&armed, "").unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(
            report.failed[0].1,
            "a repo in it changed since the run started"
        );
        assert!(lib.exists());
    }

    #[test]
    fn a_stopped_scan_judges_no_more_clones() {
        let fx = Fixture::new(&[]);
        let dir = vendored(&fx, "p");
        let stopped: Stop = &|| true;
        let why = task().judge(&dir, "vendor", stopped).unwrap_err();
        assert!(why.contains("stopped"), "{why}");
    }

    #[test]
    fn a_clone_with_no_remote_branch_is_kept() {
        assert!(!offered_after(|_, lib| {
            git(lib, &["update-ref", "-d", "refs/remotes/origin/main"]);
        }));
    }

    #[test]
    fn a_clone_with_grafts_is_kept() {
        assert!(!offered_after(|_, lib| {
            std::fs::create_dir_all(lib.join(".git/info")).unwrap();
            std::fs::write(lib.join(".git/info/grafts"), "").unwrap();
        }));
    }

    /// Git would report on another folder, so a clean status proves nothing.
    #[test]
    fn a_clone_pointing_git_at_another_folder_is_kept() {
        assert!(!offered_after(|fx, lib| {
            fx.file("p/vendor/acme/other/keep.txt", 1);
            let other = fx.root.join("p/vendor/acme/other");
            git(lib, &["config", "core.worktree", other.to_str().unwrap()]);
        }));
    }

    #[test]
    fn a_clone_reading_another_repos_refs_is_kept() {
        assert!(!offered_after(|_, lib| {
            std::fs::write(lib.join(".git/commondir"), "../../other/.git\n").unwrap();
        }));
    }

    /// Git walks up past an invalid `.git` folder and reads the repo around it.
    #[test]
    fn a_broken_git_folder_is_kept() {
        assert!(!offered_after(|_, lib| {
            std::fs::remove_dir_all(lib.join(".git")).unwrap();
            std::fs::create_dir(lib.join(".git")).unwrap();
        }));
    }

    #[test]
    fn an_unreadable_folder_keeps_the_output() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/locked/a.js", 1);
        fx.age("p", 60);
        let locked = fx.root.join("p/dist/locked");
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&locked, mode(0o000)).unwrap();
        let found = task().scan(&fx.ctx()).unwrap();
        std::fs::set_permissions(&locked, mode(0o755)).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn a_clone_that_changes_between_scan_and_removal_is_kept() {
        let fx = Fixture::new(&[]);
        let dir = vendored(&fx, "p");
        fx.age("p", 60);
        let ctx = fx.ctx();
        let t = task();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/vendor"]);
        fx.file("p/vendor/acme/lib/notes.txt", 1);
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.failed[0].1.contains("has uncommitted changes"));
        assert!(dir.join("vendor/acme/lib/notes.txt").exists());
    }

    /// Names the reason the fixture above stays kept, now that a clean clone
    /// can pass: `git init` made no commit, so its file is untracked.
    #[test]
    fn a_folder_holding_a_git_repo_is_kept_because_its_files_are_uncommitted() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), "vendor/\n").unwrap();
        fx.file("p/vendor/acme/lib/src/A.php", 1);
        git(&dir.join("vendor/acme/lib"), &["init", "-q"]);
        let why = task().judge(&dir, "vendor", NEVER_STOP).unwrap_err();
        assert!(why.ends_with("has uncommitted changes"), "{why}");
    }

    /// A project with a stale `.next` build, in no repo.
    fn loose_next(fx: &Fixture) {
        fx.file("loose/src/a.js", 1);
        fx.file("loose/.next/BUILD_ID", 1);
        fx.age("loose", 60);
    }

    fn loose_vendor(fx: &Fixture) {
        fx.file("loose/composer.json", 1);
        fx.file("loose/vendor/composer/installed.json", 1);
        fx.age("loose", 60);
    }

    #[test]
    fn a_next_build_outside_a_repo_is_accepted_and_says_why() {
        let fx = Fixture::new(&[]);
        loose_next(&fx);
        let ctx = fx.ctx();
        let t = task();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["loose/.next"]);
        assert_eq!(
            found[0].reason,
            "Next.js build, not in a git repo, untouched for 60 days"
        );
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1, "{report:?}");
        assert!(fx.root.join("loose/src/a.js").exists());
    }

    #[test]
    fn composer_packages_outside_a_repo_are_accepted_and_say_why() {
        let fx = Fixture::new(&[]);
        loose_vendor(&fx);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["loose/vendor"]);
        assert_eq!(
            found[0].reason,
            "Composer packages, not in a git repo, untouched for 60 days"
        );
    }

    #[test]
    fn outside_a_repo_a_recent_project_is_kept() {
        let fx = Fixture::new(&[]);
        loose_next(&fx);
        fx.file("loose/src/b.js", 1);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn outside_a_repo_a_folder_without_its_marker_is_kept() {
        let fx = Fixture::new(&[]);
        fx.file("a/.next/cache/x", 1);
        fx.file("b/vendor/acme/lib.php", 1);
        fx.file("c/vendor/installed.json", 1);
        fx.age("a", 60);
        fx.age("b", 60);
        fx.age("c", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_marker_only_counts_for_its_own_folder_name() {
        let fx = Fixture::new(&[]);
        fx.file("a/dist/BUILD_ID", 1);
        fx.file("b/build/composer/installed.json", 1);
        fx.file("c/.next/composer/installed.json", 1);
        fx.file("d/vendor/BUILD_ID", 1);
        for p in ["a", "b", "c", "d"] {
            fx.age(p, 60);
        }
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_marker_must_be_a_regular_file_in_a_real_folder() {
        let symlink = std::os::unix::fs::symlink::<PathBuf, PathBuf>;
        let fx = Fixture::new(&[]);
        fx.file("a/.next/BUILD_ID/inside", 1);
        let elsewhere = fx.file("elsewhere/composer/installed.json", 1);
        fx.file("b/.next/other", 1);
        symlink(elsewhere.clone(), fx.root.join("b/.next/BUILD_ID")).unwrap();
        fx.file("c/vendor/other", 1);
        symlink(
            elsewhere.parent().unwrap().to_path_buf(),
            fx.root.join("c/vendor/composer"),
        )
        .unwrap();
        for p in ["a", "b", "c", "elsewhere"] {
            fx.age(p, 60);
        }
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn outside_a_repo_a_folder_holding_a_git_repo_is_kept() {
        let fx = Fixture::new(&[]);
        fx.file("loose/vendor/composer/installed.json", 1);
        fx.file("loose/vendor/acme/lib/a.php", 1);
        clone_like(&fx.root.join("loose/vendor/acme/lib"));
        fx.age("loose", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    /// A broken worktree is not "outside a repo". Git fails in it, and
    /// nothing shows whether its files are safe to lose.
    #[test]
    fn a_git_failure_is_not_the_same_as_no_repo() {
        let fx = Fixture::new(&[]);
        fx.file("wt/.next/BUILD_ID", 1);
        std::fs::write(fx.root.join("wt/.git"), "gitdir: /gone/.git/worktrees/x\n").unwrap();
        fx.age("wt", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_marker_does_not_override_git_inside_a_repo() {
        let fx = Fixture::new(&[]);
        let dir = repo(&fx, "p");
        std::fs::write(dir.join(".gitignore"), "!.next/\n").unwrap();
        fx.file("p/.next/BUILD_ID", 1);
        fx.age("p", 60);
        assert!(task().scan(&fx.ctx()).unwrap().is_empty());
    }

    #[test]
    fn a_marker_that_goes_between_scan_and_removal_keeps_the_folder() {
        let fx = Fixture::new(&[]);
        loose_next(&fx);
        let ctx = fx.ctx();
        let t = task();
        let found = t.scan(&ctx).unwrap();
        assert_eq!(found.len(), 1);
        std::fs::remove_file(fx.root.join("loose/.next/BUILD_ID")).unwrap();
        let report = t.run(&ctx, &found);
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(fx.root.join("loose/.next").exists());
    }

    #[test]
    fn rebuilding_output_does_not_make_a_project_fresh() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/old.js", 1);
        fx.age("p", 60);
        fx.file("p/dist/new.js", 1);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/dist"]);
    }

    #[test]
    fn nested_output_folders_are_not_searched() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/build/inner.js", 1);
        fx.age("p", 60);
        let found = task().scan(&fx.ctx()).unwrap();
        assert_eq!(names(&found, &fx.root), vec!["p/dist"]);
    }

    #[test]
    fn run_removes_the_folder_and_keeps_sources() {
        let fx = Fixture::new(&[]);
        repo(&fx, "p");
        fx.file("p/dist/a.js", 1);
        fx.age("p", 60);
        let ctx = fx.ctx();
        let t = task();
        let found = t.scan(&ctx).unwrap();
        let report = t.run(&ctx, &found);
        assert_eq!(report.removed.len(), 1);
        assert!(!fx.root.join("p/dist").exists());
        assert!(fx.root.join("p/src/index.js").exists());
    }
}
