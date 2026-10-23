//! The one preview scan behind the Clean tab and ask-mode notifications.
//!
//! A Scan click, a notification's estimate and the rescan after a background
//! clean all go through `scan`, so they follow the same rules: one scan at a
//! time, never alongside a clean, stored the same way, shown the same way, and
//! cancelled by the same Cancel button. The Clean tab shows every scan the
//! same way; a ticket only tells it which reply is its own.

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

/// Where the preview scan is. Mirrors `ScanPhase` in api.ts. `ticket` is the
/// number the Clean tab sent with its own Scan, and `None` for any other
/// scan, so the tab can tell its own scan from one it should follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ScanPhase {
    #[default]
    Idle,
    /// A scan was asked for while a clean runs, and starts when it ends.
    WaitingForClean {
        since: DateTime<Utc>,
        ticket: Option<u64>,
    },
    Scanning {
        since: DateTime<Utc>,
        ticket: Option<u64>,
    },
}

/// Who hears about a scan: the open window in the app, a recorder in tests.
pub trait ScanEvents {
    fn phase_changed(&self, phase: &ScanPhase);
    /// The stored previews changed; see `Shared::previews`.
    fn previews_changed(&self);
    /// A scan that got its turn has ended, whether it stored a result,
    /// failed or was cancelled. A scan can take minutes, so the free space
    /// on show is due for a fresh read.
    fn scan_ended(&self);
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

/// When a scan was asked for, as far as Cancel is concerned. See
/// `PreviewScans::mark`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelMark(u64);

/// One scan as asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    pub scope: Scope<'a>,
    /// Taken when the request arrived. See `PreviewScans::mark`.
    pub mark: CancelMark,
    /// See `ScanPhase`.
    pub ticket: Option<u64>,
}

/// The shared scan's state, held in `Shared::scans`.
#[derive(Debug, Default)]
pub struct PreviewScans {
    /// Held for the whole of a scan, so only one runs at a time.
    turn: Mutex<()>,
    phase: Mutex<ScanPhase>,
    /// Bumped by `cancel`. A scan stops once this differs from its mark, so
    /// Cancel also stops scans still queued for their turn.
    cancels: AtomicU64,
}

impl PreviewScans {
    /// Where the scan is now, for a window that opens mid-scan.
    pub fn phase(&self) -> ScanPhase {
        lock(&self.phase).clone()
    }

    /// Marks a scan as asked for now. Take it as soon as the request arrives,
    /// before any slow work, so a Cancel pressed straight after the request
    /// still stops it.
    pub fn mark(&self) -> CancelMark {
        CancelMark(self.cancels.load(Ordering::SeqCst))
    }

    /// Stops every scan marked before now, running or queued, within about a
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

/// Puts the phase back to idle when a scan ends, however it ends, and
/// reports the end.
struct IdleOnDrop<'a> {
    scans: &'a PreviewScans,
    events: &'a dyn ScanEvents,
}

impl Drop for IdleOnDrop<'_> {
    fn drop(&mut self) {
        self.scans.set_phase(ScanPhase::Idle, self.events);
        self.events.scan_ended();
    }
}

/// Scans with the config's tasks. See `scan_with`.
pub fn scan(
    shared: &Shared,
    config: &Config,
    tools: &ToolEnv,
    request: Request,
    events: &dyn ScanEvents,
) -> Result<Option<Vec<VolumePreview>>, String> {
    let scan_volume = |volume: &Volume, stop: Stop| {
        runner::preview_volume(config, volume, &shared.sizes, tools, stop)
    };
    scan_with(shared, &config.roots, request, events, &scan_volume)
}

