//! Picard's TOML config: parsing, defaults and validation.
//!
//! The file lives at `~/Library/Application Support/Picard/config.toml`. Parsing
//! is pure (`parse` takes the text and the home folder) so every rule is unit
//! tested without touching disk. Reading, writing and watching the file happen
//! in `lib.rs` and the monitor loop.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::NaiveTime;
use globset::Glob;
use serde::{Deserialize, Serialize};

/// Written on first launch when no config file exists.
pub const DEFAULT_TOML: &str = r#"# Picard config. Picard reloads this file when it changes.

poll_minutes = 5
min_free_gb = 30          # default threshold; or use min_free_pct = 10 instead
recover_margin_gb = 10
cooldown_minutes = 60
mode = "ask"              # auto | ask | off
# daily_scan = "03:00"    # also scan once a day at this local time (24-hour)
roots = ["~/working"]
exclude = []

[[task]]
type = "git_worktrees"
stale_days = 14
min_idle_minutes = 30

[[task]]
type = "node_modules"
stale_days = 30

[[task]]
type = "cargo_clean"
mode = "stale"
stale_days = 14

[[task]]
type = "python_venv"
stale_days = 30

[[task]]
type = "build_output"     # only folders git ignores and tracks nothing in
stale_days = 30
dirs = ["dist", "build", ".next", "vendor"]
"#;

const BYTES_PER_GB: f64 = 1_000_000_000.0;

/// Where macOS mounts the APFS Data volume that holds home folders.
pub const STARTUP_DATA_MOUNT: &str = "/System/Volumes/Data";

/// Reads and parses the config, refusing it unless only this user can change it.
///
/// The config decides what Picard deletes (`roots`), which programs it runs
/// (`[tools]`, `command` tasks) and whether it acts unattended (`mode`). Anyone
/// who could edit it could make Picard delete or run whatever they like, so a
/// config that others can change is not used at all.
pub fn load(path: &Path, home: &Path) -> Result<Config> {
    let mut file = open_private(path).map_err(|why| {
        anyhow::anyhow!(
            "not using {}: {why}. Only you should be able to change it and the folders above it (for example: chmod go-w <path>)",
            path.display()
        )
    })?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    parse(&text, home).with_context(|| format!("in {}", path.display()))
}

/// Opens the config for reading only if just this user can change it.
///
/// Every folder above the file, as written and after resolving symlinks, must
/// pass `dir_problem` (OpenSSH's rule). The resolved file is then opened once
/// with `O_NOFOLLOW`, and its owner and mode are checked on that open handle
/// with `fstat`. The caller reads from the same handle, so the file that was
/// checked is the file that is read, even if something changes in between.
pub fn open_private(path: &Path) -> Result<File, String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
    for file in [path, resolved.as_path()] {
        for dir in file
            .ancestors()
            .skip(1)
            .filter(|d| !d.as_os_str().is_empty())
        {
            let meta = std::fs::metadata(dir)
                .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            if let Some(why) = dir_problem(meta.uid(), meta.mode(), me) {
                return Err(format!("{} {why}", dir.display()));
            }
        }
    }
    let file =
        open_nofollow(&resolved).map_err(|e| format!("cannot open {}: {e}", resolved.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot read {}: {e}", resolved.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", resolved.display()));
    }
    if let Some(why) = file_problem(meta.uid(), meta.mode(), me) {
        return Err(format!("{} {why}", resolved.display()));
    }
    Ok(file)
}

/// Opens read-only, failing (ELOOP) if the last path component is a symlink.
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

const GROUP_OR_OTHER_WRITE: u32 = 0o022;
const STICKY: u32 = 0o1000;
const ROOT: u32 = 0;

/// The config file itself must be the user's and writable only by them.
fn file_problem(owner: u32, mode: u32, me: u32) -> Option<&'static str> {
    if owner != me {
        Some("is owned by another user")
    } else if mode & GROUP_OR_OTHER_WRITE != 0 {
        Some("is writable by group or others")
    } else {
        None
    }
}

