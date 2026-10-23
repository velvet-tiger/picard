//! Decides when a volume is low enough to act on.
//!
//! Each volume is either `Armed` (ready to fire) or `Fired` (already alerted).
//! A fired volume fires again only after it recovers above
//! `threshold + recover_margin`, or after `cooldown` while still low. Without
//! this, a cleanup that cannot free enough space would alert on every poll.
//!
//! The repeat after `cooldown` waits for a routine check, one that comes on
//! the `poll_minutes` beat. Picard also checks when the menu opens, when a
//! scan ends and every 30 seconds while its window is open. Those checks
//! catch a volume that newly drops below its threshold, but a repeat on them
//! could chain: an alert's own estimate scan ends, which asks for a check,
//! which repeats the alert once a short cooldown has passed.
//!
//! Everything here is pure. The poll loop owns the clock and the I/O.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

use crate::config::{Config, Mode};
use crate::disk::DiskSpace;
use crate::volume::Volume;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Armed,
    Fired { at: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Quiet,
    Fire,
}

/// The rules for one volume, resolved to bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub threshold_bytes: u64,
    pub margin_bytes: u64,
    pub cooldown: Duration,
    pub mode: Mode,
}

/// Advances one volume's state given a new free-space reading.
///
/// `routine` is true on a routine check; only those may repeat an alert
/// after the cooldown. See the module notes.
pub fn step(
    state: State,
    free_bytes: u64,
    policy: &Policy,
    now: Instant,
    routine: bool,
) -> (State, Decision) {
    // Off keeps the volume armed, so switching back to ask or auto alerts at once
    // if the volume is still low.
    if policy.mode == Mode::Off {
        return (State::Armed, Decision::Quiet);
    }
    let low = free_bytes < policy.threshold_bytes;
    match state {
        State::Armed if low => (State::Fired { at: now }, Decision::Fire),
        State::Armed => (State::Armed, Decision::Quiet),
        State::Fired { .. }
            if free_bytes >= policy.threshold_bytes.saturating_add(policy.margin_bytes) =>
        {
            (State::Armed, Decision::Quiet)
        }
        State::Fired { at } if routine && low && now.duration_since(at) >= policy.cooldown => {
            (State::Fired { at: now }, Decision::Fire)
        }
        fired @ State::Fired { .. } => (fired, Decision::Quiet),
    }
}

/// A volume that should be alerted on, with the numbers for the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub name: String,
    pub mount: PathBuf,
    pub space: DiskSpace,
}

impl Alert {
    /// Notification title and body, for example "Startup disk is low" and
    /// "19 GB free (2%). About 8.5 GB can be cleaned."
    ///
    /// `cleanable` is the preview total, or `None` if the scan could not run;
    /// then the body gives only the free space rather than a guess.
    pub fn message(&self, cleanable: Option<u64>) -> (String, String) {
        let free = crate::disk::format_free(self.space);
        let body = match cleanable {
            None => free,
            Some(0) => format!("{free}. Nothing Picard can clean right now."),
            Some(bytes) => format!(
                "{free}. About {} can be cleaned.",
                crate::disk::format_bytes(bytes)
            ),
        };
        (format!("{} is low", self.name), body)
    }
}

/// Notification title and body for the daily scan in ask mode, for example
/// "Daily scan of Startup disk" and "120 GB free (12%). About 8.5 GB can be
/// cleaned." `None` when nothing can be cleaned or the estimate failed, since
/// a daily scan of a healthy disk with nothing to offer is not worth a ping.
pub fn daily_scan_message(
    name: &str,
    space: DiskSpace,
    cleanable: Option<u64>,
) -> Option<(String, String)> {
    let bytes = cleanable.filter(|&b| b > 0)?;
    Some((
        format!("Daily scan of {name}"),
        format!(
            "{}. About {} can be cleaned.",
            crate::disk::format_free(space),
            crate::disk::format_bytes(bytes)
        ),
    ))
}

/// True if `space` on `volume` is below that volume's threshold.
pub fn is_low(config: &Config, volume: &Volume, space: DiskSpace) -> bool {
    space.free_bytes
        < config
            .threshold_for(&volume.mount, volume.uuid.as_deref())
            .bytes(space.total_bytes)
}

/// Per-volume states, keyed by mount point. Mount point is used rather than
/// `st_dev` because device numbers can be reused after a drive is remounted.
pub type States = HashMap<PathBuf, State>;

