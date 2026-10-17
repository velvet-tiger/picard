//! Picard's TOML config: parsing, defaults and validation.
//!
//! The file lives at `~/Library/Application Support/Picard/config.toml`. Parsing
//! is pure (`parse` takes the text and the home folder) so every rule is unit
//! tested without touching disk. Reading, writing and watching the file happen
//! in `lib.rs` and the monitor loop.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::Glob;
use serde::Deserialize;

/// Written on first launch when no config file exists.
pub const DEFAULT_TOML: &str = r#"# Picard config. Picard reloads this file when it changes.

poll_minutes = 5
min_free_gb = 30          # default threshold; or use min_free_pct = 10 instead
recover_margin_gb = 10
cooldown_minutes = 60
mode = "ask"              # auto | ask | off
roots = ["~/working"]
exclude = []

[[task]]
type = "git_worktrees"
stale_days = 14

[[task]]
type = "node_modules"
stale_days = 30

[[task]]
type = "cargo_clean"
mode = "stale"
stale_days = 14
"#;

const BYTES_PER_GB: f64 = 1_000_000_000.0;

/// Where macOS mounts the APFS Data volume that holds home folders.
pub const STARTUP_DATA_MOUNT: &str = "/System/Volumes/Data";

/// Where the config file lives, relative to the user's home folder.
pub fn config_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Picard/config.toml")
}

/// What Picard does when a volume runs low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Run the tasks, then report what was freed.
    Auto,
    /// Notify and wait for the user to choose Clean. The default.
    Ask,
    /// Monitor only.
    Off,
}

/// A low-space threshold, either absolute or relative to the volume size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Threshold {
    Gb(f64),
    Pct(f64),
}

impl Threshold {
    /// Converts the threshold to bytes for a volume of `total_bytes`.
    pub fn bytes(self, total_bytes: u64) -> u64 {
        match self {
            Threshold::Gb(gb) => (gb * BYTES_PER_GB) as u64,
            Threshold::Pct(pct) => (total_bytes as f64 * pct / 100.0) as u64,
        }
    }

    /// Builds a threshold from the two optional TOML keys. Exactly one must be set.
    fn from_keys(gb: Option<f64>, pct: Option<f64>, scope: &str) -> Result<Option<Self>> {
        match (gb, pct) {
            (Some(_), Some(_)) => {
                bail!("{scope}: set either min_free_gb or min_free_pct, not both")
            }
            (Some(gb), None) => {
                if !(gb.is_finite() && gb >= 0.0) {
                    bail!("{scope}: min_free_gb must be zero or more, got {gb}");
                }
                Ok(Some(Threshold::Gb(gb)))
            }
            (None, Some(pct)) => {
                if !(pct.is_finite() && (0.0..=100.0).contains(&pct)) {
                    bail!("{scope}: min_free_pct must be between 0 and 100, got {pct}");
                }
                Ok(Some(Threshold::Pct(pct)))
            }
            (None, None) => Ok(None),
        }
    }
}

/// A per-volume threshold override, matched by mount point.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeOverride {
    pub mount: PathBuf,
    pub threshold: Threshold,
}

/// How `cargo_clean` chooses what to clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CargoMode {
    /// Only projects whose sources have not changed in `stale_days`.
    Stale,
    /// Everything under the roots, through `cargo clean-recursive`.
    All,
}

/// One `[[task]]` entry. Tasks run in the order listed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskConfig {
    GitWorktrees {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_worktree_stale_days")]
        stale_days: u32,
    },
    NodeModules {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_node_modules_stale_days")]
        stale_days: u32,
    },
    CargoClean {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_cargo_mode")]
        mode: CargoMode,
        #[serde(default = "default_worktree_stale_days")]
        stale_days: u32,
        depth: Option<u32>,
    },
    Command {
        #[serde(default = "default_true")]
        enabled: bool,
        name: String,
        run: String,
        #[serde(default = "default_command_timeout_secs")]
        timeout_secs: u64,
    },
}

fn default_true() -> bool {
    true
}
fn default_worktree_stale_days() -> u32 {
    14
}
fn default_node_modules_stale_days() -> u32 {
    30
}
fn default_cargo_mode() -> CargoMode {
    CargoMode::Stale
}
fn default_command_timeout_secs() -> u64 {
    600
}

/// The validated config Picard runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub poll_minutes: u64,
    pub threshold: Threshold,
    pub recover_margin_bytes: u64,
    pub cooldown_minutes: u64,
    pub mode: Mode,
    /// Absolute paths, with `~` expanded. Not canonicalised: a root on an
    /// unmounted drive must still be listed so the menu can show it greyed out.
    pub roots: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub volumes: Vec<VolumeOverride>,
    pub tasks: Vec<TaskConfig>,
}

