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
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