/// A folder above the config may belong to the user or root. It must not let
/// others replace entries, unless it is sticky (like /tmp), where only an
/// entry's owner can rename or delete it.
fn dir_problem(owner: u32, mode: u32, me: u32) -> Option<&'static str> {
    if owner != me && owner != ROOT {
        Some("is owned by another user")
    } else if mode & GROUP_OR_OTHER_WRITE != 0 && mode & STICKY == 0 {
        Some("is writable by group or others")
    } else {
        None
    }
}

/// Where the config file lives, relative to the user's home folder.
pub fn config_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Picard/config.toml")
}

/// What Picard does when a volume runs low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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

/// Absolute paths for tools, overriding the PATH search. Empty by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOverrides {
    pub git: Option<PathBuf>,
    pub npm: Option<PathBuf>,
    pub pnpm: Option<PathBuf>,
    pub uv: Option<PathBuf>,
    pub brew: Option<PathBuf>,
    pub bun: Option<PathBuf>,
    pub xcrun: Option<PathBuf>,
}

impl ToolOverrides {
    pub fn get(&self, tool: &str) -> Option<&Path> {
        match tool {
            "git" => self.git.as_deref(),
            "npm" => self.npm.as_deref(),
            "pnpm" => self.pnpm.as_deref(),
            "uv" => self.uv.as_deref(),
            "brew" => self.brew.as_deref(),
            "bun" => self.bun.as_deref(),
            "xcrun" => self.xcrun.as_deref(),
            _ => None,
        }
    }
}

/// A per-volume threshold override, matched by mount point.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeOverride {
    pub mount: Option<PathBuf>,
    /// Upper-case volume UUID, as `diskutil info` prints it.
    pub uuid: Option<String>,
    pub threshold: Threshold,
}

impl VolumeOverride {
    /// Whether this override applies to the volume at `mount` with `uuid`.
    ///
    /// When both sides know a UUID, it decides: a different drive mounted at
    /// the same path does not match, and a renamed drive still does. Otherwise
    /// the mount point decides, treating `/` and the APFS Data volume as one.
    fn matches(&self, mount: &Path, uuid: Option<&str>) -> bool {
        if let (Some(mine), Some(theirs)) = (&self.uuid, uuid) {
            return mine.eq_ignore_ascii_case(theirs);
        }
        let is_startup = |p: &Path| p == Path::new("/") || p == Path::new(STARTUP_DATA_MOUNT);
        self.mount
            .as_deref()
            .is_some_and(|m| m == mount || (is_startup(m) && is_startup(mount)))
    }
}

/// How `cargo_clean` chooses what to clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CargoMode {
    /// Only projects whose sources have not changed in `stale_days`.
    Stale,
    /// Everything under the roots, through `cargo clean-recursive`.
    All,
}

/// One `[[task]]` entry. Tasks run in the order listed.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskConfig {
    GitWorktrees {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_worktree_stale_days")]
        stale_days: u32,
        /// How long a merged worktree must sit untouched before removal. Short,
        /// because disks fill fast; the in-use check guards live sessions.
        #[serde(default = "default_min_idle_minutes")]
        min_idle_minutes: u64,
    },
    NodeModules {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_node_modules_stale_days")]
        stale_days: u32,
        /// Also run `npm cache clean --force` and `pnpm store prune`.
        #[serde(default)]
        prune_package_caches: bool,
    },
    CargoClean {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_cargo_mode")]
        mode: CargoMode,
        #[serde(default = "default_worktree_stale_days")]
        stale_days: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depth: Option<u32>,
    },
    /// Python virtualenvs (`.venv`, `venv`) in idle projects.
    PythonVenv {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_node_modules_stale_days")]
        stale_days: u32,
    },
    /// Build output folders that git ignores, in idle projects.
    BuildOutput {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_node_modules_stale_days")]
        stale_days: u32,
        /// Folder names to look for. Each must be a plain name, not a path.
        #[serde(default = "default_build_output_dirs")]
        dirs: Vec<String>,
    },
    /// Asks developer tools to prune their own caches. Each switch turns one
    /// tool on; a tool that is not installed is skipped.
    ToolCaches {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default = "default_true")]
        uv: bool,
        #[serde(default = "default_true")]
        homebrew: bool,
        #[serde(default = "default_true")]
        simulators: bool,
        #[serde(default = "default_true")]
        bun: bool,
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
fn default_min_idle_minutes() -> u64 {
    30
}
fn default_node_modules_stale_days() -> u32 {
    30
}
fn default_cargo_mode() -> CargoMode {
    CargoMode::Stale
}
fn default_build_output_dirs() -> Vec<String> {
    ["dist", "build", ".next", "vendor"]
        .map(String::from)
        .to_vec()
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
    /// Local time of day for the daily scan, which runs whether or not a
    /// volume is low. `None` turns it off.
    pub daily_scan: Option<NaiveTime>,
    /// Absolute paths, with `~` expanded. Not canonicalised: a root on an
    /// unmounted drive must still be listed so the menu can show it greyed out.
    pub roots: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub volumes: Vec<VolumeOverride>,
    pub tasks: Vec<TaskConfig>,
    pub tools: ToolOverrides,
}