/// Waits for its turn and for any clean to end, then scans the request's
/// volumes with `scan_volume`, stores the result and tells `events`. Returns
/// what it found, or `None` if it was cancelled, in which case nothing is
/// stored. A failure on any volume fails the whole scan and stores nothing.
pub fn scan_with(
    shared: &Shared,
    roots: &[PathBuf],
    request: Request,
    events: &dyn ScanEvents,
    scan_volume: &dyn Fn(&Volume, Stop) -> Result<Vec<TaskScan>>,
) -> Result<Option<Vec<VolumePreview>>, String> {
    let Request {
        scope,
        mark,
        ticket,
    } = request;
    let scans = &shared.scans;
    let stop = move || scans.mark() != mark;
    let _turn = lock(&scans.turn);
    if stop() {
        return Ok(None);
    }
    let _idle = IdleOnDrop { scans, events };
    if shared.is_cleaning() {
        let since = Utc::now();
        scans.set_phase(ScanPhase::WaitingForClean { since, ticket }, events);
    }
    // A scan alongside a clean would repeat its work and list folders it is
    // about to delete. A clean that starts after this is still caught by the
    // store, which refuses scans that started before a clean finished.
    if !shared.wait_for_cleans(&stop) {
        return Ok(None);
    }
    let started = Utc::now();
    let phase = ScanPhase::Scanning {
        since: started,
        ticket,
    };
    scans.set_phase(phase, events);
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
        ended: AtomicUsize,
    }

    impl ScanEvents for Recorder {
        fn phase_changed(&self, phase: &ScanPhase) {
            lock(&self.phases).push(phase.clone());
        }
        fn previews_changed(&self) {
            self.stored.fetch_add(1, Ordering::SeqCst);
        }
        fn scan_ended(&self) {
            self.ended.fetch_add(1, Ordering::SeqCst);
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

    /// A request for every volume, marked now, from no ticket.
    fn every(shared: &Shared) -> Request<'static> {
        Request {
            scope: Scope::Every,
            mark: shared.scans.mark(),
            ticket: None,
        }
    }

    fn until_stopped(_: &Volume, stop: Stop) -> Result<Vec<TaskScan>> {
        while !stop() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ScanStopped.into())
    }

    fn never(_: &Volume, _: Stop) -> Result<Vec<TaskScan>> {
        panic!("a cancelled scan must not start")
    }

    fn empty(_: &Volume, _: Stop) -> Result<Vec<TaskScan>> {
        Ok(Vec::new())
    }

    #[test]
    fn a_scan_stores_its_result_and_reports_each_phase() {
        let s = setup();
        let events = Recorder::default();
        let found = scan_with(&s.shared, &s.roots, every(&s.shared), &events, &empty)
            .unwrap()
            .expect("not cancelled");
        assert_eq!(found.len(), 1, "the temp root sits on one volume");
        assert_eq!(s.shared.previews(), found);
        assert_eq!(events.states(), vec!["scanning", "idle"]);
        assert_eq!(events.stored.load(Ordering::SeqCst), 1);
        assert_eq!(events.ended.load(Ordering::SeqCst), 1);
        assert_eq!(s.shared.scans.phase(), ScanPhase::Idle);
    }

    #[test]
    fn a_volume_scope_leaves_out_other_volumes() {
        let s = setup();
        let found = scan_with(
            &s.shared,
            &s.roots,
            Request {
                scope: Scope::Volume("/not/a/mount"),
                mark: s.shared.scans.mark(),
                ticket: None,
            },
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
            std::thread::spawn(move || scan_with(&shared, &roots, every(&shared), &*events, &empty))
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
                    scan_with(&shared, &roots, every(&shared), &Recorder::default(), &slow)
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
                scan_with(&shared, &roots, every(&shared), &*events, &until_stopped)
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        s.shared.scans.cancel();
        assert_eq!(scanner.join().unwrap(), Ok(None));
        assert!(s.shared.previews().is_empty());
        assert_eq!(events.stored.load(Ordering::SeqCst), 0);
        assert_eq!(events.ended.load(Ordering::SeqCst), 1);
        assert_eq!(s.shared.scans.phase(), ScanPhase::Idle);
    }

    #[test]
    fn cancel_stops_a_scan_waiting_for_a_clean() {
        let s = setup();
        s.shared.enter_cleaning();
        let scanner = {
            let (shared, roots) = (Arc::clone(&s.shared), s.roots.clone());
            std::thread::spawn(move || {
                scan_with(
                    &shared,
                    &roots,
                    every(&shared),
                    &Recorder::default(),
                    &never,
                )
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
            every(&s.shared),
            &Recorder::default(),
            &empty,
        );
        assert!(found.unwrap().is_some());
    }

    #[test]
    fn a_failed_volume_fails_the_scan_and_stores_nothing() {
        let s = setup();
        let broken = |_: &Volume, _: Stop| -> Result<Vec<TaskScan>> { anyhow::bail!("no exclude") };
        let events = Recorder::default();
        let err = scan_with(&s.shared, &s.roots, every(&s.shared), &events, &broken).unwrap_err();
        assert!(err.ends_with("no exclude"), "{err}");
        assert!(s.shared.previews().is_empty());
        assert_eq!(events.ended.load(Ordering::SeqCst), 1);
        assert_eq!(s.shared.scans.phase(), ScanPhase::Idle);
    }

    #[test]
    fn the_phase_carries_the_ticket_of_the_request() {
        let s = setup();
        let events = Recorder::default();
        let request = Request {
            ticket: Some(41),
            ..every(&s.shared)
        };
        scan_with(&s.shared, &s.roots, request, &events, &empty).unwrap();
        let phases = lock(&events.phases).clone();
        assert!(
            matches!(
                phases[0],
                ScanPhase::Scanning {
                    ticket: Some(41),
                    ..
                }
            ),
            "{phases:?}"
        );
    }

    #[test]
    fn a_cancel_before_the_request_reaches_its_thread_still_stops_it() {
        let s = setup();
        let mark = s.shared.scans.mark();
        s.shared.scans.cancel();
        let request = Request {
            mark,
            ..every(&s.shared)
        };
        let events = Recorder::default();
        let found = scan_with(&s.shared, &s.roots, request, &events, &never);
        assert_eq!(found, Ok(None));
        // It never got its turn, so there is no end to report.
        assert_eq!(events.ended.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancel_stops_a_scan_queued_behind_a_running_one() {
        let s = setup();
        let running = {
            let (shared, roots) = (Arc::clone(&s.shared), s.roots.clone());
            std::thread::spawn(move || {
                let request = every(&shared);
                scan_with(
                    &shared,
                    &roots,
                    request,
                    &Recorder::default(),
                    &until_stopped,
                )
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let queued = {
            let (shared, roots) = (Arc::clone(&s.shared), s.roots.clone());
            std::thread::spawn(move || {
                scan_with(
                    &shared,
                    &roots,
                    every(&shared),
                    &Recorder::default(),
                    &never,
                )
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!queued.is_finished(), "the second scan waits for its turn");
        s.shared.scans.cancel();
        assert_eq!(running.join().unwrap(), Ok(None));
        assert_eq!(queued.join().unwrap(), Ok(None));
        assert!(s.shared.previews().is_empty());
    }

    /// A clean finds its candidates again just before removing them. Cancel
    /// is for scans only, so it must not cut that re-check short: the clean
    /// builds its own context, which nothing can stop.
    #[test]
    fn a_cancel_does_not_reach_a_clean() {
        let s = setup();
        let root = &s.roots[0];
        let modules = root.join("web/node_modules");
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(root.join("web/package.json"), "{}").unwrap();
        std::fs::write(modules.join("x.js"), vec![b'x'; 1000]).unwrap();
        let old = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(60 * 86_400),
        );
        for e in walkdir::WalkDir::new(root.join("web")).contents_first(true) {
            filetime::set_symlink_file_times(e.unwrap().path(), old, old).unwrap();
        }
        let text = format!(
            "min_free_gb = 1\nroots = [{:?}]\n[[task]]\ntype = \"node_modules\"\nstale_days = 30\n",
            root.display().to_string()
        );
        let config = crate::config::parse(&text, s._dir.path()).unwrap();
        let found = poll::scan(&config.roots, &s.shared.uuids);
        let (volume, _) = &found.readings[0];
        let tools = ToolEnv::from_dirs(s._dir.path(), Vec::new());

        s.shared.scans.cancel();
        let run = runner::clean_volume(
            &config,
            volume,
            &s.shared.sizes,
            &s.shared.paths,
            &tools,
            runner::CleanRequest::Everything,
            &|| Ok(Vec::new()),
        )
        .unwrap();
        assert_eq!(run.removed.len(), 1, "{run:?}");
        assert!(!modules.exists());
    }
}