impl Config {
    /// The threshold for the volume mounted at `mount`, falling back to the default.
    ///
    /// Home folders live on the APFS Data volume, mounted at
    /// `/System/Volumes/Data`, but users think of the startup disk as `/`. Both
    /// share one container and report the same free space, so an override for
    /// `/` also applies to the Data volume.
    pub fn threshold_for(&self, mount: &Path) -> Threshold {
        let is_startup = |p: &Path| p == Path::new("/") || p == Path::new(STARTUP_DATA_MOUNT);
        self.volumes
            .iter()
            .find(|v| v.mount == mount || (is_startup(&v.mount) && is_startup(mount)))
            .map_or(self.threshold, |v| v.threshold)
    }
}

// Mirrors the TOML layout. Every field is optional here so that `parse` can give
// one clear message per problem instead of serde's generic "missing field".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    poll_minutes: Option<u64>,
    min_free_gb: Option<f64>,
    min_free_pct: Option<f64>,
    recover_margin_gb: Option<f64>,
    cooldown_minutes: Option<u64>,
    mode: Option<Mode>,
    roots: Option<Vec<String>>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    volume: Vec<RawVolume>,
    #[serde(default)]
    task: Vec<TaskConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVolume {
    mount: String,
    min_free_gb: Option<f64>,
    min_free_pct: Option<f64>,
}

/// Parses and validates config text. `home` is used to expand `~`.
///
/// Returns an error naming the offending key on the first problem found.
pub fn parse(text: &str, home: &Path) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text).context("config.toml is not valid")?;

    let poll_minutes = raw.poll_minutes.unwrap_or(5);
    if poll_minutes == 0 {
        bail!("poll_minutes must be 1 or more");
    }

    let threshold = Threshold::from_keys(raw.min_free_gb, raw.min_free_pct, "top level")?
        .context("set a default threshold with min_free_gb or min_free_pct")?;

    let margin_gb = raw.recover_margin_gb.unwrap_or(10.0);
    if !(margin_gb.is_finite() && margin_gb >= 0.0) {
        bail!("recover_margin_gb must be zero or more, got {margin_gb}");
    }

    let roots = raw
        .roots
        .context("roots is missing; list at least one folder to clean")?;
    if roots.is_empty() {
        bail!("roots is empty; list at least one folder to clean");
    }
    let roots = roots
        .iter()
        .map(|r| expand_path(r, home).with_context(|| format!("roots entry {r:?}")))
        .collect::<Result<Vec<_>>>()?;

    for pattern in &raw.exclude {
        Glob::new(pattern)
            .with_context(|| format!("exclude pattern {pattern:?} is not a valid glob"))?;
    }

    let volumes = raw
        .volume
        .iter()
        .map(|v| parse_volume(v, home))
        .collect::<Result<Vec<_>>>()?;

    Ok(Config {
        poll_minutes,
        threshold,
        recover_margin_bytes: (margin_gb * BYTES_PER_GB) as u64,
        cooldown_minutes: raw.cooldown_minutes.unwrap_or(60),
        mode: raw.mode.unwrap_or(Mode::Ask),
        roots,
        exclude: raw.exclude,
        volumes,
        tasks: raw.task,
    })
}

fn parse_volume(v: &RawVolume, home: &Path) -> Result<VolumeOverride> {
    let scope = format!("[[volume]] {}", v.mount);
    let threshold = Threshold::from_keys(v.min_free_gb, v.min_free_pct, &scope)?
        .with_context(|| format!("{scope}: set min_free_gb or min_free_pct"))?;
    let mount = expand_path(&v.mount, home).with_context(|| format!("{scope}: mount"))?;
    Ok(VolumeOverride { mount, threshold })
}