impl Config {
    /// The threshold for the volume mounted at `mount`, falling back to the default.
    ///
    /// Home folders live on the APFS Data volume, mounted at
    /// `/System/Volumes/Data`, but users think of the startup disk as `/`. Both
    /// share one container and report the same free space, so an override for
    /// `/` also applies to the Data volume.
    pub fn threshold_for(&self, mount: &Path, uuid: Option<&str>) -> Threshold {
        self.volumes
            .iter()
            .find(|v| v.matches(mount, uuid))
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
    daily_scan: Option<String>,
    roots: Option<Vec<String>>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    volume: Vec<RawVolume>,
    #[serde(default)]
    task: Vec<TaskConfig>,
    #[serde(default)]
    tools: RawTools,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTools {
    git: Option<String>,
    npm: Option<String>,
    pnpm: Option<String>,
    uv: Option<String>,
    brew: Option<String>,
    bun: Option<String>,
    xcrun: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVolume {
    mount: Option<String>,
    uuid: Option<String>,
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

    for task in &raw.task {
        validate_task(task)?;
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
        daily_scan: raw
            .daily_scan
            .as_deref()
            .map(parse_daily_time)
            .transpose()?,
        roots,
        exclude: raw.exclude,
        volumes,
        tasks: raw.task,
        tools: parse_tools(&raw.tools, home)?,
    })
}

/// The format `daily_scan` is written in, and the one the Settings window sends.
pub const DAILY_SCAN_FORMAT: &str = "%H:%M";

/// Parses a `daily_scan` value such as `"03:00"` (24-hour, local time).
fn parse_daily_time(raw: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(raw.trim(), DAILY_SCAN_FORMAT)
        .with_context(|| format!("daily_scan must be a 24-hour time like \"03:00\", got {raw:?}"))
}

/// Checks task options that serde cannot.
fn validate_task(task: &TaskConfig) -> Result<()> {
    if let TaskConfig::BuildOutput { dirs, .. } = task {
        if dirs.is_empty() {
            bail!("build_output dirs is empty; list folder names such as \"dist\"");
        }
        for d in dirs {
            if d.is_empty()
                || d == "."
                || d == ".."
                || d == ".git"
                || d.contains('/')
                || d.starts_with(':')
                || d.chars().any(|c| c.is_control() || "*?[]\\".contains(c))
            {
                bail!(
                    "build_output dirs entry {d:?} must be a plain folder name, not a path or pattern"
                );
            }
        }
    }
    Ok(())
}

fn parse_tools(raw: &RawTools, home: &Path) -> Result<ToolOverrides> {
    let one = |name: &str, value: &Option<String>| -> Result<Option<PathBuf>> {
        value
            .as_deref()
            .map(|v| expand_path(v, home).with_context(|| format!("[tools] {name}")))
            .transpose()
    };
    Ok(ToolOverrides {
        git: one("git", &raw.git)?,
        npm: one("npm", &raw.npm)?,
        pnpm: one("pnpm", &raw.pnpm)?,
        uv: one("uv", &raw.uv)?,
        brew: one("brew", &raw.brew)?,
        bun: one("bun", &raw.bun)?,
        xcrun: one("xcrun", &raw.xcrun)?,
    })
}

fn parse_volume(v: &RawVolume, home: &Path) -> Result<VolumeOverride> {
    let label = v
        .mount
        .as_deref()
        .or(v.uuid.as_deref())
        .unwrap_or("(no mount or uuid)");
    let scope = format!("[[volume]] {label}");
    if v.mount.is_none() && v.uuid.is_none() {
        bail!("{scope}: set mount, uuid, or both");
    }
    let threshold = Threshold::from_keys(v.min_free_gb, v.min_free_pct, &scope)?
        .with_context(|| format!("{scope}: set min_free_gb or min_free_pct"))?;
    let mount = v
        .mount
        .as_deref()
        .map(|m| expand_path(m, home).with_context(|| format!("{scope}: mount")))
        .transpose()?;
    let uuid = v
        .uuid
        .as_deref()
        .map(|u| parse_uuid(u).with_context(|| format!("{scope}: uuid")))
        .transpose()?;
    Ok(VolumeOverride {
        mount,
        uuid,
        threshold,
    })
}

/// Accepts a UUID in the 8-4-4-4-12 form and returns it upper-cased.
fn parse_uuid(raw: &str) -> Result<String> {
    let groups: Vec<&str> = raw.trim().split('-').collect();
    let well_formed = groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()));
    if !well_formed {
        bail!("{raw:?} is not a UUID like 2628355E-9A10-43A3-A8F9-B992912277BA");
    }
    Ok(raw.trim().to_ascii_uppercase())
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
        assert_eq!(c.tasks.len(), 5);
        assert_eq!(
            c.tasks[2],
            TaskConfig::CargoClean {
                enabled: true,
                mode: CargoMode::Stale,
                stale_days: 14,
                depth: None
            }
        );
        assert_eq!(
            c.tasks[3],
            TaskConfig::PythonVenv {
                enabled: true,
                stale_days: 30
            }
        );
        assert_eq!(
            c.tasks[4],
            TaskConfig::BuildOutput {
                enabled: true,
                stale_days: 30,
                dirs: vec![
                    "dist".into(),
                    "build".into(),
                    ".next".into(),
                    "vendor".into()
                ]
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
    fn daily_scan_is_off_unless_set() {
        assert_eq!(parse_ok(DEFAULT_TOML).daily_scan, None);
        let c = parse_ok("daily_scan = \"03:30\"\nmin_free_gb = 1\nroots = [\"/a\"]");
        assert_eq!(c.daily_scan, NaiveTime::from_hms_opt(3, 30, 0));
    }

    #[test]
    fn rejects_bad_daily_scan_times() {
        for bad in ["25:00", "12:60", "noon", "", "03:00:00", "3pm"] {
            let e = parse_err(&format!(
                "daily_scan = {bad:?}\nmin_free_gb = 1\nroots = [\"/a\"]"
            ));
            assert!(e.contains("daily_scan"), "{bad:?}: {e}");
        }
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
            c.threshold_for(Path::new("/Volumes/Projects"), None),
            Threshold::Pct(5.0)
        );
        assert_eq!(c.threshold_for(Path::new("/"), None), Threshold::Gb(30.0));
    }

    #[test]
    fn root_override_applies_to_the_data_volume() {
        let c = parse_ok(
            "min_free_gb = 30\nroots = [\"/a\"]\n[[volume]]\nmount = \"/\"\nmin_free_gb = 5\n",
        );
        assert_eq!(
            c.threshold_for(Path::new("/System/Volumes/Data"), None),
            Threshold::Gb(5.0)
        );
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/X"), None),
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
    fn parses_tool_overrides_and_rejects_unknown_tools() {
        let c = parse_ok(
            "min_free_gb = 1\nroots = [\"/a\"]\n[tools]\ngit = \"/opt/homebrew/bin/git\"\nnpm = \"~/bin/npm\"\n",
        );
        assert_eq!(c.tools.get("git"), Some(Path::new("/opt/homebrew/bin/git")));
        assert_eq!(c.tools.get("npm"), Some(Path::new("/Users/test/bin/npm")));
        assert_eq!(c.tools.get("pnpm"), None);
        let c = parse_ok(
            "min_free_gb = 1\nroots = [\"/a\"]\n[tools]\nuv = \"~/.local/bin/uv\"\nbrew = \"/opt/homebrew/bin/brew\"\nbun = \"/b/bun\"\nxcrun = \"/usr/bin/xcrun\"\n",
        );
        assert_eq!(
            c.tools.get("uv"),
            Some(Path::new("/Users/test/.local/bin/uv"))
        );
        assert_eq!(
            c.tools.get("brew"),
            Some(Path::new("/opt/homebrew/bin/brew"))
        );
        assert_eq!(c.tools.get("bun"), Some(Path::new("/b/bun")));
        assert_eq!(c.tools.get("xcrun"), Some(Path::new("/usr/bin/xcrun")));
        assert!(
            parse_err("min_free_gb = 1\nroots = [\"/a\"]\n[tools]\ncargo = \"/x\"")
                .contains("cargo")
        );
        assert!(
            parse_err("min_free_gb = 1\nroots = [\"/a\"]\n[tools]\ngit = \"git\"")
                .contains("absolute")
        );
    }

    fn private(p: &Path) -> Result<(), String> {
        open_private(p).map(|_| ())
    }

    #[test]
    fn open_nofollow_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        std::fs::write(&real, "x").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_nofollow(&real).is_ok());
        let err = open_nofollow(&link).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{err}");
    }

