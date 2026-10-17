//! The Settings window's view of the config, and writing it back.
//!
//! Edits go through `toml_edit`, so comments, key order and the parts the
//! window does not show (`[[volume]]`, `[tools]`) survive a save. Every result
//! is checked with `config::parse` before it is written, and it is written
//! atomically with mode 600 so the privacy check in `config::load` still passes.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::config::{self, Mode, TaskConfig, Threshold};

const BYTES_PER_GB: f64 = 1_000_000_000.0;

/// What the Settings form edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub poll_minutes: u64,
    pub threshold: ThresholdSetting,
    pub recover_margin_gb: f64,
    pub cooldown_minutes: u64,
    pub mode: Mode,
    /// As written in the file, so `~/working` stays `~/working`.
    pub roots: Vec<String>,
    pub exclude: Vec<String>,
    pub tasks: Vec<TaskConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "lowercase")]
pub enum ThresholdSetting {
    Gb(f64),
    Pct(f64),
}

/// Builds the form from config text. Fails if the text is not a valid config.
pub fn read(text: &str, home: &Path) -> Result<Settings> {
    let cfg = config::parse(text, home)?;
    let doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    let roots = string_array(&doc, "roots");
    Ok(Settings {
        poll_minutes: cfg.poll_minutes,
        threshold: match cfg.threshold {
            Threshold::Gb(v) => ThresholdSetting::Gb(v),
            Threshold::Pct(v) => ThresholdSetting::Pct(v),
        },
        recover_margin_gb: cfg.recover_margin_bytes as f64 / BYTES_PER_GB,
        cooldown_minutes: cfg.cooldown_minutes,
        mode: cfg.mode,
        roots,
        exclude: cfg.exclude,
        tasks: cfg.tasks,
    })
}

/// Applies the form to config text and returns the new text.
///
/// Fails, changing nothing, if the result would not be a valid config.
pub fn apply(text: &str, settings: &Settings, home: &Path) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    set(
        &mut doc,
        "poll_minutes",
        Value::from(to_i64(settings.poll_minutes)?),
    );
    // Only one threshold key may exist. Removing just the other one keeps the
    // current key, and its comment, in place when the unit does not change.
    match settings.threshold {
        ThresholdSetting::Gb(v) => {
            doc.remove("min_free_pct");
            set(&mut doc, "min_free_gb", Value::from(v));
        }
        ThresholdSetting::Pct(v) => {
            doc.remove("min_free_gb");
            set(&mut doc, "min_free_pct", Value::from(v));
        }
    }
    set(
        &mut doc,
        "recover_margin_gb",
        Value::from(settings.recover_margin_gb),
    );
    set(
        &mut doc,
        "cooldown_minutes",
        Value::from(to_i64(settings.cooldown_minutes)?),
    );
    set(&mut doc, "mode", Value::from(mode_name(settings.mode)));
    set(&mut doc, "roots", Value::Array(strings(&settings.roots)));
    set(
        &mut doc,
        "exclude",
        Value::Array(strings(&settings.exclude)),
    );
    doc.insert("task", tasks_item(&settings.tasks)?);

    let out = doc.to_string();
    config::parse(&out, home).context("these settings are not valid")?;
    Ok(out)
}

/// Replaces `path` with `text` atomically: write a private temporary file next
/// to it, flush it to disk, then rename it over the original.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent folder", path.display()))?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        // Best effort: the temporary file is useless now, and its failure to
        // delete is less important than the write error being reported.
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