/// Expands a leading `~` and requires the result to be absolute.
fn expand_path(raw: &str, home: &Path) -> Result<PathBuf> {
    let path = if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        bail!("path must be absolute or start with ~/");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/test")
    }

    fn parse_ok(text: &str) -> Config {
        parse(text, &home()).unwrap_or_else(|e| panic!("expected valid config: {e:#}"))
    }

    fn parse_err(text: &str) -> String {
        format!("{:#}", parse(text, &home()).expect_err("expected an error"))
    }

    #[test]
    fn default_file_parses_with_plan_defaults() {
        let c = parse_ok(DEFAULT_TOML);
        assert_eq!(c.poll_minutes, 5);
        assert_eq!(c.threshold, Threshold::Gb(30.0));
        assert_eq!(c.recover_margin_bytes, 10_000_000_000);
        assert_eq!(c.cooldown_minutes, 60);
        assert_eq!(c.mode, Mode::Ask);
        assert_eq!(c.roots, vec![PathBuf::from("/Users/test/working")]);
        assert_eq!(c.tasks.len(), 3);
        assert_eq!(
            c.tasks[2],
            TaskConfig::CargoClean {
                enabled: true,
                mode: CargoMode::Stale,
                stale_days: 14,
                depth: None
            }
        );
    }

    #[test]
    fn minimal_file_fills_defaults_and_mode_is_ask() {
        let c = parse_ok("min_free_pct = 10\nroots = [\"/code\"]\n");
        assert_eq!(c.threshold, Threshold::Pct(10.0));
        assert_eq!(c.mode, Mode::Ask);
        assert_eq!(c.poll_minutes, 5);
        assert!(c.tasks.is_empty());
    }

    #[test]
    fn rejects_both_thresholds() {
        let e = parse_err("min_free_gb = 1\nmin_free_pct = 1\nroots = [\"/a\"]");
        assert!(e.contains("not both"), "{e}");
    }

    #[test]
    fn rejects_missing_threshold() {
        let e = parse_err("roots = [\"/a\"]");
        assert!(e.contains("min_free_gb or min_free_pct"), "{e}");
    }

    #[test]
    fn rejects_out_of_range_thresholds() {
        assert!(parse_err("min_free_pct = 101\nroots = [\"/a\"]").contains("between 0 and 100"));
        assert!(parse_err("min_free_gb = -1\nroots = [\"/a\"]").contains("zero or more"));
    }

    #[test]
    fn rejects_zero_poll_minutes() {
        let e = parse_err("poll_minutes = 0\nmin_free_gb = 1\nroots = [\"/a\"]");
        assert!(e.contains("poll_minutes"), "{e}");
    }

    #[test]
    fn rejects_missing_or_empty_roots() {
        assert!(parse_err("min_free_gb = 1").contains("roots is missing"));
        assert!(parse_err("min_free_gb = 1\nroots = []").contains("roots is empty"));
    }

    #[test]
    fn rejects_relative_root() {
        let e = parse_err("min_free_gb = 1\nroots = [\"working\"]");
        assert!(e.contains("\"working\"") && e.contains("absolute"), "{e}");
    }

    #[test]
    fn rejects_unknown_mode() {
        let e = parse_err("min_free_gb = 1\nroots = [\"/a\"]\nmode = \"sometimes\"");
        assert!(e.contains("sometimes"), "{e}");
    }

    #[test]
    fn rejects_unknown_key() {
        let e = parse_err("min_free_gb = 1\nroots = [\"/a\"]\npoll_minuets = 3");
        assert!(e.contains("poll_minuets"), "{e}");
    }

    #[test]
    fn rejects_unknown_task_type_and_task_key() {
        let base = "min_free_gb = 1\nroots = [\"/a\"]\n";
        let e = parse_err(&format!("{base}[[task]]\ntype = \"defrag\""));
        assert!(e.contains("defrag"), "{e}");
        let e = parse_err(&format!(
            "{base}[[task]]\ntype = \"node_modules\"\nstale_dayz = 3"
        ));
        assert!(e.contains("stale_dayz"), "{e}");
    }

    #[test]
    fn rejects_invalid_exclude_glob() {
        let e = parse_err("min_free_gb = 1\nroots = [\"/a\"]\nexclude = [\"**/[oops\"]");
        assert!(e.contains("[oops"), "{e}");
    }

    #[test]
    fn expands_tilde() {
        let c = parse_ok("min_free_gb = 1\nroots = [\"~\", \"~/code\", \"/Volumes/X\"]");
        assert_eq!(
            c.roots,
            vec![
                PathBuf::from("/Users/test"),
                PathBuf::from("/Users/test/code"),
                PathBuf::from("/Volumes/X")
            ]
        );
    }

    #[test]
    fn volume_override_applies_only_to_its_mount() {
        let c = parse_ok(
            "min_free_gb = 30\nroots = [\"/a\"]\n[[volume]]\nmount = \"/Volumes/Projects\"\nmin_free_pct = 5\n",
        );
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/Projects")),
            Threshold::Pct(5.0)
        );
        assert_eq!(c.threshold_for(Path::new("/")), Threshold::Gb(30.0));
    }

    #[test]
    fn root_override_applies_to_the_data_volume() {
        let c = parse_ok(
            "min_free_gb = 30\nroots = [\"/a\"]\n[[volume]]\nmount = \"/\"\nmin_free_gb = 5\n",
        );
        assert_eq!(
            c.threshold_for(Path::new("/System/Volumes/Data")),
            Threshold::Gb(5.0)
        );
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/X")),
            Threshold::Gb(30.0)
        );
    }

    #[test]
    fn volume_override_needs_a_threshold() {
        let e =
            parse_err("min_free_gb = 1\nroots = [\"/a\"]\n[[volume]]\nmount = \"/Volumes/P\"\n");
        assert!(e.contains("/Volumes/P"), "{e}");
    }

    #[test]
    fn command_task_requires_name_and_run() {
        let e = parse_err(
            "min_free_gb = 1\nroots = [\"/a\"]\n[[task]]\ntype = \"command\"\nname = \"x\"",
        );
        assert!(e.contains("run"), "{e}");
    }

    #[test]
    fn threshold_bytes() {
        assert_eq!(Threshold::Gb(30.0).bytes(0), 30_000_000_000);
        assert_eq!(Threshold::Pct(10.0).bytes(500_000_000_000), 50_000_000_000);
    }

    #[test]
    fn config_path_is_under_application_support() {
        assert_eq!(
            config_path(Path::new("/Users/test")),
            PathBuf::from("/Users/test/Library/Application Support/Picard/config.toml")
        );
    }
}
