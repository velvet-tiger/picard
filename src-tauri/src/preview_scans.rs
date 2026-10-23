//! The one preview scan behind the Clean tab and ask-mode notifications.
//!
//! A Scan click, a notification's estimate and the rescan after a background
//! clean all go through `scan`, so they follow the same rules: one scan at a
//! time, never alongside a clean, stored the same way, shown the same way, and
//! cancelled by the same Cancel button. The Clean tab cannot tell who started
//! a scan, and does not need to.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::commands::{self, VolumePreview};
use crate::config::Config;
use crate::env::ToolEnv;
use crate::poll;
use crate::runner::{self, ScanStopped, TaskScan};
use crate::state::Shared;
use crate::tasks::Stop;
use crate::volume::Volume;

/// Where the preview scan is. Mirrors `ScanPhase` in api.ts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ScanPhase {
    #[default]
    Idle,
    /// A scan was asked for while a clean runs, and starts when it ends.
    WaitingForClean {
        since: DateTime<Utc>,
    },
    Scanning {
        since: DateTime<Utc>,
    },
}

/// Who hears about a scan: the open window in the app, a recorder in tests.
pub trait ScanEvents {
    fn phase_changed(&self, phase: &ScanPhase);
    /// The stored previews changed; see `Shared::previews`.
    fn previews_changed(&self);
}

/// Which volumes a scan covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Every mounted volume that holds a root. Volumes not found are dropped
    /// from the store, because they are no longer mounted.
    Every,
    /// The volume mounted at this path, as `VolumePreview::mount` spells it.
    Volume(&'a str),
}

/// The shared scan's state, held in `Shared::scans`.
#[derive(Debug, Default)]
pub struct PreviewScans {
    /// Held for the whole of a scan, so only one runs at a time.
    turn: Mutex<()>,
    phase: Mutex<ScanPhase>,
    /// Bumped by `cancel`. A scan stops once this differs from the value it
    /// read when it was asked for, so Cancel also stops scans still queued.
    cancels: AtomicU64,
}

impl PreviewScans {
    /// Where the scan is now, for a window that opens mid-scan.
    pub fn phase(&self) -> ScanPhase {
        lock(&self.phase).clone()
    }

    /// Stops the running scan, and any asked for before now, within about a
    /// second. They store nothing. Harmless when no scan runs.
    pub fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }

    fn set_phase(&self, phase: ScanPhase, events: &dyn ScanEvents) {
        *lock(&self.phase) = phase.clone();
        events.phase_changed(&phase);
    }
}

/// A poisoned lock here guards a unit or a finished value, so it is still usable.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Puts the phase back to idle when a scan ends, however it ends.
struct IdleOnDrop<'a> {
    scans: &'a PreviewScans,
    events: &'a dyn ScanEvents,
}

impl Drop for IdleOnDrop<'_> {
    fn drop(&mut self) {
        self.scans.set_phase(ScanPhase::Idle, self.events);
    }
}

/// Scans `scope` with the config's tasks. See `scan_with`.
pub fn scan(
    shared: &Shared,
    config: &Config,
    tools: &ToolEnv,
    scope: Scope,
    events: &dyn ScanEvents,
) -> Result<Option<Vec<VolumePreview>>, String> {
    let scan_volume = |volume: &Volume, stop: Stop| {
        runner::preview_volume(config, volume, &shared.sizes, tools, stop)
    };
    scan_with(shared, &config.roots, scope, events, &scan_volume)
}

