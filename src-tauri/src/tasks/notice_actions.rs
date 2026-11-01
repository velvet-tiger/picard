//! What a person can do with a notice from the Clean window.
//!
//! Picard never removes a broken worktree on its own, because it cannot judge
//! whether the files matter. A person pressing Delete makes that judgement, so
//! that one kind of notice can go to the Trash. The space comes back only when
//! the Trash is emptied, so this is logged but never added to the history or
//! to "Freed by Picard".
//!
//! The window's paths are untrusted, and a stored scan can be hours old.
//! Everything is checked again here, just before acting.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use trash::TrashContext;
use trash::macos::{DeleteMethod, TrashContextExtMacos};
use walkdir::WalkDir;

use super::git_worktrees::{SEARCH_DEPTH, SKIP_DIRS, broken_worktree, surely_broken};
use super::in_use::InUse;
use super::{Ctx, check_target};
use crate::volume::Volume;

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// `open` prints nothing on success and a line or two on failure.
const OPEN_OUTPUT_CAP: usize = 4 * 1024;

/// Regenerable folders the search for nested checkouts does not enter, so a
/// broken worktree with installed dependencies can still be deleted.
const CONTENTS_NOT_SEARCHED: &[&str] = &["node_modules", "target"];
/// The most entries the search reads. More than this is refused, not skipped.
const SEARCH_ENTRY_CAP: usize = 200_000;

/// Folders that a trashed folder must not hold.
pub struct KeptFolders<'a> {
    /// Every configured root, on any volume.
    pub roots: &'a [PathBuf],
    pub home: &'a Path,
}

/// Returns the canonical path of `path` if it is a real folder at or inside
/// one of `roots`. A root can hold a notice, so unlike `check_target` this
/// accepts a root itself.
fn check_reveal(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot read: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("is a symlink".to_string());
    }
    if !meta.is_dir() {
        return Err("is not a directory".to_string());
    }
    let canon = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve: {e}"))?;
    let inside = |root: &PathBuf| root.canonicalize().is_ok_and(|r| canon.starts_with(r));
    if !roots.iter().any(inside) {
        return Err("is outside the configured roots".to_string());
    }
    Ok(canon)
}

/// Shows `path` selected in Finder. `open` hands it to Launch Services and
/// exits at once, so the time limit only covers a stalled Launch Services.
/// The canonical path starts with `/`, so `open` cannot read it as an option.
pub fn reveal_folder(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let shown = path.display();
    let canon =
        check_reveal(path, roots).map_err(|why| format!("could not show {shown}: {why}"))?;
    let mut cmd = Command::new("/usr/bin/open");
    cmd.arg("-R").arg(&canon);
    let out = crate::proc::run(cmd, OPEN_TIMEOUT, OPEN_OUTPUT_CAP)
        .map_err(|e| format!("could not show {shown}: {e}"))?;
    if out.success() {
        Ok(())
    } else {
        Err(format!(
            "could not show {shown}: {}",
            out.failure_summary(OPEN_TIMEOUT)
        ))
    }
}

/// The volume that holds `path`, out of the ones with configured roots.
pub fn volume_holding<'v>(
    path: &Path,
    mut volumes: impl Iterator<Item = &'v Volume>,
) -> Result<&'v Volume, String> {
    let dev = std::fs::metadata(path)
        .map_err(|e| format!("cannot read: {e}"))?
        .dev();
    volumes
        .find(|v| v.dev == dev)
        .ok_or_else(|| "is outside the configured roots".to_string())
}

