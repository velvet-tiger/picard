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

use crate::config::{self, Appearance, Mode, TaskConfig, Threshold};

const BYTES_PER_GB: f64 = 1_000_000_000.0;

/// What the Settings form edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub poll_minutes: u64,
    pub threshold: ThresholdSetting,
    pub recover_margin_gb: f64,
    pub cooldown_minutes: u64,
    pub mode: Mode,
    /// Daily scan time as `HH:MM`, or `None` when the daily scan is off.
    pub daily_scan: Option<String>,
    pub appearance: Appearance,
    /// As written in the file, so `~/working` stays `~/working`.
    pub roots: Vec<String>,
    pub exclude: Vec<String>,
    /// Names whose changes do not count as work: the defaults unless the file
    /// sets its own list.
    pub activity_ignore: Vec<String>,
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
        daily_scan: cfg
            .daily_scan
            .map(|t| t.format(config::DAILY_SCAN_FORMAT).to_string()),
        appearance: cfg.appearance,
        roots,
        exclude: cfg.exclude,
        activity_ignore: cfg.activity_ignore,
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
    match &settings.daily_scan {
        Some(at) => set(&mut doc, "daily_scan", Value::from(at.as_str())),
        None => {
            doc.remove("daily_scan");
        }
    }
    // System is the default, so it is written by leaving the key out.
    match appearance_name(settings.appearance) {
        Some(name) => set(&mut doc, "appearance", Value::from(name)),
        None => {
            doc.remove("appearance");
        }
    }
    set(&mut doc, "roots", Value::Array(strings(&settings.roots)));
    set(
        &mut doc,
        "exclude",
        Value::Array(strings(&settings.exclude)),
    );
    // A list holding exactly the defaults, in any order, removes the key, so
    // the file follows any later change to the defaults. That also removes
    // comments above the key.
    if same_names(
        &settings.activity_ignore,
        &config::default_activity_ignore(),
    ) {
        doc.remove("activity_ignore");
    } else {
        set(
            &mut doc,
            "activity_ignore",
            Value::Array(strings(&settings.activity_ignore)),
        );
    }
    doc.insert("task", tasks_item(&settings.tasks)?);

    let out = doc.to_string();
    config::parse(&out, home).context("these settings are not valid")?;
    Ok(out)
}

/// True when both lists hold the same names, ignoring order and repeats.
fn same_names(a: &[String], b: &[String]) -> bool {
    fn set(names: &[String]) -> std::collections::BTreeSet<&str> {
        names.iter().map(String::as_str).collect()
    }
    set(a) == set(b)
}

/// The `ignore` entries as written in the file, so `~/…` stays `~/…`.
///
/// Kept out of `Settings` on purpose: the Clean tab changes this list while
/// the Settings tab may hold an older copy, and saving that copy must not
/// undo those changes. Both tabs edit it one entry at a time instead.
pub fn ignored(text: &str, home: &Path) -> Result<Vec<String>> {
    config::parse(text, home)?;
    let doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    // A hand-edited file may repeat an entry. Listing it once keeps the
    // Settings list from showing, and keying on, the same folder twice.
    let mut seen = std::collections::HashSet::new();
    Ok(string_array(&doc, "ignore")
        .into_iter()
        .filter(|e| seen.insert(e.clone()))
        .collect())
}

/// Adds `folder` to the `ignore` list, or removes every entry naming it, and
/// returns the new text. Running it twice with the same arguments gives the
/// same text as running it once.
///
/// `folder` may be absolute or start with `~/`. An entry names it when both
/// expand to the same path or resolve to the same folder. Fails, changing
/// nothing, if the result would not be a valid config.
pub fn set_ignored(text: &str, folder: &str, ignore: bool, home: &Path) -> Result<String> {
    let target = config::expand_path(folder.trim(), home)
        .with_context(|| format!("cannot ignore {folder:?}"))?;
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    let current = string_array(&doc, "ignore");
    let names_target = |entry: &str| {
        config::expand_path(entry, home).is_ok_and(|path| same_folder(&path, &target))
    };
    let updated: Vec<String> = if ignore {
        if current.iter().any(|e| names_target(e)) {
            return Ok(text.to_string());
        }
        let mut list = current;
        list.push(display_root(&target, home));
        list
    } else {
        let kept: Vec<String> = current
            .iter()
            .filter(|e| !names_target(e))
            .cloned()
            .collect();
        if kept.len() == current.len() {
            return Ok(text.to_string());
        }
        kept
    };
    set(&mut doc, "ignore", Value::Array(strings(&updated)));
    let out = doc.to_string();
    config::parse(&out, home).context("the ignore list is not valid")?;
    Ok(out)
}