/// Waits for its turn and for any clean to end, then scans `scope` with
/// `scan_volume`, stores the result and tells `events`. Returns what it
/// found, or `None` if it was cancelled, in which case nothing is stored.
/// A failure on any volume fails the whole scan and stores nothing.
pub fn scan_with(
    shared: &Shared,
    roots: &[PathBuf],
    scope: Scope,
    events: &dyn ScanEvents,
    scan_volume: &dyn Fn(&Volume, Stop) -> Result<Vec<TaskScan>>,
) -> Result<Option<Vec<VolumePreview>>, String> {
    let scans = &shared.scans;
    let asked = scans.cancels.load(Ordering::SeqCst);
    let stop = move || scans.cancels.load(Ordering::SeqCst) != asked;
    let _turn = lock(&scans.turn);
    if stop() {
        return Ok(None);
    }
    let _idle = IdleOnDrop { scans, events };
    if shared.is_cleaning() {
        scans.set_phase(ScanPhase::WaitingForClean { since: Utc::now() }, events);
    }
    // A scan alongside a clean would repeat its work and list folders it is
    // about to delete. A clean that starts after this is still caught by the
    // store, which refuses scans that started before a clean finished.
    if !shared.wait_for_cleans(&stop) {
        return Ok(None);
    }
    let started = Utc::now();
    scans.set_phase(ScanPhase::Scanning { since: started }, events);
    log::info!("preview scan started ({scope:?})");
    let found = poll::scan(roots, &shared.uuids);
    let mut out = Vec::new();
    for (volume, space) in &found.readings {
        if let Scope::Volume(mount) = scope
            && volume.mount.to_string_lossy() != mount
        {
            continue;
        }
        match scan_volume(volume, &stop) {
            Ok(task_scans) => {
                out.push(commands::preview_view(volume, *space, &task_scans, started))
            }
            Err(e) if e.is::<ScanStopped>() => return Ok(None),
            Err(e) => return Err(format!("{}: {e:#}", volume.name)),
        }
    }
    if stop() {
        log::info!("preview scan cancelled");
        return Ok(None);
    }
    match scope {
        Scope::Every => shared.replace_previews(out.clone()),
        Scope::Volume(_) => out.iter().cloned().for_each(|v| shared.record_preview(v)),
    }
    events.previews_changed();
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        phases: Mutex<Vec<ScanPhase>>,
        stored: AtomicUsize,
    }

    impl ScanEvents for Recorder {
        fn phase_changed(&self, phase: &ScanPhase) {
            lock(&self.phases).push(phase.clone());
        }
        fn previews_changed(&self) {
            self.stored.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Recorder {
        fn states(&self) -> Vec<&'static str> {
            lock(&self.phases)
                .iter()
                .map(|p| match p {
                    ScanPhase::Idle => "idle",
                    ScanPhase::WaitingForClean { .. } => "waiting",
                    ScanPhase::Scanning { .. } => "scanning",
                })
                .collect()
        }
    }

    struct Setup {
        shared: Arc<Shared>,
        _dir: tempfile::TempDir,
        roots: Vec<PathBuf>,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir(&root).unwrap();
        Setup {
            shared: Arc::new(Shared::new(
                dir.path().to_path_buf(),
                dir.path().join("config.toml"),
            )),
            roots: vec![root],
            _dir: dir,
        }
    }

    fn empty(_: &Volume, _: Stop) -> Result<Vec<TaskScan>> {
        Ok(Vec::new())
    }

    #[test]
    fn a_scan_stores_its_result_and_reports_each_phase() {
        let s = setup();
        let events = Recorder::default();
        let found = scan_with(&s.shared, &s.roots, Scope::Every, &events, &empty)
            .unwrap()
            .expect("not cancelled");
        assert_eq!(found.len(), 1, "the temp root sits on one volume");
        assert_eq!(s.shared.previews(), found);
        assert_eq!(events.states(), vec!["scanning", "idle"]);
        assert_eq!(events.stored.load(Ordering::SeqCst), 1);
        assert_eq!(s.shared.scans.phase(), ScanPhase::Idle);
    }

    #[test]
    fn a_volume_scope_leaves_out_other_volumes() {
        let s = setup();
        let found = scan_with(
            &s.shared,
            &s.roots,
            Scope::Volume("/not/a/mount"),
            &Recorder::default(),
            &empty,
        )
        .unwrap()
        .unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn a_scan_waits_for_a_running_clean() {
        let s = setup();
        s.shared.enter_cleaning();
        let events = Arc::new(Recorder::default());
        let scanner = {
            let (shared, roots, events) =
                (Arc::clone(&s.shared), s.roots.clone(), Arc::clone(&events));
            std::thread::spawn(move || scan_with(&shared, &roots, Scope::Every, &*events, &empty))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(
            s.shared.scans.phase(),
            ScanPhase::WaitingForClean { .. }
        ));
        let clean_ended = Utc::now();
        s.shared.leave_cleaning();
        let found = scanner.join().unwrap().unwrap().unwrap();
        assert!(
            found[0].scanned_at >= clean_ended,
            "the start time is taken after the wait"
        );
        assert_eq!(events.states(), vec!["waiting", "scanning", "idle"]);
    }

    #[test]
    fn only_one_scan_runs_at_a_time() {
        let s = setup();
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..3)
            .map(|_| {
                let (shared, roots) = (Arc::clone(&s.shared), s.roots.clone());
                let (running, most) = (Arc::clone(&running), Arc::clone(&most));
                std::thread::spawn(move || {
                    let slow = move |_: &Volume, _: Stop| {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        running.fetch_sub(1, Ordering::SeqCst);
                        Ok(Vec::new())
                    };
                    scan_with(&shared, &roots, Scope::Every, &Recorder::default(), &slow)
                })
            })
            .collect();
        for t in threads {
            assert!(t.join().unwrap().unwrap().is_some());
        }
        assert_eq!(most.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cancelled_scan_stores_nothing() {
        let s = setup();
        let events = Arc::new(Recorder::default());
        let scanner = {
            let (shared, roots, events) =
                (Arc::clone(&s.shared), s.roots.clone(), Arc::clone(&events));
            std::thread::spawn(move || {
                let until_stopped = |_: &Volume, stop: Stop| {
                    while !stop() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(ScanStopped.into())
                };
                scan_with(&shared, &roots, Scope::Every, &*events, &until_stopped)
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        s.shared.scans.cancel();
        assert_eq!(scanner.join().unwrap(), Ok(None));
        assert!(s.shared.previews().is_empty());
        assert_eq!(events.stored.load(Ordering::SeqCst), 0);
        assert_eq!(s.shared.scans.phase(), ScanPhase::Idle);
    }

    #[test]
    fn cancel_stops_a_scan_waiting_for_a_clean() {
        let s = setup();
        s.shared.enter_cleaning();
        let scanner = {
            let (shared, roots) = (Arc::clone(&s.shared), s.roots.clone());
            std::thread::spawn(move || {
                let never = |_: &Volume, _: Stop| -> Result<Vec<TaskScan>> {
                    panic!("a cancelled scan must not start")
                };
                scan_with(&shared, &roots, Scope::Every, &Recorder::default(), &never)
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        s.shared.scans.cancel();
        assert_eq!(scanner.join().unwrap(), Ok(None));
        assert!(
            s.shared.is_cleaning(),
            "cancelling the scan leaves the clean alone"
        );
    }

    #[test]
    fn a_scan_asked_for_after_a_cancel_runs() {
        let s = setup();
        s.shared.scans.cancel();
        let found = scan_with(
            &s.shared,
            &s.roots,
            Scope::Every,
            &Recorder::default(),
            &empty,
        );
        assert!(found.unwrap().is_some());
    }

    #[test]
    fn a_failed_volume_fails_the_scan_and_stores_nothing() {
        let s = setup();
        let broken = |_: &Volume, _: Stop| -> Result<Vec<TaskScan>> { anyhow::bail!("no exclude") };
        let err = scan_with(
            &s.shared,
            &s.roots,
            Scope::Every,
            &Recorder::default(),
            &broken,
        )
        .unwrap_err();
        assert!(err.ends_with("no exclude"), "{err}");
        assert!(s.shared.previews().is_empty());
        assert_eq!(s.shared.scans.phase(), ScanPhase::Idle);
    }
}