/// States to keep after a config reload.
///
/// A mode change re-arms every volume, so switching from ask to auto acts on a
/// disk that is already low instead of waiting out the cooldown. Any other
/// change keeps the states, so editing a threshold does not repeat an alert.
pub fn states_after_reload(states: States, old_mode: Option<Mode>, new_mode: Mode) -> States {
    if old_mode == Some(new_mode) {
        states
    } else {
        States::new()
    }
}

/// Evaluates every reading against the config.
///
/// Returns the new states and the alerts to send. Volumes missing from
/// `readings` are dropped, so a drive that is unplugged and plugged back in
/// starts armed. `routine` is passed to `step`.
pub fn evaluate(
    states: &States,
    readings: &[(Volume, DiskSpace)],
    config: &Config,
    now: Instant,
    routine: bool,
) -> (States, Vec<Alert>) {
    let mut next = States::new();
    let mut alerts = Vec::new();
    for (volume, space) in readings {
        let policy = Policy {
            threshold_bytes: config
                .threshold_for(&volume.mount, volume.uuid.as_deref())
                .bytes(space.total_bytes),
            margin_bytes: config.recover_margin_bytes,
            cooldown: Duration::from_secs(config.cooldown_minutes.saturating_mul(60)),
            mode: config.mode,
        };
        let prev = states.get(&volume.mount).copied().unwrap_or(State::Armed);
        let (state, decision) = step(prev, space.free_bytes, &policy, now, routine);
        next.insert(volume.mount.clone(), state);
        if decision == Decision::Fire {
            alerts.push(Alert {
                name: volume.name.clone(),
                mount: volume.mount.clone(),
                space: *space,
            });
        }
    }
    (next, alerts)
}

/// Whether the daily scan should run now.
///
/// It is due once the local time reaches `at`, unless it already ran today.
/// So a scan missed while the Mac slept or Picard was closed runs at the next
/// check that day, and never twice in one day. `last` is the local date of
/// the last scan; a date after today (the clock moved back, or a flight west
/// across time zones) counts as not today, so at most one extra scan can follow.
pub fn daily_scan_due(at: NaiveTime, now: NaiveDateTime, last: Option<NaiveDate>) -> bool {
    now.time() >= at && last != Some(now.date())
}

/// Added to the wait for the daily scan so the loop wakes just after the
/// scheduled minute starts rather than a moment before it.
pub const DAILY_SCAN_SLACK: Duration = Duration::from_secs(1);

/// How long the poll loop sleeps: until the next poll or the next daily scan,
/// whichever is sooner. `daily` is `None` when the daily scan is off.
pub fn next_wait(
    poll: Duration,
    daily: Option<NaiveTime>,
    now: NaiveDateTime,
    last: Option<NaiveDate>,
) -> Duration {
    match daily {
        Some(at) => poll.min(until_daily_scan(at, now, last) + DAILY_SCAN_SLACK),
        None => poll,
    }
}

/// The last-scan date to keep after `daily_scan` changes from `old` to `new`.
///
/// Turning the scan on, or moving its time, once that time has already passed
/// today counts today as done. The user asked for a scheduled scan, so saving
/// the setting at 14:00 must not start a full cleanup straight away; the first
/// scan waits for the next scheduled time. Any other change keeps `last`.
pub fn last_scan_after_change(
    old: Option<NaiveTime>,
    new: Option<NaiveTime>,
    now: NaiveDateTime,
    last: Option<NaiveDate>,
) -> Option<NaiveDate> {
    match new {
        Some(at) if old != new && now.time() >= at => Some(now.date()),
        _ => last,
    }
}

/// What the daily scan does with one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyAction {
    /// Auto mode: run every task in full.
    Clean,
    /// Ask mode: preview, and notify if anything can be cleaned.
    Offer,
    /// Nothing: off mode, or a low volume in ask mode that already had its
    /// low-space alert (the cooldown decides when it gets the next one).
    Skip,
}

/// Chooses the daily scan's action for a volume in `state` under `mode`.
pub fn daily_action(mode: Mode, state: Option<State>) -> DailyAction {
    match (mode, state) {
        (Mode::Off, _) => DailyAction::Skip,
        (Mode::Auto, _) => DailyAction::Clean,
        (Mode::Ask, Some(State::Fired { .. })) => DailyAction::Skip,
        (Mode::Ask, _) => DailyAction::Offer,
    }
}

