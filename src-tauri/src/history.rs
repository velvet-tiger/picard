//! Append-only log of every removal, one JSON object per line.
//!
//! Lives next to the config as `history.jsonl`. Lines are written as soon as a
//! task finishes, so a crash part-way through a run still leaves a record of
//! what was already removed.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub time: DateTime<Utc>,
    pub task: String,
    pub volume: String,
    pub path: String,
    pub bytes: u64,
}

/// Appends entries to the log, creating the file and its folder if needed.
pub fn append(log_path: &Path, entries: &[Entry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut text = String::new();
    for e in entries {
        text.push_str(&serde_json::to_string(e).context("serialising history entry")?);
        text.push('\n');
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", log_path.display()))
}

/// Everything Picard has removed, added up across every volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub bytes: u64,
    pub items: u64,
    /// The oldest entry's time; `None` when nothing has been removed.
    pub since: Option<DateTime<Utc>>,
}

/// The newest `limit` entries, newest first. A missing log means nothing has
/// been removed yet, so it reads as empty. Lines that do not parse are skipped
/// and logged rather than hiding the rest of the history.
pub fn read_recent(log_path: &Path, limit: usize) -> Result<Vec<Entry>> {
    let text = read_log(log_path)?;
    Ok(parse_lines(text.lines().rev()).take(limit).collect())
}

/// Adds up the whole log. It is the only record of what was removed, so the
/// total always agrees with it, and starts again from zero if it is deleted.
/// Missing and unreadable lines are treated as in `read_recent`. The log is
/// never rotated, so it is read a line at a time to keep memory flat.
pub fn read_totals(log_path: &Path) -> Result<Totals> {
    let file = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Totals::default()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", log_path.display())),
    };
    let mut totals = Totals::default();
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("reading {}", log_path.display()))?;
        for e in parse_lines(std::iter::once(line.as_str())) {
            totals = Totals {
                bytes: totals.bytes.saturating_add(e.bytes),
                items: totals.items.saturating_add(1),
                since: Some(totals.since.map_or(e.time, |s| s.min(e.time))),
            };
        }
    }
    Ok(totals)
}

/// The log's text, or an empty string if nothing has been removed yet.
fn read_log(log_path: &Path) -> Result<String> {
    match std::fs::read_to_string(log_path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", log_path.display())),
    }
}

fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> impl Iterator<Item = Entry> {
    lines
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str(l) {
            Ok(e) => Some(e),
            Err(err) => {
                log::warn!("skipping unreadable history line: {err}");
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        Entry {
            time: DateTime::parse_from_rfc3339("2026-10-17T01:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            task: "node_modules".into(),
            volume: "Startup disk".into(),
            path: path.into(),
            bytes: 42,
        }
    }

    #[test]
    fn reads_recent_entries_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("history.jsonl");
        assert!(read_recent(&log, 5).unwrap().is_empty());
        append(&log, &[entry("/a"), entry("/b"), entry("/c")]).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        let recent = read_recent(&log, 2).unwrap();
        let paths: Vec<_> = recent.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/c", "/b"]);
    }

    fn entry_at(path: &str, time: &str, bytes: u64) -> Entry {
        Entry {
            time: DateTime::parse_from_rfc3339(time)
                .unwrap()
                .with_timezone(&Utc),
            bytes,
            ..entry(path)
        }
    }

    #[test]
    fn totals_are_zero_without_a_log() {
        let dir = tempfile::tempdir().unwrap();
        let totals = read_totals(&dir.path().join("history.jsonl")).unwrap();
        assert_eq!(totals, Totals::default());
    }

    #[test]
    fn totals_add_up_every_entry_and_skip_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("history.jsonl");
        // Written out of time order, so `since` must be the oldest, not the first line.
        append(&log, &[entry_at("/a", "2026-10-17T01:00:00Z", 100)]).unwrap();
        append(&log, &[entry_at("/b", "2026-09-03T08:30:00Z", 250)]).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"not json\n\n")
            .unwrap();
        append(&log, &[entry_at("/c", "2026-10-20T12:00:00Z", 7)]).unwrap();

        let totals = read_totals(&log).unwrap();
        assert_eq!(totals.bytes, 357);
        assert_eq!(totals.items, 3);
        assert_eq!(
            totals.since,
            Some(
                DateTime::parse_from_rfc3339("2026-09-03T08:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn totals_saturate_instead_of_overflowing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("history.jsonl");
        append(
            &log,
            &[
                entry_at("/a", "2026-10-17T01:00:00Z", u64::MAX),
                entry_at("/b", "2026-10-17T01:00:00Z", 1),
            ],
        )
        .unwrap();
        assert_eq!(read_totals(&log).unwrap().bytes, u64::MAX);
    }

    #[test]
    fn appends_one_json_line_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sub/history.jsonl");
        append(&log, &[entry("/a")]).unwrap();
        append(&log, &[entry("/b"), entry("/c")]).unwrap();
        append(&log, &[]).unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["path"], "/a");
        assert_eq!(first["bytes"], 42);
        assert_eq!(first["time"], "2026-10-17T01:00:00Z");
    }
}
