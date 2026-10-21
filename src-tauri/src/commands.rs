//! Commands the Settings and Preview windows call.
//!
//! These are thin: the work lives in `runner`, `settings` and `poll::scan`,
//! which are tested on their own. Blocking work runs on blocking threads.
//! Errors go back to the window as text, because that is what it shows.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime, State, WebviewWindow};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::DialogExt;

use crate::about::{self, Link};
use crate::config::{self, Appearance, Config, Mode};
use crate::disk::{self, DiskSpace};
use crate::history::{self, Entry, Totals};
use crate::poll;
use crate::runner::{self, CleanRequest, TaskScan, VolumeRun};
use crate::settings::{self, Settings};
use crate::state::Shared;
use crate::tasks::{IgnoreList, Protected};
use crate::updater::{self, UpdateStatus, Updates};
use crate::volume::Volume;
use crate::windows;

type SharedState<'a> = State<'a, Arc<Shared>>;

const STILL_STARTING: &str = "Picard is still starting up. Try again in a moment.";
const CLEAN_RUNNING: &str = "A clean is running. Rescan when it finishes.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeStatus {
    pub name: String,
    pub mount: String,
    pub free_text: String,
    pub low: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub mode: Mode,
    pub volumes: Vec<VolumeStatus>,
    /// "Projects: not mounted", one per unavailable root.
    pub unavailable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewItem {
    pub path: String,
    pub bytes: u64,
    pub size_text: String,
    pub reason: String,
    /// In the config's `ignore` list: shown, but never removed.
    pub ignored: bool,
    /// Holds a folder in the `ignore` list, so it is never removed either.
    pub holds_ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewTask {
    pub task: String,
    pub items: Vec<PreviewItem>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumePreview {
    pub name: String,
    pub mount: String,
    pub free_text: String,
    pub tasks: Vec<PreviewTask>,
    pub total_bytes: u64,
    pub total_text: String,
    /// When the scan of this volume started. A stored scan can be hours old.
    pub scanned_at: DateTime<Utc>,
}

/// Paths the user ticked on one volume.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Selection {
    pub mount: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunItem {
    pub task: String,
    pub path: String,
    pub size_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunFailure {
    pub task: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunResult {
    pub name: String,
    pub freed_text: String,
    pub removed: Vec<RunItem>,
    pub failed: Vec<RunFailure>,
    /// Left alone because the ignore list protects them; `reason` says why.
    pub skipped: Vec<RunFailure>,
    pub stopped: Option<String>,
    /// Set when the whole volume could not be cleaned, for example because
    /// another cleanup was already running.
    pub error: Option<String>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn load_config(shared: &Shared) -> Result<Config, String> {
    config::load(&shared.config_path, &shared.home).map_err(|e| format!("{e:#}"))
}

/// Runs blocking work off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("background task failed: {e}"))?
}

#[tauri::command]
pub async fn get_status(state: SharedState<'_>) -> Result<Status, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let config = load_config(&shared)?;
        let found = poll::scan(&config.roots, &shared.uuids);
        Ok(status_view(&config, &found.readings, &found.unavailable))
    })
    .await
}

#[tauri::command]
pub async fn get_settings(state: SharedState<'_>) -> Result<Settings, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let text = read_config_text(&shared)?;
        settings::read(&text, &shared.home).map_err(|e| format!("{e:#}"))
    })
    .await
}

/// The appearance the config asks for, so the window can match it on load.
#[tauri::command]
pub async fn get_appearance(state: SharedState<'_>) -> Result<Appearance, String> {
    let shared = Arc::clone(&state);
    blocking(move || load_config(&shared).map(|cfg| cfg.appearance)).await
}

#[tauri::command]
pub async fn save_settings<R: Runtime>(
    app: AppHandle<R>,
    state: SharedState<'_>,
    settings: Settings,
) -> Result<(), String> {
    let shared = Arc::clone(&state);
    let appearance = settings.appearance;
    blocking(move || {
        let _writing = shared.lock_config();
        let text = read_config_text(&shared)?;
        let updated =
            settings::apply(&text, &settings, &shared.home).map_err(|e| format!("{e:#}"))?;
        settings::write_private(&shared.config_path, &updated).map_err(|e| format!("{e:#}"))?;
        log::info!("settings saved to {}", shared.config_path.display());
        Ok(())
    })
    .await?;
    // The settings are saved by now, so a failure here only leaves the title
    // bar in the old appearance until the window next opens.
    if let Err(e) = windows::set_appearance(&app, appearance) {
        log::warn!("could not apply the appearance to the title bar: {e}");
    }
    Ok(())
}

