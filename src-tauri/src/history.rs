//! Append-only log of every removal, one JSON object per line.
//!
//! Lives next to the config as `history.jsonl`. Lines are written as soon as a
//! task finishes, so a crash part-way through a run still leaves a record of
//! what was already removed.

use std::fs::OpenOptions;
use std::io::Write;
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

/// The newest `limit` entries, newest first. A missing log means nothing has
/// been removed yet, so it reads as empty. Lines that do not parse are skipped
/// and logged rather than hiding the rest of the history.
pub fn read_recent(log_path: &Path, limit: usize) -> Result<Vec<Entry>> {
    let text = match std::fs::read_to_string(log_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", log_path.display())),
    };
    Ok(text
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str(l) {
            Ok(e) => Some(e),
            Err(err) => {
                log::warn!("skipping unreadable history line: {err}");
                None
            }
        })
        .take(limit)
        .collect())
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