/// Refuses a folder that sits inside a folder Picard would never search,
/// inside any `.git`, or deeper than the search goes. The scan cannot have
/// found one there, so the row is stale or forged. Roots can nest, so the
/// nearest root counts. Also refuses a folder inside an ignored folder. A
/// clean would remove that one, because protection never reaches down, but
/// Delete has no scan's judgement behind it, so it is stricter on purpose.
fn check_placement(canon: &Path, ctx: &Ctx) -> Result<(), String> {
    if canon.components().any(|c| c.as_os_str() == ".git") {
        return Err("sits inside a .git folder".to_string());
    }
    if canon.ancestors().skip(1).any(|a| ctx.is_ignored(a)) {
        return Err("sits inside an ignored folder".to_string());
    }
    let Some(root) = ctx
        .roots
        .iter()
        .filter(|r| canon.starts_with(r))
        .max_by_key(|r| r.components().count())
    else {
        return Err("is outside the configured roots".to_string());
    };
    let depth = canon.components().count() - root.components().count();
    if depth > SEARCH_DEPTH {
        return Err("sits deeper than Picard searches".to_string());
    }
    for folder in canon.ancestors().take_while(|a| a != root) {
        if ctx.exclude.is_match(folder) {
            return Err("sits inside an excluded folder".to_string());
        }
        if folder
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| SKIP_DIRS.contains(&n))
        {
            return Err(format!("sits inside {}", folder.display()));
        }
    }
    Ok(())
}

/// Everything about the folder itself that must hold. Returns its canonical
/// path. The caller must have refreshed the ignore list on `ctx`.
fn check_folder(path: &Path, ctx: &Ctx) -> Result<PathBuf, String> {
    let canon = check_target(path, ctx)?;
    ctx.refuse_kept(&canon)?;
    check_placement(&canon, ctx)?;
    if broken_worktree(&canon).is_none() {
        return Err("is not a broken worktree".to_string());
    }
    if !surely_broken(&canon) {
        return Err("its repo may be on a drive that is not mounted".to_string());
    }
    Ok(canon)
}