/// Writes a folder the way roots are usually written: `~/…` when it is inside
/// the home folder, otherwise the absolute path.
pub fn display_root(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Sets a top-level key. An existing value is replaced in place, so the key
/// keeps its position and its comments (including any comment block above it).
fn set(doc: &mut DocumentMut, key: &str, mut new: Value) {
    if let Some(old) = doc.get_mut(key).and_then(Item::as_value_mut) {
        *new.decor_mut() = old.decor().clone();
        *old = new;
        return;
    }
    doc.insert(key, Item::Value(new));
}

fn string_array(doc: &DocumentMut, key: &str) -> Vec<String> {
    doc.get(key)
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn strings(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Auto => "auto",
        Mode::Ask => "ask",
        Mode::Off => "off",
    }
}

fn to_i64(n: u64) -> Result<i64> {
    i64::try_from(n).with_context(|| format!("{n} is too large"))
}

/// Serialises the task list as a `[[task]]` array of tables.
fn tasks_item(tasks: &[TaskConfig]) -> Result<Item> {
    #[derive(Serialize)]
    struct Wrapper<'a> {
        task: &'a [TaskConfig],
    }
    if tasks.is_empty() {
        return Ok(Item::None);
    }
    let text = toml::to_string(&Wrapper { task: tasks }).context("serialising tasks")?;
    let doc: DocumentMut = text.parse().context("re-reading serialised tasks")?;
    match doc.get("task") {
        Some(item @ Item::ArrayOfTables(_)) => Ok(item.clone()),
        other => bail!("serialised tasks had an unexpected shape: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CargoMode, DEFAULT_TOML};
    use std::os::unix::fs::PermissionsExt;

    fn home() -> &'static Path {
        Path::new("/Users/test")
    }

    #[test]
    fn reads_the_default_config() {
        let s = read(DEFAULT_TOML, home()).unwrap();
        assert_eq!(s.threshold, ThresholdSetting::Gb(30.0));
        assert_eq!(s.roots, vec!["~/working"]);
        assert_eq!(s.mode, Mode::Ask);
        assert_eq!(s.tasks.len(), 4);
    }

    #[test]
    fn unchanged_settings_round_trip_to_an_equivalent_config() {
        let s = read(DEFAULT_TOML, home()).unwrap();
        let out = apply(DEFAULT_TOML, &s, home()).unwrap();
        assert_eq!(read(&out, home()).unwrap(), s);
    }

    #[test]
    fn edits_keep_comments_and_untouched_sections() {
        let text = format!(
            "{DEFAULT_TOML}\n# my drive\n[[volume]]\nmount = \"/Volumes/P\"\nmin_free_pct = 5\n\n[tools]\ngit = \"/opt/homebrew/bin/git\"\n"
        );
        let mut s = read(&text, home()).unwrap();
        s.mode = Mode::Auto;
        s.threshold = ThresholdSetting::Pct(10.0);
        s.roots.push("/Volumes/P/code".into());
        let out = apply(&text, &s, home()).unwrap();

        assert!(out.contains("# Picard config."), "top comment lost:\n{out}");
        assert!(
            out.contains("mode = \"auto\"              # auto | ask | off"),
            "inline comment lost:\n{out}"
        );
        assert!(out.contains("# my drive"), "{out}");
        assert!(
            out.contains("[tools]") && out.contains("/opt/homebrew/bin/git"),
            "{out}"
        );
        assert!(
            !out.contains("min_free_gb = 30"),
            "old threshold left behind:\n{out}"
        );
        let back = read(&out, home()).unwrap();
        assert_eq!(back.threshold, ThresholdSetting::Pct(10.0));
        assert_eq!(back.roots, vec!["~/working", "/Volumes/P/code"]);
    }

    #[test]
    fn task_edits_are_written() {
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        s.tasks.reverse();
        s.tasks[0] = TaskConfig::CargoClean {
            enabled: false,
            mode: CargoMode::All,
            stale_days: 7,
            depth: Some(4),
        };
        let out = apply(DEFAULT_TOML, &s, home()).unwrap();
        let back = read(&out, home()).unwrap();
        assert_eq!(back.tasks, s.tasks);
    }

    #[test]
    fn invalid_settings_are_refused() {
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        s.roots.clear();
        let err = format!("{:#}", apply(DEFAULT_TOML, &s, home()).unwrap_err());
        assert!(err.contains("roots"), "{err}");
        s.roots = vec!["relative/path".into()];
        assert!(apply(DEFAULT_TOML, &s, home()).is_err());
        s.roots = vec!["~/working".into()];
        s.threshold = ThresholdSetting::Pct(150.0);
        assert!(apply(DEFAULT_TOML, &s, home()).is_err());
    }

    #[test]
    fn display_root_uses_tilde_inside_home_only() {
        let home = Path::new("/Users/test");
        assert_eq!(
            display_root(Path::new("/Users/test/working"), home),
            "~/working"
        );
        assert_eq!(display_root(Path::new("/Users/test"), home), "~");
        assert_eq!(
            display_root(Path::new("/Users/tester/x"), home),
            "/Users/tester/x",
            "a sibling whose name starts the same is not inside home"
        );
        assert_eq!(
            display_root(Path::new("/Volumes/P/code"), home),
            "/Volumes/P/code"
        );
    }

    #[test]
    fn write_private_replaces_atomically_with_mode_600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        assert!(config::open_private(&path).is_ok());
    }
}