/// The default `activity_ignore` names, for the Settings tab's Restore
/// defaults button.
#[tauri::command]
pub fn get_not_activity() -> Vec<String> {
    config::default_activity_ignore()
}

/// The `ignore` list as written in the config, for the Settings tab.
#[tauri::command]
pub async fn get_ignored(state: SharedState<'_>) -> Result<Vec<String>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let text = read_config_text(&shared)?;
        settings::ignored(&text, &shared.home).map_err(|e| format!("{e:#}"))
    })
    .await
}

/// Adds `path` to the `ignore` list, or takes it off. Ignoring only ever stops
/// removals, so any absolute path is accepted, not only current candidates.
#[tauri::command]
pub async fn set_ignored(
    state: SharedState<'_>,
    path: String,
    ignored: bool,
) -> Result<(), String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let _writing = shared.lock_config();
        let text = read_config_text(&shared)?;
        let updated = settings::set_ignored(&text, &path, ignored, &shared.home)
            .map_err(|e| format!("{e:#}"))?;
        if updated == text {
            return Ok(());
        }
        settings::write_private(&shared.config_path, &updated).map_err(|e| format!("{e:#}"))?;
        log::info!(
            "{} {path}",
            if ignored {
                "ignored"
            } else {
                "stopped ignoring"
            }
        );
        Ok(())
    })
    .await
}

/// How the current `ignore` list protects each of `paths`, in order:
/// "ignored", "holds_ignored", or null. Reads the config only, with no scan,
/// so the Clean tab can refresh every row after an Ignore or Unignore.
#[tauri::command]
pub async fn get_protection(
    state: SharedState<'_>,
    paths: Vec<String>,
) -> Result<Vec<Option<&'static str>>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let config = load_config(&shared)?;
        Ok(protection_view(&IgnoreList::new(&config.ignore), &paths))
    })
    .await
}

fn protection_view(ignore: &IgnoreList, paths: &[String]) -> Vec<Option<&'static str>> {
    paths
        .iter()
        .map(|p| match ignore.protects(std::path::Path::new(p)) {
            Some(Protected::Ignored) => Some("ignored"),
            Some(Protected::HoldsIgnored) => Some("holds_ignored"),
            None => None,
        })
        .collect()
}

/// A preview during a clean repeats the clean's work and lists folders it may
/// be about to delete, so it is refused. The check is advisory: a clean that
/// starts a moment later still re-checks every folder before removing it.
fn refuse_while_cleaning(cleaning: bool) -> Result<(), String> {
    if cleaning {
        Err(CLEAN_RUNNING.to_string())
    } else {
        Ok(())
    }
}

#[tauri::command]
pub async fn preview(state: SharedState<'_>) -> Result<Vec<VolumePreview>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        refuse_while_cleaning(shared.is_cleaning())?;
        let config = load_config(&shared)?;
        let tools = shared.tools().ok_or(STILL_STARTING)?;
        log::info!("preview scan started");
        let started = Utc::now();
        let found = poll::scan(&config.roots, &shared.uuids);
        let mut out = Vec::new();
        for (volume, space) in &found.readings {
            let scans = runner::preview_volume(&config, volume, &shared.sizes, &tools)
                .map_err(|e| format!("{}: {e:#}", volume.name))?;
            out.push(preview_view(volume, *space, &scans, started));
        }
        shared.replace_previews(out.clone());
        Ok(out)
    })
    .await
}

/// The latest scan of each volume, from a Scan in the Clean tab or from the
/// estimate behind a notification. Empty when neither has run since launch.
#[tauri::command]
pub fn get_last_preview(state: SharedState<'_>) -> Vec<VolumePreview> {
    state.previews()
}