/// How long until the daily scan is next due. Zero if it is due now.
///
/// Uses wall-clock local time without time zones, so across a daylight saving
/// change the wait can be an hour off. That is harmless: the poll loop also
/// wakes every `poll_minutes` and checks `daily_scan_due` each time.
pub fn until_daily_scan(at: NaiveTime, now: NaiveDateTime, last: Option<NaiveDate>) -> Duration {
    if daily_scan_due(at, now, last) {
        return Duration::ZERO;
    }
    let today = now.date();
    let date = if now.time() < at && last != Some(today) {
        today
    } else {
        today.succ_opt().unwrap_or(today)
    };
    (date.and_time(at) - now).to_std().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use std::path::Path;

    const GB: u64 = 1_000_000_000;

    fn policy(mode: Mode) -> Policy {
        Policy {
            threshold_bytes: 30 * GB,
            margin_bytes: 10 * GB,
            cooldown: Duration::from_secs(3600),
            mode,
        }
    }

    /// Feeds (free GB, minutes since start) readings to routine checks and
    /// returns which ones fired.
    fn run(readings: &[(u64, u64)], mode: Mode) -> Vec<bool> {
        let t0 = Instant::now();
        let p = policy(mode);
        let mut state = State::Armed;
        readings
            .iter()
            .map(|&(free_gb, minute)| {
                let now = t0 + Duration::from_secs(minute * 60);
                let (s, d) = step(state, free_gb * GB, &p, now, true);
                state = s;
                d == Decision::Fire
            })
            .collect()
    }

    #[test]
    fn fires_once_while_staying_low() {
        let fired = run(&[(50, 0), (25, 5), (24, 10), (20, 15), (29, 20)], Mode::Ask);
        assert_eq!(fired, vec![false, true, false, false, false]);
    }

    #[test]
    fn does_not_rearm_inside_the_margin() {
        // 35 GB is above the 30 GB threshold but below threshold + margin (40 GB).
        let fired = run(&[(25, 0), (35, 5), (25, 10)], Mode::Ask);
        assert_eq!(fired, vec![true, false, false]);
    }

    #[test]
    fn rearms_after_recovering_past_the_margin() {
        let fired = run(&[(25, 0), (40, 5), (25, 10)], Mode::Ask);
        assert_eq!(fired, vec![true, false, true]);
    }

    #[test]
    fn fires_again_after_cooldown_if_still_low() {
        let fired = run(
            &[(25, 0), (25, 59), (25, 60), (25, 61), (25, 120)],
            Mode::Ask,
        );
        assert_eq!(fired, vec![true, false, true, false, true]);
    }

    #[test]
    fn cooldown_alone_does_not_fire_when_no_longer_low() {
        let fired = run(&[(25, 0), (35, 90)], Mode::Ask);
        assert_eq!(fired, vec![true, false]);
    }

    #[test]
    fn exactly_at_threshold_is_not_low() {
        assert_eq!(run(&[(30, 0)], Mode::Ask), vec![false]);
    }

    #[test]
    fn auto_mode_fires_like_ask() {
        assert_eq!(run(&[(25, 0), (25, 5)], Mode::Auto), vec![true, false]);
    }

    #[test]
    fn only_a_routine_check_repeats_an_alert() {
        let t0 = Instant::now();
        let no_cooldown = Policy {
            cooldown: Duration::ZERO,
            ..policy(Mode::Ask)
        };
        let fired = State::Fired { at: t0 };
        let later = t0 + Duration::from_secs(5);
        assert_eq!(
            step(fired, 25 * GB, &no_cooldown, later, false),
            (fired, Decision::Quiet),
            "a refresh while still low stays quiet"
        );
        assert_eq!(
            step(fired, 25 * GB, &no_cooldown, later, true),
            (State::Fired { at: later }, Decision::Fire)
        );
    }

    #[test]
    fn any_check_catches_a_new_drop_and_a_recovery() {
        let t0 = Instant::now();
        let p = policy(Mode::Ask);
        assert_eq!(
            step(State::Armed, 25 * GB, &p, t0, false),
            (State::Fired { at: t0 }, Decision::Fire)
        );
        assert_eq!(
            step(State::Fired { at: t0 }, 40 * GB, &p, t0, false),
            (State::Armed, Decision::Quiet)
        );
    }

    #[test]
    fn off_mode_never_fires_and_stays_armed() {
        let t0 = Instant::now();
        let (s, d) = step(State::Fired { at: t0 }, 0, &policy(Mode::Off), t0, true);
        assert_eq!((s, d), (State::Armed, Decision::Quiet));
        assert_eq!(run(&[(1, 0), (1, 500)], Mode::Off), vec![false, false]);
    }

    fn volume(mount: &str, name: &str) -> Volume {
        Volume {
            dev: 1,
            mount: PathBuf::from(mount),
            name: name.to_string(),
            roots: vec![],
            uuid: None,
        }
    }

    fn space(free_gb: u64, total_gb: u64) -> DiskSpace {
        DiskSpace {
            free_bytes: free_gb * GB,
            total_bytes: total_gb * GB,
        }
    }

    #[test]
    fn evaluate_uses_per_volume_override_and_only_alerts_low_volume() {
        let config = parse(
            "min_free_gb = 30\nroots = [\"/a\"]\n[[volume]]\nmount = \"/Volumes/P\"\nmin_free_pct = 5\n",
            Path::new("/Users/t"),
        )
        .unwrap();
        // Startup: 20 GB free is below 30 GB. Projects: 20 of 1000 GB is 2%, below 5%.
        // Big: 60 GB free is fine.
        let readings = vec![
            (volume("/", "Startup disk"), space(20, 500)),
            (volume("/Volumes/P", "P"), space(20, 1000)),
            (volume("/Volumes/Big", "Big"), space(60, 1000)),
        ];
        let now = Instant::now();
        let (states, alerts) = evaluate(&States::new(), &readings, &config, now, false);
        let names: Vec<_> = alerts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Startup disk", "P"]);
        assert_eq!(states[Path::new("/Volumes/Big")], State::Armed);

        // Second pass: still low, but already fired, so nothing new.
        let (_, alerts) = evaluate(
            &states,
            &readings,
            &config,
            now + Duration::from_secs(300),
            true,
        );
        assert!(alerts.is_empty());
    }

    #[test]
    fn evaluate_repeats_after_the_cooldown_only_when_routine() {
        let config = parse(
            "min_free_gb = 30\ncooldown_minutes = 0\nroots = [\"/a\"]",
            Path::new("/Users/t"),
        )
        .unwrap();
        let readings = vec![(volume("/", "Startup disk"), space(20, 500))];
        let now = Instant::now();
        let (states, alerts) = evaluate(&States::new(), &readings, &config, now, true);
        assert_eq!(alerts.len(), 1);
        let later = now + Duration::from_secs(1);
        let (states, alerts) = evaluate(&states, &readings, &config, later, false);
        assert!(alerts.is_empty(), "a refresh does not repeat the alert");
        let (_, alerts) = evaluate(&states, &readings, &config, later, true);
        assert_eq!(alerts.len(), 1, "the routine check does");
    }

    #[test]
    fn mode_change_rearms_but_other_edits_do_not() {
        let mut states = States::new();
        states.insert(PathBuf::from("/"), State::Fired { at: Instant::now() });
        let kept = states_after_reload(states.clone(), Some(Mode::Ask), Mode::Ask);
        assert_eq!(kept.len(), 1);
        assert!(states_after_reload(states, Some(Mode::Ask), Mode::Auto).is_empty());
    }

    #[test]
    fn alert_message_names_the_volume() {
        let alert = Alert {
            name: "Startup disk".into(),
            mount: PathBuf::from("/"),
            space: space(19, 994),
        };
        let title = "Startup disk is low".to_string();
        assert_eq!(
            alert.message(None),
            (title.clone(), "19 GB free (2%)".to_string())
        );
        assert_eq!(
            alert.message(Some(8_500_000_000)),
            (
                title.clone(),
                "19 GB free (2%). About 8.5 GB can be cleaned.".to_string()
            )
        );
        assert_eq!(
            alert.message(Some(0)).1,
            "19 GB free (2%). Nothing Picard can clean right now."
        );
    }

    #[test]
    fn daily_scan_message_only_when_something_can_be_cleaned() {
        assert_eq!(daily_scan_message("D", space(120, 994), None), None);
        assert_eq!(daily_scan_message("D", space(120, 994), Some(0)), None);
        assert_eq!(
            daily_scan_message("Startup disk", space(120, 994), Some(8_500_000_000)),
            Some((
                "Daily scan of Startup disk".to_string(),
                "120 GB free (12%). About 8.5 GB can be cleaned.".to_string()
            ))
        );
    }

    #[test]
    fn evaluate_drops_volumes_that_disappear() {
        let config = parse("min_free_gb = 30\nroots = [\"/a\"]", Path::new("/Users/t")).unwrap();
        let mut states = States::new();
        states.insert(
            PathBuf::from("/Volumes/Gone"),
            State::Fired { at: Instant::now() },
        );
        let (next, _) = evaluate(&states, &[], &config, Instant::now(), true);
        assert!(next.is_empty());
    }

    fn at(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    fn when(d: u32, hour: u32, minute: u32) -> NaiveDateTime {
        day(d).and_time(at(hour, minute))
    }

    #[test]
    fn daily_scan_is_due_from_its_time_until_it_runs() {
        assert!(!daily_scan_due(at(3, 0), when(20, 2, 59), None));
        assert!(daily_scan_due(at(3, 0), when(20, 3, 0), None));
        assert!(daily_scan_due(at(3, 0), when(20, 3, 0), Some(day(19))));
        assert!(!daily_scan_due(at(3, 0), when(20, 3, 0), Some(day(20))));
    }

    #[test]
    fn missed_daily_scan_runs_later_the_same_day() {
        // The Mac slept through 03:00 and woke at 14:00.
        assert!(daily_scan_due(at(3, 0), when(20, 14, 0), Some(day(19))));
        assert!(!daily_scan_due(at(3, 0), when(20, 23, 59), Some(day(20))));
    }

    #[test]
    fn clock_moved_back_past_last_scan_still_allows_a_scan() {
        assert!(daily_scan_due(at(3, 0), when(20, 4, 0), Some(day(21))));
    }

    #[test]
    fn waits_until_today_or_tomorrow() {
        let hour = Duration::from_secs(3600);
        assert_eq!(
            until_daily_scan(at(3, 0), when(20, 1, 0), Some(day(19))),
            2 * hour
        );
        // Already ran today: next is tomorrow at 03:00.
        assert_eq!(
            until_daily_scan(at(3, 0), when(20, 4, 0), Some(day(20))),
            23 * hour
        );
        assert_eq!(
            until_daily_scan(at(3, 0), when(20, 4, 0), None),
            Duration::ZERO
        );
    }

    #[test]
    fn next_wait_is_the_sooner_of_poll_and_daily_scan() {
        let poll = Duration::from_secs(300);
        let now = when(20, 2, 58);
        assert_eq!(next_wait(poll, None, now, None), poll);
        assert_eq!(
            next_wait(poll, Some(at(3, 0)), now, None),
            Duration::from_secs(121)
        );
        assert_eq!(
            next_wait(poll, Some(at(3, 0)), when(20, 4, 0), Some(day(20))),
            poll
        );
        // Due now: wakes after the slack only, never with a zero wait.
        assert_eq!(
            next_wait(poll, Some(at(3, 0)), when(20, 4, 0), None),
            DAILY_SCAN_SLACK
        );
    }

    #[test]
    fn enabling_after_the_time_has_passed_waits_for_tomorrow() {
        let now = when(20, 14, 0);
        // Turned on at 14:00 for 03:00: today counts as done.
        assert_eq!(
            last_scan_after_change(None, Some(at(3, 0)), now, None),
            Some(day(20))
        );
        // Moved from 22:00 to 03:00 at 14:00: same.
        assert_eq!(
            last_scan_after_change(Some(at(22, 0)), Some(at(3, 0)), now, Some(day(19))),
            Some(day(20))
        );
        // Turned on for later today: today's scan still runs.
        assert_eq!(
            last_scan_after_change(None, Some(at(22, 0)), now, Some(day(19))),
            Some(day(19))
        );
        // Unchanged or turned off: kept as is, so a missed scan still catches up.
        assert_eq!(
            last_scan_after_change(Some(at(3, 0)), Some(at(3, 0)), now, Some(day(19))),
            Some(day(19))
        );
        assert_eq!(
            last_scan_after_change(Some(at(3, 0)), None, now, Some(day(19))),
            Some(day(19))
        );
    }

    #[test]
    fn daily_action_follows_the_mode() {
        let fired = Some(State::Fired { at: Instant::now() });
        assert_eq!(daily_action(Mode::Off, None), DailyAction::Skip);
        assert_eq!(daily_action(Mode::Auto, None), DailyAction::Clean);
        assert_eq!(daily_action(Mode::Auto, fired), DailyAction::Clean);
        assert_eq!(
            daily_action(Mode::Ask, Some(State::Armed)),
            DailyAction::Offer
        );
        assert_eq!(daily_action(Mode::Ask, None), DailyAction::Offer);
        assert_eq!(daily_action(Mode::Ask, fired), DailyAction::Skip);
    }

    #[test]
    fn early_scan_today_waits_for_tomorrow() {
        // Ran at 01:00 today under an earlier time; the new 03:00 time waits a day.
        let wait = until_daily_scan(at(3, 0), when(20, 2, 0), Some(day(20)));
        assert_eq!(wait, Duration::from_secs(25 * 3600));
    }
}