/// Whether two absolute paths name the same folder, directly or once
/// symlinks are resolved. A folder that no longer exists matches only by path.
fn same_folder(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
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

/// The value written for `appearance`, or `None` for the default.
fn appearance_name(appearance: Appearance) -> Option<&'static str> {
    match appearance {
        Appearance::System => None,
        Appearance::Light => Some("light"),
        Appearance::Dark => Some("dark"),
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
    use std::path::PathBuf;

    fn home() -> &'static Path {
        Path::new("/Users/test")
    }

    #[test]
    fn reads_the_default_config() {
        let s = read(DEFAULT_TOML, home()).unwrap();
        assert_eq!(s.threshold, ThresholdSetting::Gb(30.0));
        assert_eq!(s.roots, vec!["~/working"]);
        assert_eq!(s.mode, Mode::Ask);
        assert_eq!(s.tasks.len(), 5);
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
    fn daily_scan_is_written_and_removed() {
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        assert_eq!(s.daily_scan, None);
        s.daily_scan = Some("03:15".into());
        let on = apply(DEFAULT_TOML, &s, home()).unwrap();
        assert_eq!(
            read(&on, home()).unwrap().daily_scan.as_deref(),
            Some("03:15")
        );
        s.daily_scan = None;
        let off = apply(&on, &s, home()).unwrap();
        assert!(!off.contains("daily_scan = \"03:15\""), "{off}");
        assert_eq!(read(&off, home()).unwrap().daily_scan, None);
        s.daily_scan = Some("".into());
        let err = format!("{:#}", apply(DEFAULT_TOML, &s, home()).unwrap_err());
        assert!(err.contains("daily_scan"), "{err}");
    }

    #[test]
    fn appearance_is_written_and_removed_for_system() {
        let key = |text: &str| text.lines().any(|l| l.starts_with("appearance"));
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        assert_eq!(s.appearance, Appearance::System);
        assert!(!key(&apply(DEFAULT_TOML, &s, home()).unwrap()));

        s.appearance = Appearance::Dark;
        let dark = apply(DEFAULT_TOML, &s, home()).unwrap();
        assert!(dark.contains("\nappearance = \"dark\""), "{dark}");
        assert_eq!(read(&dark, home()).unwrap().appearance, Appearance::Dark);

        s.appearance = Appearance::Light;
        let light = apply(&dark, &s, home()).unwrap();
        assert_eq!(read(&light, home()).unwrap().appearance, Appearance::Light);

        s.appearance = Appearance::System;
        let system = apply(&light, &s, home()).unwrap();
        assert!(!key(&system), "{system}");
        assert_eq!(
            read(&system, home()).unwrap().appearance,
            Appearance::System
        );
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
    fn set_ignored_adds_and_removes_one_entry_keeping_the_rest() {
        let added =
            set_ignored(DEFAULT_TOML, "/Users/test/w/a/node_modules", true, home()).unwrap();
        assert_eq!(ignored(&added, home()).unwrap(), vec!["~/w/a/node_modules"]);
        assert!(added.contains("# Picard config."), "{added}");
        assert_eq!(
            config::parse(&added, home()).unwrap().ignore,
            vec![PathBuf::from("/Users/test/w/a/node_modules")]
        );
        // A top-level key added to a file with [[task]] tables must stay top level.
        assert_eq!(config::parse(&added, home()).unwrap().tasks.len(), 5);

        let two = set_ignored(&added, "/Volumes/P/x/target", true, home()).unwrap();
        assert_eq!(
            ignored(&two, home()).unwrap(),
            vec!["~/w/a/node_modules", "/Volumes/P/x/target"]
        );
        // The `~/` form names the same folder as the absolute one.
        let one = set_ignored(&two, "~/w/a/node_modules", false, home()).unwrap();
        assert_eq!(ignored(&one, home()).unwrap(), vec!["/Volumes/P/x/target"]);
    }

    #[test]
    fn set_ignored_is_idempotent() {
        let once = set_ignored(DEFAULT_TOML, "~/w/a", true, home()).unwrap();
        assert_eq!(
            set_ignored(&once, "/Users/test/w/a", true, home()).unwrap(),
            once
        );
        assert_eq!(
            set_ignored(DEFAULT_TOML, "~/w/a", false, home()).unwrap(),
            DEFAULT_TOML,
            "removing an entry that is not there changes nothing"
        );
    }

    #[test]
    fn set_ignored_matches_entries_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("nm")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let via_link = link.join("nm").display().to_string();
        let via_real = real.join("nm").display().to_string();
        let text = set_ignored(DEFAULT_TOML, &via_link, true, home()).unwrap();
        assert_eq!(set_ignored(&text, &via_real, true, home()).unwrap(), text);
        let removed = set_ignored(&text, &via_real, false, home()).unwrap();
        assert!(ignored(&removed, home()).unwrap().is_empty());
    }

    #[test]
    fn ignored_lists_a_repeated_entry_once_and_remove_clears_every_copy() {
        let text = DEFAULT_TOML.replacen(
            "exclude = []\n",
            "exclude = []\nignore = [\"~/a\", \"~/b\", \"~/a\"]\n",
            1,
        );
        assert_eq!(ignored(&text, home()).unwrap(), vec!["~/a", "~/b"]);
        let out = set_ignored(&text, "~/a", false, home()).unwrap();
        assert_eq!(ignored(&out, home()).unwrap(), vec!["~/b"]);
    }

    #[test]
    fn set_ignored_refuses_relative_paths() {
        let err = format!(
            "{:#}",
            set_ignored(DEFAULT_TOML, "w/a", true, home()).unwrap_err()
        );
        assert!(err.contains("w/a"), "{err}");
        assert!(set_ignored(DEFAULT_TOML, "", true, home()).is_err());
    }

    #[test]
    fn saving_settings_leaves_the_ignore_list_alone() {
        let text = set_ignored(DEFAULT_TOML, "~/w/a", true, home()).unwrap();
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        s.mode = Mode::Auto;
        let out = apply(&text, &s, home()).unwrap();
        assert_eq!(ignored(&out, home()).unwrap(), vec!["~/w/a"]);
    }

    #[test]
    fn activity_ignore_shows_the_defaults_and_writes_only_a_changed_list() {
        let defaults = crate::config::default_activity_ignore();
        let key = |text: &str| text.lines().any(|l| l.starts_with("activity_ignore"));

        // No key: the form shows the defaults, and saving it adds no key.
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        assert_eq!(s.activity_ignore, defaults);
        let out = apply(DEFAULT_TOML, &s, home()).unwrap();
        assert!(!key(&out), "{out}");

        // Dropping a default writes the whole list, without it.
        s.activity_ignore.retain(|n| n != "CLAUDE.md");
        s.activity_ignore.push(".vscode".to_string());
        let out = apply(&out, &s, home()).unwrap();
        let parsed = crate::config::parse(&out, home()).unwrap();
        assert_eq!(parsed.activity_ignore, s.activity_ignore);
        assert!(!parsed.activity_ignore.contains(&"CLAUDE.md".to_string()));

        // An empty list is kept as `[]`: every change counts.
        s.activity_ignore.clear();
        let out = apply(&out, &s, home()).unwrap();
        assert!(out.contains("\nactivity_ignore = []"), "{out}");
        assert!(
            crate::config::parse(&out, home())
                .unwrap()
                .activity_ignore
                .is_empty()
        );

        // Back to the defaults, even reordered and with a repeat, removes the key.
        s.activity_ignore = defaults.iter().rev().cloned().collect();
        s.activity_ignore.push(defaults[0].clone());
        let out = apply(&out, &s, home()).unwrap();
        assert!(!key(&out), "{out}");
        assert_eq!(read(&out, home()).unwrap().activity_ignore, defaults);
    }

    #[test]
    fn saving_an_invalid_activity_ignore_name_changes_nothing() {
        let mut s = read(DEFAULT_TOML, home()).unwrap();
        s.activity_ignore = vec!["src/lib".to_string()];
        let e = format!("{:#}", apply(DEFAULT_TOML, &s, home()).unwrap_err());
        assert!(e.contains("activity_ignore"), "{e}");
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