/// Refuses a folder that holds a root, the home folder or another checkout.
/// `CONTENTS_NOT_SEARCHED` limits only the search for checkouts, and
/// `entry_cap` bounds it. A root or the home folder is found from its path,
/// wherever it sits.
fn check_contents(canon: &Path, kept: &KeptFolders, entry_cap: usize) -> Result<(), String> {
    let holds =
        |p: &Path| p.starts_with(canon) || p.canonicalize().is_ok_and(|c| c.starts_with(canon));
    if let Some(root) = kept.roots.iter().find(|r| holds(r)) {
        return Err(format!(
            "it holds {}, a configured root. Check it, then delete it yourself",
            root.display()
        ));
    }
    if holds(kept.home) {
        return Err("it holds your home folder. Check it, then delete it yourself".to_string());
    }
    let walk = WalkDir::new(canon)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_str();
            !e.file_type().is_dir() || !name.is_some_and(|n| CONTENTS_NOT_SEARCHED.contains(&n))
        });
    for (seen, entry) in walk.enumerate() {
        if seen >= entry_cap {
            return Err(
                "it holds too many files to check. Check it, then delete it yourself".to_string(),
            );
        }
        let entry = entry.map_err(|e| {
            format!("Picard could not read all of it ({e}). Check it, then delete it yourself")
        })?;
        if entry.file_name() == ".git" && entry.depth() > 1 {
            return Err(format!(
                "it holds another git checkout at {}. Check it, then delete it yourself",
                entry.path().display()
            ));
        }
        if entry.file_type().is_dir() && is_bare_repo(entry.path()) {
            return Err(format!(
                "it holds a bare git repo at {}. Check it, then delete it yourself",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

/// True if `dir` has both `HEAD` and `objects`, as a git repo does. Neither is
/// followed if it is a symlink.
fn is_bare_repo(dir: &Path) -> bool {
    let has = |name| std::fs::symlink_metadata(dir.join(name)).is_ok();
    has("HEAD") && has("objects")
}

/// Returns the canonical path of `path` if it may go to the Trash now.
///
/// Refuses while a clean runs, and fails closed if `load_in_use` fails. The
/// slow process listing waits until the cheap checks pass. The folder is then
/// judged once more, so its path and state are what was checked. The caller
/// must have refreshed the ignore list on `ctx` with `Ctx::set_keep`.
fn check_broken_worktree(
    path: &Path,
    ctx: &Ctx,
    cleaning: bool,
    kept: &KeptFolders,
    load_in_use: impl FnOnce() -> Result<InUse>,
) -> Result<PathBuf, String> {
    if cleaning {
        return Err("a cleanup is running".to_string());
    }
    let canon = check_folder(path, ctx)?;
    check_contents(&canon, kept, SEARCH_ENTRY_CAP)?;
    let in_use = load_in_use()
        .map_err(|e| format!("could not check whether a process is using it: {e:#}"))?;
    if in_use.contains(&canon) {
        return Err("a running process is using it".to_string());
    }
    if check_folder(path, ctx)? != canon {
        return Err("it moved while Picard was checking it".to_string());
    }
    Ok(canon)
}

/// Checks `path`, then hands the canonical path to `trash`. Returns that path.
fn trash_checked(
    path: &Path,
    ctx: &Ctx,
    cleaning: bool,
    kept: &KeptFolders,
    load_in_use: impl FnOnce() -> Result<InUse>,
    trash: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let refused = |why| format!("did not delete {}: {why}", path.display());
    let canon = check_broken_worktree(path, ctx, cleaning, kept, load_in_use).map_err(refused)?;
    trash(&canon).map_err(|e| format!("could not delete {}: {e}", canon.display()))?;
    log::info!("moved broken worktree {} to the Trash", canon.display());
    Ok(canon)
}

/// Moves `path` to the Trash. The crate's default asks Finder through
/// osascript, which needs an Automation permission, so this picks
/// NSFileManager. The price is that "Put Back" may not be offered.
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    let mut trash = TrashContext::new();
    trash.set_delete_method(DeleteMethod::NsFileManager);
    trash.delete(path).map_err(|e| e.to_string())
}

/// Hands a broken worktree to `trash`, if it still passes every check, and
/// returns its canonical path. `cleaning` says whether a clean is marked as
/// running. The caller holds the run lock, so no clean can start until this
/// returns.
pub fn trash_broken_worktree(
    path: &Path,
    ctx: &Ctx,
    cleaning: bool,
    kept: &KeptFolders,
    trash: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    trash_checked(path, ctx, cleaning, kept, InUse::load, trash)
}

#[cfg(test)]
pub(super) fn check_with_in_use(
    path: &Path,
    ctx: &Ctx,
    in_use: Vec<PathBuf>,
) -> Result<PathBuf, String> {
    let kept = KeptFolders {
        roots: &[],
        home: Path::new("/nonexistent-home"),
    };
    check_broken_worktree(path, ctx, false, &kept, || Ok(InUse::from_cwds(in_use)))
}

#[cfg(test)]
mod tests {
    use super::super::IgnoreList;
    use super::super::test_support::Fixture;
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// `root/<name>`, whose `.git` file names a record that does not exist.
    fn broken(fx: &Fixture, name: &str) -> PathBuf {
        let folder = fx.file(&format!("{name}/work.txt"), 10);
        let folder = folder.parent().unwrap().to_path_buf();
        let id = folder.file_name().unwrap();
        fs::create_dir_all(fx.root.join("repo/.git/worktrees")).unwrap();
        let record = fx.root.join("repo/.git/worktrees").join(id);
        fs::write(
            folder.join(".git"),
            format!("gitdir: {}\n", record.display()),
        )
        .unwrap();
        folder
    }

    fn nothing_kept() -> KeptFolders<'static> {
        KeptFolders {
            roots: &[],
            home: Path::new("/nonexistent-home"),
        }
    }

    fn nothing_in_use() -> Result<InUse> {
        Ok(InUse::from_cwds(vec![]))
    }

    /// Tries to trash `path` and returns the error, proving nothing was trashed.
    fn refusal(path: &Path, ctx: &Ctx) -> String {
        let trashed = RefCell::new(Vec::new());
        let result = trash_checked(path, ctx, false, &nothing_kept(), nothing_in_use, |p| {
            trashed.borrow_mut().push(p.to_path_buf());
            Ok(())
        });
        assert!(trashed.borrow().is_empty(), "{trashed:?}");
        result.unwrap_err()
    }

    #[test]
    fn a_broken_worktree_goes_to_the_trash_by_its_canonical_path() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let trashed = RefCell::new(Vec::new());
        trash_checked(
            &wt,
            &fx.ctx(),
            false,
            &nothing_kept(),
            nothing_in_use,
            |p| {
                trashed.borrow_mut().push(p.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*trashed.borrow(), vec![wt.canonicalize().unwrap()]);
    }

    #[test]
    fn a_failed_trash_comes_back_with_the_path() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = trash_checked(
            &wt,
            &fx.ctx(),
            false,
            &nothing_kept(),
            nothing_in_use,
            |_| Err("the Trash is full".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("stuck"), "{err}");
        assert!(err.contains("the Trash is full"), "{err}");
    }

    #[test]
    fn refuses_a_plain_folder() {
        let fx = Fixture::new(&[]);
        fx.file("plain/a.txt", 10);
        let err = refusal(&fx.root.join("plain"), &fx.ctx());
        assert!(err.contains("is not a broken worktree"), "{err}");
    }

    /// As a repaired worktree looks, or one that only lost its `.gitignore`:
    /// its record exists.
    #[test]
    fn refuses_a_worktree_whose_record_exists() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        fs::create_dir_all(fx.root.join("repo/.git/worktrees/stuck")).unwrap();
        let err = refusal(&wt, &fx.ctx());
        assert!(err.contains("is not a broken worktree"), "{err}");
    }

    #[test]
    fn refuses_a_symlink() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let link = fx.root.join("link");
        symlink(&wt, &link).unwrap();
        let err = refusal(&link, &fx.ctx());
        assert!(err.contains("is a symlink"), "{err}");
    }

    #[test]
    fn refuses_a_folder_outside_the_roots() {
        let fx = Fixture::new(&[]);
        let outside = fx.dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join(".git"), "gitdir: /nowhere/.git/worktrees/x\n").unwrap();
        let err = refusal(&outside, &fx.ctx());
        assert!(err.contains("is outside the configured roots"), "{err}");
    }

    #[test]
    fn refuses_a_root_itself() {
        let fx = Fixture::new(&[]);
        fs::write(fx.root.join(".git"), "gitdir: /nowhere/.git/worktrees/x\n").unwrap();
        let err = refusal(&fx.root, &fx.ctx());
        assert!(err.contains("is outside the configured roots"), "{err}");
    }

    #[test]
    fn refuses_an_excluded_folder() {
        let fx = Fixture::new(&["**/stuck"]);
        let wt = broken(&fx, "stuck");
        let err = refusal(&wt, &fx.ctx());
        assert!(err.contains("matches an exclude pattern"), "{err}");
    }

    #[test]
    fn refuses_an_ignored_folder_and_one_that_holds_an_ignored_folder() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let ctx = fx.ctx();
        ctx.set_keep(IgnoreList::new(std::slice::from_ref(&wt)));
        let err = refusal(&wt, &ctx);
        assert!(err.contains("is ignored"), "{err}");
        ctx.set_keep(IgnoreList::new(&[wt.join("work.txt")]));
        let err = refusal(&wt, &ctx);
        assert!(err.contains("holds an ignored folder"), "{err}");
    }

    #[test]
    fn refuses_a_folder_a_process_is_working_in() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let cwd = wt.canonicalize().unwrap().join("sub");
        let err = check_with_in_use(&wt, &fx.ctx(), vec![cwd]).unwrap_err();
        assert!(err.contains("a running process is using it"), "{err}");
    }

    #[test]
    fn refuses_when_the_process_list_cannot_be_read() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = check_broken_worktree(&wt, &fx.ctx(), false, &nothing_kept(), || {
            anyhow::bail!("lsof broke")
        })
        .unwrap_err();
        assert!(err.contains("lsof broke"), "{err}");
    }

    #[test]
    fn refuses_while_a_clean_is_running() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = check_broken_worktree(&wt, &fx.ctx(), true, &nothing_kept(), nothing_in_use)
            .unwrap_err();
        assert!(err.contains("a cleanup is running"), "{err}");
    }

    #[test]
    fn refuses_a_folder_inside_a_folder_the_search_skips() {
        let fx = Fixture::new(&["**/private"]);
        for (parent, why) in [
            ("a/node_modules", "sits inside"),
            ("a/target", "sits inside"),
            ("a/private", "sits inside an excluded folder"),
            ("a/.git", "sits inside a .git folder"),
        ] {
            let wt = broken(&fx, &format!("{parent}/stuck"));
            let err = refusal(&wt, &fx.ctx());
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn the_nearest_of_two_nested_roots_decides() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "target/sub/wt");
        let mut ctx = fx.ctx();
        assert!(refusal(&wt, &ctx).contains("sits inside"));
        let nested = fx.root.canonicalize().unwrap().join("target/sub");
        ctx.roots.push(nested);
        assert!(check_with_in_use(&wt, &ctx, vec![]).is_ok());
    }

    #[test]
    fn refuses_a_folder_inside_an_ignored_folder_but_not_beside_one() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "a/stuck");
        fx.file("a/other/x.txt", 1);
        let ctx = fx.ctx();
        ctx.set_keep(IgnoreList::new(&[fx.root.join("a/other")]));
        assert!(check_with_in_use(&wt, &ctx, vec![]).is_ok());
        ctx.set_keep(IgnoreList::new(&[fx.root.join("a")]));
        let err = refusal(&wt, &ctx);
        assert!(err.contains("sits inside an ignored folder"), "{err}");
    }

    #[test]
    fn refuses_a_folder_deeper_than_the_search_goes() {
        let fx = Fixture::new(&[]);
        let deepest = broken(&fx, "1/2/3/4/5/6/7/8");
        assert!(check_with_in_use(&deepest, &fx.ctx(), vec![]).is_ok());
        let too_deep = broken(&fx, "a/2/3/4/5/6/7/8/9");
        let err = refusal(&too_deep, &fx.ctx());
        assert!(err.contains("deeper than Picard searches"), "{err}");
    }

    #[test]
    fn finds_a_nested_checkout_at_any_depth() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        fx.file("stuck/1/2/3/4/5/6/7/8/9/10/11/.git/HEAD", 1);
        let err = refusal(&wt, &fx.ctx());
        assert!(err.contains("another git checkout"), "{err}");
    }

    #[test]
    fn refuses_a_folder_holding_a_bare_repo() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        fx.file("stuck/a/repo.git/HEAD", 1);
        assert!(check_with_in_use(&wt, &fx.ctx(), vec![]).is_ok());
        fx.file("stuck/a/repo.git/objects/info", 1);
        let err = refusal(&wt, &fx.ctx());
        assert!(err.contains("a bare git repo"), "{err}");
        assert!(err.contains("delete it yourself"), "{err}");
    }

    #[test]
    fn refuses_a_folder_holding_a_nested_checkout() {
        for nested in [".git/HEAD", "b/.git"] {
            let fx = Fixture::new(&[]);
            let wt = broken(&fx, "stuck");
            fx.file(&format!("stuck/a/{nested}"), 1);
            let err = refusal(&wt, &fx.ctx());
            assert!(err.contains("another git checkout"), "{err}");
            assert!(err.contains("delete it yourself"), "{err}");
        }
    }

    #[test]
    fn dependencies_and_build_output_do_not_stop_a_delete() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        fx.file("stuck/node_modules/dep/.git/HEAD", 1);
        fx.file("stuck/target/dep/.git/HEAD", 1);
        assert!(check_with_in_use(&wt, &fx.ctx(), vec![]).is_ok());
    }

    #[test]
    fn refuses_a_folder_holding_a_root_or_the_home_folder_at_any_depth() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck").canonicalize().unwrap();
        fx.file("stuck/node_modules/deep/a.txt", 1);
        let inner = wt.join("node_modules/deep");
        let roots = [inner.clone()];
        let as_root = KeptFolders {
            roots: &roots,
            home: Path::new("/nonexistent-home"),
        };
        let err = check_contents(&wt, &as_root, SEARCH_ENTRY_CAP).unwrap_err();
        assert!(err.contains("a configured root"), "{err}");
        let as_home = KeptFolders {
            roots: &[],
            home: &inner,
        };
        let err = check_contents(&wt, &as_home, SEARCH_ENTRY_CAP).unwrap_err();
        assert!(err.contains("your home folder"), "{err}");
    }

    #[test]
    fn refuses_when_the_entry_cap_is_hit() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = check_contents(&wt, &nothing_kept(), 1).unwrap_err();
        assert!(err.contains("too many files"), "{err}");
        assert!(check_contents(&wt, &nothing_kept(), 100).is_ok());
    }

    #[test]
    fn refuses_when_an_entry_cannot_be_read() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let locked = wt.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let result = check_contents(&wt, &nothing_kept(), SEARCH_ENTRY_CAP);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.unwrap_err().contains("could not read"));
    }

    /// `change` runs while the process list loads, the last slow step.
    fn check_after(path: &Path, ctx: &Ctx, change: impl FnOnce()) -> Result<PathBuf, String> {
        check_broken_worktree(path, ctx, false, &nothing_kept(), || {
            change();
            nothing_in_use()
        })
    }

    #[test]
    fn refuses_a_folder_repaired_during_the_process_listing() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = check_after(&wt, &fx.ctx(), || {
            fs::create_dir_all(fx.root.join("repo/.git/worktrees/stuck")).unwrap();
        })
        .unwrap_err();
        assert_eq!(err, "is not a broken worktree");
    }

    #[test]
    fn refuses_a_folder_swapped_for_a_symlink_during_the_process_listing() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = check_after(&wt, &fx.ctx(), || {
            fs::rename(&wt, fx.root.join("moved")).unwrap();
            symlink(fx.root.join("moved"), &wt).unwrap();
        })
        .unwrap_err();
        assert_eq!(err, "is a symlink");
    }

    #[test]
    fn refuses_a_path_that_resolves_elsewhere_after_the_process_listing() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "a/stuck");
        let other = broken(&fx, "b/stuck");
        let err = check_after(&wt, &fx.ctx(), || {
            fs::rename(fx.root.join("a"), fx.root.join("old")).unwrap();
            symlink(other.parent().unwrap(), fx.root.join("a")).unwrap();
        })
        .unwrap_err();
        assert_eq!(err, "it moved while Picard was checking it");
    }

    #[test]
    fn reveal_accepts_a_folder_or_root_and_gives_an_absolute_path() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let roots = vec![fx.root.clone()];
        assert_eq!(
            check_reveal(&wt, &roots).unwrap(),
            wt.canonicalize().unwrap()
        );
        assert!(check_reveal(&fx.root, &roots).unwrap().is_absolute());
    }

    #[test]
    fn reveal_refuses_what_is_not_a_real_folder_in_the_roots() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let roots = vec![fx.root.clone()];
        let link = fx.root.join("link");
        symlink(&wt, &link).unwrap();
        let reason = |p: &Path| check_reveal(p, &roots).unwrap_err();
        assert_eq!(reason(&link), "is a symlink");
        assert_eq!(reason(&wt.join("work.txt")), "is not a directory");
        assert_eq!(reason(fx.dir.path()), "is outside the configured roots");
        assert_eq!(
            reason(&fx.root.join("stuck/../..")),
            "is outside the configured roots"
        );
        assert!(reason(&fx.root.join("gone")).starts_with("cannot read"));
    }

    #[test]
    fn finds_the_volume_by_device_and_refuses_an_unknown_one() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let dev = fs::metadata(&fx.root).unwrap().dev();
        let volume = |dev| Volume {
            dev,
            mount: PathBuf::from("/"),
            name: "test".into(),
            roots: vec![fx.root.clone()],
            uuid: None,
        };
        let (other, ours) = (volume(dev + 1), volume(dev));
        let found = volume_holding(&wt, [&other, &ours].into_iter()).unwrap();
        assert_eq!(found.dev, dev);
        let err = volume_holding(&wt, [&other].into_iter()).err().unwrap();
        assert_eq!(err, "is outside the configured roots");
    }
}
