//! Decides when a volume is low enough to act on.
//!
//! Each volume is either `Armed` (ready to fire) or `Fired` (already alerted).
//! A fired volume fires again only after it recovers above
//! `threshold + recover_margin`, or after `cooldown` while still low. Without
//! this, a cleanup that cannot free enough space would alert on every poll.
//!
//! Everything here is pure. The poll loop owns the clock and the I/O.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
pub fn step(state: State, free_bytes: u64, policy: &Policy, now: Instant) -> (State, Decision) {
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
        State::Fired { at } if low && now.duration_since(at) >= policy.cooldown => {
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
/// starts armed.
pub fn evaluate(
    states: &States,
    readings: &[(Volume, DiskSpace)],
    config: &Config,
    now: Instant,
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
        let (state, decision) = step(prev, space.free_bytes, &policy, now);
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

    /// Feeds (free GB, minutes since start) readings and returns which ones fired.
    fn run(readings: &[(u64, u64)], mode: Mode) -> Vec<bool> {
        let t0 = Instant::now();
        let p = policy(mode);
        let mut state = State::Armed;
        readings
            .iter()
            .map(|&(free_gb, minute)| {
                let now = t0 + Duration::from_secs(minute * 60);
                let (s, d) = step(state, free_gb * GB, &p, now);
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
    fn off_mode_never_fires_and_stays_armed() {
        let t0 = Instant::now();
        let (s, d) = step(State::Fired { at: t0 }, 0, &policy(Mode::Off), t0);
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
        let (states, alerts) = evaluate(&States::new(), &readings, &config, now);
        let names: Vec<_> = alerts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Startup disk", "P"]);
        assert_eq!(states[Path::new("/Volumes/Big")], State::Armed);

        // Second pass: still low, but already fired, so nothing new.
        let (_, alerts) = evaluate(&states, &readings, &config, now + Duration::from_secs(300));
        assert!(alerts.is_empty());
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
    fn evaluate_drops_volumes_that_disappear() {
        let config = parse("min_free_gb = 30\nroots = [\"/a\"]", Path::new("/Users/t")).unwrap();
        let mut states = States::new();
        states.insert(
            PathBuf::from("/Volumes/Gone"),
            State::Fired { at: Instant::now() },
        );
        let (next, _) = evaluate(&states, &[], &config, Instant::now());
        assert!(next.is_empty());
    }
}
