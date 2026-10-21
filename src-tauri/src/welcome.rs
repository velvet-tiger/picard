//! The first-run warning.
//!
//! Picard deletes files, so the first launch asks the user to accept that they
//! use it at their own risk. Acceptance is a marker file next to the config, not
//! a config setting: the config is refused when its permissions are loose, and
//! Picard never rewrites it on its own.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tauri::{AppHandle, Runtime};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tokio::sync::oneshot;

const TITLE: &str = "Before you start";
const MESSAGE: &str = "Picard cleans up filesystem artifacts such as git worktrees, node_modules and build folders. Removed items cannot be restored.\n\nPlease use Picard at your own risk.";
const ACCEPT: &str = "I understand";
const DECLINE: &str = "Quit";

/// True until the user has accepted the warning. If the marker's state cannot
/// be read, the warning is shown: asking twice is safer than never asking.
pub fn needs_warning(marker: &Path) -> bool {
    !marker.try_exists().unwrap_or(false)
}

/// Records that the user accepted the warning.
pub fn record_acceptance(marker: &Path) -> Result<()> {
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(marker, "accepted\n")
        .with_context(|| format!("recording the accepted warning in {}", marker.display()))
}

/// Resolves to `true` when Picard may start: the warning was accepted now or
/// earlier. Resolves to `false` when the user chose Quit or the dialog could
/// not be shown to completion. The dialog does not block the caller, so the
/// event loop keeps running while it is open.
pub fn confirm<R: Runtime>(app: &AppHandle<R>, marker: PathBuf) -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    if !needs_warning(&marker) {
        // The receiver is held by the caller, so this cannot fail.
        let _ = tx.send(true);
        return rx;
    }
    app.dialog()
        .message(MESSAGE)
        .title(TITLE)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            ACCEPT.to_string(),
            DECLINE.to_string(),
        ))
        .show(move |accepted| {
            if accepted && let Err(e) = record_acceptance(&marker) {
                // Picard still starts; the warning returns on the next launch.
                log::error!("{e:#}");
            }
            // The receiver only goes away if Picard is already shutting down.
            let _ = tx.send(accepted);
        });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_until_acceptance_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("warning-accepted.txt");

        assert!(needs_warning(&marker));
        record_acceptance(&marker).unwrap();
        assert!(!needs_warning(&marker));
    }

    #[test]
    fn recording_creates_a_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("Picard/warning-accepted.txt");

        record_acceptance(&marker).unwrap();
        assert!(marker.is_file());
    }

    #[test]
    fn recording_twice_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("warning-accepted.txt");

        record_acceptance(&marker).unwrap();
        record_acceptance(&marker).unwrap();
        assert!(!needs_warning(&marker));
    }

    #[test]
    fn a_failed_write_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where the folder should be makes create_dir_all fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let marker = blocker.join("warning-accepted.txt");

        let err = record_acceptance(&marker).unwrap_err();
        assert!(format!("{err:#}").contains("blocker"), "{err:#}");
        assert!(needs_warning(&marker));
    }
}