#[tauri::command]
pub async fn run_selected<R: Runtime>(
    app: AppHandle<R>,
    state: SharedState<'_>,
    selections: Vec<Selection>,
) -> Result<Vec<RunResult>, String> {
    let shared = Arc::clone(&state);
    blocking(move || {
        let _cleaning = shared.start_cleaning(&app);
        let config = load_config(&shared)?;
        let tools = shared.tools().ok_or(STILL_STARTING)?;
        let found = poll::scan(&config.roots, &shared.uuids);
        let mut out = Vec::new();
        for selection in selections {
            let Some((volume, _)) = found
                .readings
                .iter()
                .find(|(v, _)| v.mount.to_string_lossy() == selection.mount)
            else {
                return Err(format!("{} is no longer mounted", selection.mount));
            };
            let chosen: HashSet<PathBuf> = selection.paths.iter().map(PathBuf::from).collect();
            // The stored list may now name removed folders. The tab rescans
            // after a clean; this covers a rescan that fails.
            shared.forget_preview(&selection.mount);
            let result = runner::clean_volume(
                &config,
                volume,
                &shared.sizes,
                &shared.paths,
                &tools,
                CleanRequest::Selected(chosen),
                &runner::ignore_from_file(&shared.config_path, &shared.home),
            );
            out.push(run_view(&volume.name, result.map_err(|e| format!("{e:#}"))));
        }
        Ok(out)
    })
    .await
}

#[tauri::command]
pub async fn get_history(state: SharedState<'_>, limit: usize) -> Result<Vec<Entry>, String> {
    let shared = Arc::clone(&state);
    blocking(move || history::read_recent(&shared.paths.history, limit.min(500)).map_err(err)).await
}

/// Everything Picard has removed so far, summed from the history log.
#[tauri::command]
pub async fn get_totals(state: SharedState<'_>) -> Result<Totals, String> {
    let shared = Arc::clone(&state);
    blocking(move || history::read_totals(&shared.paths.history).map_err(err)).await
}

/// Whether Picard opens at login (a LaunchAgent written by the autostart plugin).
#[tauri::command]
pub fn get_launch_at_login<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(err)
}

#[tauri::command]
pub fn set_launch_at_login<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    let result = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    result.map_err(err)?;
    log::info!("launch at login {}", if enabled { "on" } else { "off" });
    Ok(())
}

/// Asks for a newer version. With automatic updates on, installs it too.
/// A failure comes back as `UpdateStatus::Failed`, not as an error, so the
/// About tab and the background task show it the same way.
#[tauri::command]
pub async fn check_for_update<R: Runtime>(
    app: AppHandle<R>,
    updates: State<'_, Arc<Updates>>,
    state: SharedState<'_>,
) -> Result<UpdateStatus, String> {
    let auto_install = updater::auto_install_enabled(&state).await;
    Ok(updates.check(&app, auto_install).await)
}

/// Installs the update that `check_for_update` offered.
#[tauri::command]
pub async fn install_update<R: Runtime>(
    app: AppHandle<R>,
    updates: State<'_, Arc<Updates>>,
) -> Result<UpdateStatus, String> {
    Ok(updates.install_offered(&app).await)
}

/// Restarts Picard into the version that was installed.
#[tauri::command]
pub fn restart_to_update<R: Runtime>(
    app: AppHandle<R>,
    updates: State<'_, Arc<Updates>>,
    state: SharedState<'_>,
) -> Result<(), String> {
    updates.restart(&app, &state)
}

/// Opens one of the About tab's fixed pages in the default browser.
#[tauri::command]
pub async fn open_link(link: Link) -> Result<(), String> {
    blocking(move || about::open(link)).await
}

/// Shows the macOS folder picker as a sheet on the calling window. Returns the
/// chosen folder as it should appear in `roots` (`~/…` inside the home
/// folder), or `None` if the user cancelled.
#[tauri::command]
pub async fn pick_folder<R: Runtime>(
    window: WebviewWindow<R>,
    state: SharedState<'_>,
) -> Result<Option<String>, String> {
    let home = state.home.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_parent(&window)
        .set_title("Choose a folder for Picard to clean")
        .set_directory(&home)
        .pick_folder(move |picked| {
            // The receiver only goes away if the window closed meanwhile.
            let _ = tx.send(picked);
        });
    let Some(picked) = rx
        .await
        .map_err(|_| "the folder picker closed unexpectedly")?
    else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(err)?;
    Ok(Some(settings::display_root(&path, &home)))
}

