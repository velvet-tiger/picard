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

use super::git_worktrees::broken_worktree;
use super::in_use::InUse;
use super::{Ctx, check_target};
use crate::volume::Volume;

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// `open` prints nothing on success and a line or two on failure.
const OPEN_OUTPUT_CAP: usize = 4 * 1024;

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

/// Returns the canonical path of `path` if it may go to the Trash now.
///
/// Refuses while a clean runs, and fails closed if `load_in_use` fails. The
/// slow process listing comes last, so a bad path costs nothing. The caller
/// must have refreshed the ignore list on `ctx` with `Ctx::set_keep`.
fn check_broken_worktree(
    path: &Path,
    ctx: &Ctx,
    cleaning: bool,
    load_in_use: impl FnOnce() -> Result<InUse>,
) -> Result<PathBuf, String> {
    if cleaning {
        return Err("a cleanup is running".to_string());
    }
    let canon = check_target(path, ctx)?;
    ctx.refuse_kept(&canon)?;
    if broken_worktree(&canon).is_none() {
        return Err("is not a broken worktree".to_string());
    }
    let in_use = load_in_use()
        .map_err(|e| format!("could not check whether a process is using it: {e:#}"))?;
    if in_use.contains(&canon) {
        return Err("a running process is using it".to_string());
    }
    Ok(canon)
}

/// Checks `path`, then hands the canonical path to `trash`.
fn trash_checked(
    path: &Path,
    ctx: &Ctx,
    cleaning: bool,
    load_in_use: impl FnOnce() -> Result<InUse>,
    trash: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let refused = |why| format!("did not delete {}: {why}", path.display());
    let canon = check_broken_worktree(path, ctx, cleaning, load_in_use).map_err(refused)?;
    trash(&canon).map_err(|e| format!("could not delete {}: {e}", canon.display()))?;
    log::info!("moved broken worktree {} to the Trash", canon.display());
    Ok(())
}

/// Moves `path` to the Trash. The crate's default asks Finder through
/// osascript, which needs an Automation permission, so this picks
/// NSFileManager. The price is that "Put Back" may not be offered.
fn move_to_trash(path: &Path) -> Result<(), String> {
    let mut trash = TrashContext::new();
    trash.set_delete_method(DeleteMethod::NsFileManager);
    trash.delete(path).map_err(|e| e.to_string())
}

/// Moves a broken worktree to the Trash, if it still passes every check.
/// `cleaning` says whether a clean is marked as running. The caller holds the
/// run lock, so no clean can start until this returns.
pub fn trash_broken_worktree(path: &Path, ctx: &Ctx, cleaning: bool) -> Result<(), String> {
    trash_checked(path, ctx, cleaning, InUse::load, move_to_trash)
}

#[cfg(test)]
pub(super) fn check_with_in_use(
    path: &Path,
    ctx: &Ctx,
    in_use: Vec<PathBuf>,
) -> Result<PathBuf, String> {
    check_broken_worktree(path, ctx, false, || Ok(InUse::from_cwds(in_use)))
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
        fs::create_dir_all(fx.root.join("repo/.git/worktrees")).unwrap();
        let record = fx.root.join("repo/.git/worktrees").join(name);
        fs::write(
            folder.join(".git"),
            format!("gitdir: {}\n", record.display()),
        )
        .unwrap();
        folder
    }

    fn nothing_in_use() -> Result<InUse> {
        Ok(InUse::from_cwds(vec![]))
    }

    /// Tries to trash `path` and returns the error, proving nothing was trashed.
    fn refusal(path: &Path, ctx: &Ctx) -> String {
        let trashed = RefCell::new(Vec::new());
        let result = trash_checked(path, ctx, false, nothing_in_use, |p| {
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
        trash_checked(&wt, &fx.ctx(), false, nothing_in_use, |p| {
            trashed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(*trashed.borrow(), vec![wt.canonicalize().unwrap()]);
    }

    #[test]
    fn a_failed_trash_comes_back_with_the_path() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = trash_checked(&wt, &fx.ctx(), false, nothing_in_use, |_| {
            Err("the Trash is full".to_string())
        })
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
        let err = check_broken_worktree(&wt, &fx.ctx(), false, || anyhow::bail!("lsof broke"))
            .unwrap_err();
        assert!(err.contains("lsof broke"), "{err}");
    }

    #[test]
    fn refuses_while_a_clean_is_running() {
        let fx = Fixture::new(&[]);
        let wt = broken(&fx, "stuck");
        let err = check_broken_worktree(&wt, &fx.ctx(), true, nothing_in_use).unwrap_err();
        assert!(err.contains("a cleanup is running"), "{err}");
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
