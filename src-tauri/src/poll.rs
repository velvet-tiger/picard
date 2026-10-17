//! The poll loop: reads config, checks volumes, sends alerts, updates the menu.
//!
//! It wakes every `poll_minutes`, and also as soon as the config file changes or
//! a drive is mounted or unmounted under `/Volumes`. File-system watchers run
//! on their own thread and send `Wake` messages into the loop.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::mpsc;

use crate::config::{self, Config};
use crate::disk::{self, DiskSpace};
use crate::monitor::{self, Alert, States};
use crate::tray::{MenuView, Tray};
use crate::volume::{self, UnavailableRoot, Volume};

/// Why the loop woke up early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    ConfigChanged,
    VolumesChanged,
}

const VOLUMES_DIR: &str = "/Volumes";

// Editors and FSEvents emit several events per save or mount. Waiting briefly
// and draining the queue turns a burst into one check.
const DEBOUNCE: Duration = Duration::from_millis(500);

// Used only when there is no valid config yet, so a fixed config error still
// gets retried even if no file event arrives.
const FALLBACK_POLL: Duration = Duration::from_secs(5 * 60);

/// Writes the default config if none exists. Returns true if it wrote one.
pub fn ensure_config_file(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let dir = path
        .parent()
        .with_context(|| format!("config path {} has no parent folder", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(path, config::DEFAULT_TOML)
        .with_context(|| format!("writing default config to {}", path.display()))?;
    Ok(true)
}

/// Starts watching the config folder and `/Volumes`.
///
/// The returned watcher must be kept alive; dropping it stops the events.
/// The config folder is watched rather than the file, because many editors
/// save by replacing the file, which would end a watch on the file itself.
pub fn start_watchers(config_path: &Path, tx: mpsc::Sender<Wake>) -> Result<RecommendedWatcher> {
    let config_name = config_path.file_name().map(|n| n.to_os_string());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                log::warn!("file watcher error: {e}");
                return;
            }
        };
        for path in &event.paths {
            let wake = if path.starts_with(VOLUMES_DIR) {
                Wake::VolumesChanged
            } else if path.file_name().map(|n| n.to_os_string()) == config_name {
                Wake::ConfigChanged
            } else {
                continue;
            };
            // A full queue already holds a pending wake, so dropping this one loses nothing.
            let _ = tx.try_send(wake);
        }
    })
    .context("creating file watcher")?;

    let config_dir = config_path
        .parent()
        .with_context(|| format!("config path {} has no parent folder", config_path.display()))?;
    watcher
        .watch(config_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", config_dir.display()))?;
    watcher
        .watch(Path::new(VOLUMES_DIR), RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {VOLUMES_DIR}"))?;
    Ok(watcher)
}

/// Everything one check found.
struct Scan {
    readings: Vec<(Volume, DiskSpace)>,
    unavailable: Vec<UnavailableRoot>,
}

/// Maps roots to volumes and reads each volume's free space. Blocking I/O.
fn scan(roots: &[PathBuf]) -> Scan {
    let map = volume::map_roots(roots, volume::locate);
    let mut unavailable = map.unavailable;
    let mut readings = Vec::new();
    for v in map.volumes {
        match disk::query(&v.mount) {
            Ok(space) => readings.push((v, space)),
            // The drive can be unplugged between mapping and reading it.
            Err(e) => {
                log::warn!("reading {} failed: {e}", v.mount.display());
                unavailable.extend(v.roots.iter().map(|root| UnavailableRoot {
                    root: root.clone(),
                    name: v.name.clone(),
                    reason: "not readable".to_string(),
                }));
            }
        }
    }
    Scan {
        readings,
        unavailable,
    }
}

/// The loop's state. Owned by the loop task, never shared.
pub struct Poller<R: Runtime> {
    app: AppHandle<R>,
    tray: Tray<R>,
    home: PathBuf,
    config_path: PathBuf,
    config: Option<Config>,
    config_error: bool,
    states: States,
}

impl<R: Runtime> Poller<R> {
    pub fn new(app: AppHandle<R>, tray: Tray<R>, home: PathBuf, config_path: PathBuf) -> Self {
        Self {
            app,
            tray,
            home,
            config_path,
            config: None,
            config_error: false,
            states: States::new(),
        }
    }

    /// Runs until the app exits or every watcher sender is dropped.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Wake>) {
        self.reload_config();
        loop {
            self.check().await;
            let wait = self
                .config
                .as_ref()
                .map_or(FALLBACK_POLL, |c| Duration::from_secs(c.poll_minutes * 60));
            match tokio::time::timeout(wait, rx.recv()).await {
                Err(_elapsed) => {}
                Ok(None) => {
                    log::error!("file watcher channel closed; poll loop stopping");
                    return;
                }
                Ok(Some(first)) => {
                    tokio::time::sleep(DEBOUNCE).await;
                    let mut config_changed = first == Wake::ConfigChanged;
                    while let Ok(next) = rx.try_recv() {
                        config_changed |= next == Wake::ConfigChanged;
                    }
                    log::info!("woken early: config changed = {config_changed}");
                    if config_changed {
                        self.reload_config();
                    }
                }
            }
        }
    }

    /// Re-reads the config file. A bad file keeps the last good config running.
    fn reload_config(&mut self) {
        let loaded = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("reading {}", self.config_path.display()))
            .and_then(|text| config::parse(&text, &self.home));
        match loaded {
            Ok(c) => {
                log::info!(
                    "config loaded: mode {:?}, {} root(s)",
                    c.mode,
                    c.roots.len()
                );
                self.config = Some(c);
                self.config_error = false;
            }
            Err(e) => {
                let keeping = if self.config.is_some() {
                    "keeping the previous config"
                } else {
                    "not monitoring until it is fixed"
                };
                log::error!("config error, {keeping}: {e:#}");
                self.config_error = true;
            }
        }
    }

    /// One check: scan, decide, alert, redraw.
    async fn check(&mut self) {
        let Some(config) = self.config.clone() else {
            self.show(MenuView {
                config_error: true,
                ..MenuView::default()
            });
            return;
        };
        let roots = config.roots.clone();
        let scan = match tauri::async_runtime::spawn_blocking(move || scan(&roots)).await {
            Ok(s) => s,
            Err(e) => {
                log::error!("disk scan task failed: {e}");
                return;
            }
        };
        let (states, alerts) =
            monitor::evaluate(&self.states, &scan.readings, &config, Instant::now());
        self.states = states;
        for alert in &alerts {
            self.send(alert);
        }
        self.show(MenuView {
            readings: scan.readings,
            unavailable: scan.unavailable,
            config_error: self.config_error,
        });
    }

    // TODO(M3): in auto mode, run the cleanup tasks before notifying.
    // TODO(M6): in ask mode, add the cleanable estimate and a Clean button.
    fn send(&self, alert: &Alert) {
        let (title, body) = alert.message();
        log::info!("alert: {title}: {body}");
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(&title)
            .body(&body)
            .show()
        {
            log::error!("could not show notification {title:?}: {e}");
        }
    }

    fn show(&self, view: MenuView) {
        log::info!("status: {}", crate::tray::status_lines(&view).join("; "));
        if let Err(e) = self.tray.show(&view) {
            log::error!("could not update tray menu: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_config_file_writes_once_and_keeps_edits() {
        let dir = std::env::temp_dir().join(format!("picard-config-test-{}", std::process::id()));
        let path = dir.join("nested/config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(ensure_config_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            config::DEFAULT_TOML
        );

        std::fs::write(&path, "edited").unwrap();
        assert!(!ensure_config_file(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_reports_missing_root_as_not_mounted() {
        let s = scan(&[
            std::env::temp_dir(),
            PathBuf::from("/Volumes/picard-no-such-drive/x"),
        ]);
        assert_eq!(s.readings.len(), 1);
        assert_eq!(s.unavailable.len(), 1);
        assert_eq!(s.unavailable[0].name, "picard-no-such-drive");
    }
}