/// Reads the config file through the same privacy check `config::load` uses.
fn read_config_text(shared: &Shared) -> Result<String, String> {
    use std::io::Read;
    let mut file = config::open_private(&shared.config_path)
        .map_err(|why| format!("not using {}: {why}", shared.config_path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(err)?;
    Ok(text)
}

fn status_view(
    config: &Config,
    readings: &[(Volume, DiskSpace)],
    unavailable: &[crate::volume::UnavailableRoot],
) -> Status {
    Status {
        mode: config.mode,
        volumes: readings
            .iter()
            .map(|(v, space)| VolumeStatus {
                name: v.name.clone(),
                mount: v.mount.display().to_string(),
                free_text: disk::format_free(*space),
                low: crate::monitor::is_low(config, v, *space),
            })
            .collect(),
        unavailable: unavailable
            .iter()
            .map(|u| format!("{}: {}", u.name, u.reason))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    }
}

/// The Clean tab's view of one volume's preview scan, which started at `scanned_at`.
pub(crate) fn preview_view(
    volume: &Volume,
    space: DiskSpace,
    scans: &[TaskScan],
    scanned_at: DateTime<Utc>,
) -> VolumePreview {
    let tasks: Vec<PreviewTask> = scans
        .iter()
        .map(|s| match &s.result {
            Ok(cands) => PreviewTask {
                task: s.task.to_string(),
                items: cands
                    .iter()
                    .map(|c| PreviewItem {
                        path: c.path.display().to_string(),
                        bytes: c.bytes,
                        size_text: c.size_text(),
                        reason: c.reason.clone(),
                        ignored: s.ignored.contains(&c.path),
                        holds_ignored: s.holds_ignored.contains(&c.path),
                    })
                    .collect(),
                error: None,
            },
            Err(e) => PreviewTask {
                task: s.task.to_string(),
                items: Vec::new(),
                error: Some(e.clone()),
            },
        })
        .collect();
    let total_bytes = scans.iter().map(TaskScan::cleanable_bytes).sum();
    VolumePreview {
        name: volume.name.clone(),
        mount: volume.mount.display().to_string(),
        free_text: disk::format_free(space),
        tasks,
        total_bytes,
        total_text: disk::format_bytes(total_bytes),
        scanned_at,
    }
}

fn run_view(name: &str, result: Result<VolumeRun, String>) -> RunResult {
    match result {
        Ok(run) => RunResult {
            name: name.to_string(),
            freed_text: disk::format_bytes(run.freed_bytes()),
            removed: run
                .removed
                .iter()
                .map(|(task, r)| RunItem {
                    task: task.to_string(),
                    path: r.path.display().to_string(),
                    size_text: disk::format_bytes(r.bytes),
                })
                .collect(),
            failed: run
                .failed
                .iter()
                .map(|(task, path, why)| RunFailure {
                    task: task.to_string(),
                    path: path.display().to_string(),
                    reason: why.clone(),
                })
                .collect(),
            skipped: run
                .skipped
                .iter()
                .map(|(task, path, why)| RunFailure {
                    task: task.to_string(),
                    path: path.display().to_string(),
                    reason: why.to_string(),
                })
                .collect(),
            stopped: run.stopped,
            error: None,
        },
        Err(e) => RunResult {
            name: name.to_string(),
            freed_text: disk::format_bytes(0),
            removed: Vec::new(),
            failed: Vec::new(),
            skipped: Vec::new(),
            stopped: None,
            error: Some(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{Candidate, Estimate, Removed};

    #[test]
    fn not_activity_lists_the_default_names() {
        let names = get_not_activity();
        assert_eq!(names, config::default_activity_ignore());
        assert!(names.contains(&"AGENTS.md".to_string()));
        assert!(names.contains(&".claude".to_string()));
    }

    #[test]
    fn preview_is_refused_only_while_a_clean_runs() {
        assert_eq!(refuse_while_cleaning(true), Err(CLEAN_RUNNING.to_string()));
        assert_eq!(refuse_while_cleaning(false), Ok(()));
    }

    fn volume() -> Volume {
        Volume {
            dev: 1,
            mount: PathBuf::from("/System/Volumes/Data"),
            name: "Startup disk".into(),
            roots: vec![],
            uuid: None,
        }
    }

    const SPACE: DiskSpace = DiskSpace {
        free_bytes: 20_000_000_000,
        total_bytes: 994_000_000_000,
    };

    #[test]
    fn preview_groups_by_task_with_total_and_errors_leaving_out_ignored_items() {
        let scans = vec![
            TaskScan {
                task: "node_modules",
                result: Ok(vec![
                    Candidate {
                        path: PathBuf::from("/w/a/node_modules"),
                        bytes: 2_000_000_000,
                        reason: "untouched for 40 days".into(),
                        estimate: Estimate::Exact,
                    },
                    Candidate {
                        path: PathBuf::from("/w/b/node_modules"),
                        bytes: 500_000_000,
                        reason: "untouched for 31 days".into(),
                        estimate: Estimate::Exact,
                    },
                    Candidate {
                        path: PathBuf::from("/w/c/node_modules"),
                        bytes: 9_000_000_000,
                        reason: "untouched for 90 days".into(),
                        estimate: Estimate::Exact,
                    },
                ]),
                ignored: [PathBuf::from("/w/c/node_modules")].into_iter().collect(),
                holds_ignored: Default::default(),
            },
            TaskScan {
                task: "tool_caches",
                result: Ok(vec![Candidate {
                    path: PathBuf::from("uv cache"),
                    bytes: 0,
                    reason: "runs uv cache prune".into(),
                    estimate: Estimate::UpTo(2_600_000_000),
                }]),
                ignored: Default::default(),
                holds_ignored: Default::default(),
            },
            TaskScan {
                task: "git_worktrees",
                result: Err("lsof broke".into()),
                ignored: Default::default(),
                holds_ignored: Default::default(),
            },
        ];
        let view = preview_view(&volume(), SPACE, &scans, Utc::now());
        assert_eq!(view.total_bytes, 2_500_000_000);
        assert_eq!(view.total_text, "2.5 GB");
        assert_eq!(view.free_text, "20 GB free (2%)");
        assert_eq!(view.tasks[0].items[1].size_text, "500 MB");
        assert!(!view.tasks[0].items[1].ignored);
        assert!(view.tasks[0].items[2].ignored, "ignored items stay listed");
        assert_eq!(view.tasks[1].items[0].size_text, "up to 2.6 GB");
        assert_eq!(view.tasks[2].error.as_deref(), Some("lsof broke"));
    }

    #[test]
    fn protection_view_reports_each_path_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("wt/node_modules");
        std::fs::create_dir_all(&kept).unwrap();
        let ignore = IgnoreList::new(std::slice::from_ref(&kept));
        let paths = [
            dir.path().join("wt"),
            kept.clone(),
            dir.path().join("other"),
        ]
        .map(|p| p.display().to_string());
        assert_eq!(
            protection_view(&ignore, &paths),
            vec![Some("holds_ignored"), Some("ignored"), None]
        );
    }

    #[test]
    fn run_view_reports_success_and_whole_volume_errors() {
        let run = VolumeRun {
            removed: vec![(
                "node_modules",
                Removed {
                    path: PathBuf::from("/w/a/node_modules"),
                    bytes: 2_000_000_000,
                },
            )],
            failed: vec![(
                "git_worktrees",
                PathBuf::from("/w/r/wt"),
                "now in use".into(),
            )],
            skipped: vec![(
                "node_modules",
                PathBuf::from("/w/k/node_modules"),
                "is ignored",
            )],
            stopped: None,
        };
        let ok = run_view("Startup disk", Ok(run));
        assert_eq!(ok.freed_text, "2.0 GB");
        assert_eq!(ok.failed[0].reason, "now in use");
        assert_eq!(ok.skipped[0].reason, "is ignored");
        let failed = run_view(
            "Startup disk",
            Err("another cleanup is already running".into()),
        );
        assert_eq!(
            failed.error.as_deref(),
            Some("another cleanup is already running")
        );
    }

    #[test]
    fn status_marks_low_volumes_and_dedupes_unavailable() {
        let config = config::parse(
            "min_free_gb = 30\nroots = [\"/a\"]\n",
            std::path::Path::new("/u"),
        )
        .unwrap();
        let un = |root: &str| crate::volume::UnavailableRoot {
            root: PathBuf::from(root),
            name: "Projects".into(),
            reason: "not mounted".into(),
        };
        let status = status_view(&config, &[(volume(), SPACE)], &[un("/V/P/a"), un("/V/P/b")]);
        assert!(status.volumes[0].low);
        assert_eq!(status.unavailable, vec!["Projects: not mounted"]);
        assert_eq!(status.mode, Mode::Ask);
    }
}
