//! The poll loop: reads config, checks volumes, sends alerts, updates the menu.
//!
//! It wakes every `poll_minutes`, and also as soon as the config file changes or
//! a drive is mounted or unmounted under `/Volumes`. File-system watchers run
//! on their own thread and send `Wake` messages into the loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::mpsc;

use crate::config::{self, Config, Mode};
use crate::disk::{self, DiskSpace};
use crate::env::ToolEnv;
use crate::monitor::{self, States};
use crate::runner::{self, DataPaths};
use crate::tasks::SizeCache;
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
    sizes: Arc<SizeCache>,
    /// Resolved once when the loop starts; asking the login shell takes about a second.
    tools: Option<Arc<ToolEnv>>,
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
            sizes: Arc::new(SizeCache::default()),
            tools: None,
        }
    }

    /// Runs until the app exits or every watcher sender is dropped.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Wake>) {
        let home = self.home.clone();
        let shell = std::env::var_os("SHELL");
        match tauri::async_runtime::spawn_blocking(move || ToolEnv::detect(&home, shell)).await {
            Ok(env) => self.tools = Some(Arc::new(env)),
            Err(e) => log::error!("resolving tool PATH failed; cleanup tasks are off: {e}"),
        }
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
                let old_mode = self.config.as_ref().map(|old| old.mode);
                self.states = monitor::states_after_reload(
                    std::mem::take(&mut self.states),
                    old_mode,
                    c.mode,
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

    /// One check: scan, decide, alert (or clean, in auto mode), redraw.
    async fn check(&mut self) {
        let Some(config) = self.config.clone() else {
            self.show(MenuView {
                config_error: true,
                ..MenuView::default()
            });
            return;
        };
        let Some(mut found) = self.scan_volumes(&config).await else {
            return;
        };
        let (states, alerts) =
            monitor::evaluate(&self.states, &found.readings, &config, Instant::now());
        self.states = states;
        let mut cleaned = false;
        for alert in &alerts {
            if config.mode == Mode::Auto {
                let volume = found.readings.iter().find(|(v, _)| v.mount == alert.mount);
                if let Some((volume, _)) = volume {
                    self.clean(&config, volume.clone()).await;
                    cleaned = true;
                    continue;
                }
            }
            // TODO(M6): in ask mode, add the cleanable estimate and a Clean button.
            let (title, body) = alert.message();
            self.notify(&title, &body);
        }
        // Show the space after cleaning, not the reading that triggered it.
        if cleaned && let Some(after) = self.scan_volumes(&config).await {
            found = after;
        }
        self.show(MenuView {
            readings: found.readings,
            unavailable: found.unavailable,
            config_error: self.config_error,
        });
    }

    async fn scan_volumes(&self, config: &Config) -> Option<Scan> {
        let roots = config.roots.clone();
        match tauri::async_runtime::spawn_blocking(move || scan(&roots)).await {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("disk scan task failed: {e}");
                None
            }
        }
    }

    /// Runs the cleanup tasks on one volume and reports the result.
    async fn clean(&self, config: &Config, volume: Volume) {
        log::info!("auto mode: cleaning {}", volume.name);
        let Some(tools) = self.tools.clone() else {
            self.notify(
                &format!("{} is low", volume.name),
                "Cleanup is off because tool paths could not be resolved. See the log.",
            );
            return;
        };
        let config = config.clone();
        let sizes = Arc::clone(&self.sizes);
        let paths = DataPaths::beside(&self.config_path);
        let name = volume.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            let run = runner::clean_volume(&config, &volume, &sizes, &paths, &tools);
            let after = disk::query(&volume.mount).ok();
            (run, after)
        })
        .await;
        let (title, body) = match outcome {
            Ok((run, after)) => {
                let run = run.map_err(|e| format!("{e:#}"));
                if let Ok(r) = &run {
                    log::info!(
                        "auto mode: {name}: removed {}, failed {}, stopped {:?}",
                        r.removed.len(),
                        r.failed.len(),
                        r.stopped
                    );
                }
                runner::outcome_message(&name, &run, after)
            }
            Err(e) => {
                log::error!("cleanup task for {name} panicked or was cancelled: {e}");
                runner::outcome_message(&name, &Err(e.to_string()), None)
            }
        };
        self.notify(&title, &body);
    }

    fn notify(&self, title: &str, body: &str) {
        log::info!("alert: {title}: {body}");
        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
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