    #[test]
    fn a_folder_path_is_not_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("config.toml");
        std::fs::create_dir(&sub).unwrap();
        assert!(private(&sub).is_err());
    }

    #[test]
    fn file_must_be_mine_and_not_group_or_world_writable() {
        assert_eq!(file_problem(501, 0o100600, 501), None);
        assert_eq!(file_problem(501, 0o100644, 501), None);
        assert!(file_problem(501, 0o100664, 501).unwrap().contains("group"));
        assert!(
            file_problem(0, 0o100644, 501)
                .unwrap()
                .contains("another user")
        );
    }

    #[test]
    fn folders_may_be_root_owned_or_sticky() {
        assert_eq!(dir_problem(501, 0o040755, 501), None);
        assert_eq!(dir_problem(0, 0o040755, 501), None);
        assert_eq!(dir_problem(0, 0o041777, 501), None, "sticky /tmp is fine");
        assert!(dir_problem(0, 0o040777, 501).unwrap().contains("group"));
        assert!(dir_problem(501, 0o040775, 501).unwrap().contains("group"));
        assert!(
            dir_problem(502, 0o040755, 501)
                .unwrap()
                .contains("another user")
        );
    }

    #[test]
    fn real_config_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let f = dir.path().join("config.toml");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(private(&f), Ok(()));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(private(&f).unwrap_err().contains("writable by group"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            private(&f).is_err(),
            "a world-writable folder lets others replace the file"
        );
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn writable_grandparent_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let top = tempfile::tempdir().unwrap();
        let inner = top.path().join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        let f = inner.join("config.toml");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(private(&f), Ok(()));
        let a = top.path().join("a");
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = private(&f).unwrap_err();
        assert!(err.contains("/a ") && err.contains("writable"), "{err}");
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn symlinked_config_is_checked_where_it_really_lives() {
        use std::os::unix::fs::PermissionsExt;
        let real_dir = tempfile::tempdir().unwrap();
        let real = real_dir.path().join("real.toml");
        std::fs::write(&real, "x").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o666)).unwrap();
        let link_dir = tempfile::tempdir().unwrap();
        let link = link_dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(private(&link).unwrap_err().contains("writable"));
    }

    #[test]
    fn load_refuses_a_shared_config_and_reads_a_private_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("config.toml");
        std::fs::write(&f, "min_free_gb = 1\nroots = [\"/a\"]\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&f, Path::new("/Users/t")).is_ok());
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        let err = format!("{:#}", load(&f, Path::new("/Users/t")).unwrap_err());
        assert!(
            err.contains("not using") && err.contains("writable"),
            "{err}"
        );
    }

    #[test]
    fn build_output_dirs_must_be_plain_names() {
        let base = "min_free_gb = 1\nroots = [\"/a\"]\n[[task]]\ntype = \"build_output\"\n";
        assert!(matches!(
            &parse_ok(base).tasks[0],
            TaskConfig::BuildOutput { dirs, .. } if dirs.len() == 4
        ));
        for bad in [
            "[]",
            "[\"\"]",
            "[\"..\"]",
            "[\".git\"]",
            "[\"a/dist\"]",
            "[\"dist\\n\"]",
            "[\":dist\"]",
            "[\"dist*\"]",
            "[\"[ab]\"]",
        ] {
            let err = parse_err(&format!("{base}dirs = {bad}\n"));
            assert!(err.contains("build_output dirs"), "{bad}: {err}");
        }
    }

    #[test]
    fn tool_caches_turns_every_tool_on_by_default() {
        let base = "min_free_gb = 1\nroots = [\"/a\"]\n[[task]]\ntype = \"tool_caches\"\n";
        assert_eq!(
            parse_ok(base).tasks[0],
            TaskConfig::ToolCaches {
                enabled: true,
                uv: true,
                homebrew: true,
                simulators: true,
                bun: true
            }
        );
        assert!(matches!(
            parse_ok(&format!("{base}homebrew = false\n")).tasks[0],
            TaskConfig::ToolCaches {
                homebrew: false,
                uv: true,
                ..
            }
        ));
        assert!(parse_err(&format!("{base}docker = true\n")).contains("docker"));
    }

    #[test]
    fn package_cache_pruning_is_off_by_default() {
        let base = "min_free_gb = 1\nroots = [\"/a\"]\n[[task]]\ntype = \"node_modules\"\n";
        assert!(matches!(
            parse_ok(base).tasks[0],
            TaskConfig::NodeModules {
                prune_package_caches: false,
                ..
            }
        ));
        assert!(matches!(
            parse_ok(&format!("{base}prune_package_caches = true\n")).tasks[0],
            TaskConfig::NodeModules {
                prune_package_caches: true,
                ..
            }
        ));
    }

    #[test]
    fn uuid_override_follows_the_drive_not_the_path() {
        let c = parse_ok(
            "min_free_gb = 30\nroots = [\"/a\"]\n[[volume]]\nmount = \"/Volumes/P\"\nuuid = \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\"\nmin_free_pct = 5\n",
        );
        let ours = Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        let other = Some("11111111-2222-3333-4444-555555555555");
        // Renamed drive: new mount, same UUID.
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/Renamed"), ours),
            Threshold::Pct(5.0)
        );
        // A different drive at the old mount point.
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/P"), other),
            Threshold::Gb(30.0)
        );
        // UUID unknown: fall back to the mount point.
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/P"), None),
            Threshold::Pct(5.0)
        );
    }

    #[test]
    fn uuid_only_override_and_bad_uuid() {
        let c = parse_ok(
            "min_free_gb = 30\nroots = [\"/a\"]\n[[volume]]\nuuid = \"AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\"\nmin_free_gb = 2\n",
        );
        assert_eq!(
            c.threshold_for(Path::new("/Volumes/P"), None),
            Threshold::Gb(30.0)
        );
        assert_eq!(
            c.threshold_for(
                Path::new("/Volumes/P"),
                Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")
            ),
            Threshold::Gb(2.0)
        );
        assert!(
            parse_err(
                "min_free_gb = 1\nroots = [\"/a\"]\n[[volume]]\nuuid = \"nope\"\nmin_free_gb = 1\n"
            )
            .contains("not a UUID")
        );
        assert!(
            parse_err("min_free_gb = 1\nroots = [\"/a\"]\n[[volume]]\nmin_free_gb = 1\n")
                .contains("mount, uuid")
        );
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
