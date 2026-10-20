//! Command-line entry points that reuse the runner without the menu bar UI.
//!
//! `picard scan` lists what would be removed and changes nothing.
//! `picard run --yes` cleans every volume that is below its target.
//! Both accept `--config <path>` to use a config other than the default.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config};
use crate::disk::{self, DiskSpace};
use crate::env::ToolEnv;
use crate::runner::{self, CleanRequest, DataPaths, TaskScan, VolumeRun};
use crate::tasks::SizeCache;
use crate::volume::{self, Volume};

const USAGE: &str = "usage:
  picard scan [--config <path>]        list what would be removed (changes nothing)
  picard run --yes [--config <path>]   remove it, on volumes below their target
  picard help";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Scan,
    Run,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    command: Command,
    config: Option<PathBuf>,
    yes: bool,
}

/// True when the first argument is a CLI command. Anything else starts the app.
pub fn is_cli(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("scan" | "run" | "help" | "--help" | "-h")
    )
}

/// Runs a CLI command and returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("picard: {e:#}\n{USAGE}");
            return 2;
        }
    };
    let result = match parsed.command {
        Command::Help => {
            println!("{USAGE}");
            return 0;
        }
        Command::Run if !parsed.yes => {
            eprintln!("picard: run removes files; add --yes to confirm\n{USAGE}");
            return 2;
        }
        Command::Scan => load(parsed.config).map(|(c, _, env)| scan(&c, &env)),
        Command::Run => load(parsed.config).and_then(|(c, path, env)| run(&c, &path, &env)),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("picard: {e:#}");
            1
        }
    }
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut iter = args.iter();
    let command = match iter.next().map(String::as_str) {
        Some("scan") => Command::Scan,
        Some("run") => Command::Run,
        Some("help" | "--help" | "-h") => Command::Help,
        other => bail!("unknown command {other:?}"),
    };
    let mut parsed = Args {
        command,
        config: None,
        yes: false,
    };
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--yes" => parsed.yes = true,
            "--config" => {
                let p = iter.next().context("--config needs a path")?;
                parsed.config = Some(PathBuf::from(p));
            }
            other => bail!("unknown option {other:?}"),
        }
    }
    Ok(parsed)
}

/// Reads the config and resolves tools the way the app does, so a CLI run from a
/// bare environment (cron, launchd, `env -i`) finds the same git and npm.
fn load(config_path: Option<PathBuf>) -> Result<(Config, PathBuf, ToolEnv)> {
    let home = std::env::home_dir().context("cannot find the home folder")?;
    let path = config_path.unwrap_or_else(|| config::config_path(&home));
    let config = config::load(&path, &home)?;
    let env = ToolEnv::detect(&home, std::env::var_os("SHELL"));
    Ok((config, path, env))
}

/// Volumes holding roots, with their current free space. Prints the ones it skips.
fn mounted_volumes(config: &Config) -> Vec<(Volume, DiskSpace)> {
    let mut map = volume::map_roots(&config.roots, volume::locate);
    volume::attach_uuids(&mut map.volumes, &volume::UuidCache::default());
    for u in &map.unavailable {
        println!("{}: {} ({})", u.name, u.reason, u.root.display());
    }
    map.volumes
        .into_iter()
        .filter_map(|v| match disk::query(&v.mount) {
            Ok(space) => Some((v, space)),
            Err(e) => {
                println!("{}: cannot read free space: {e}", v.name);
                None
            }
        })
        .collect()
}

fn scan(config: &Config, env: &ToolEnv) -> i32 {
    let sizes = SizeCache::default();
    let mut code = 0;
    for (volume, space) in mounted_volumes(config) {
        match runner::preview_volume(config, &volume, &sizes, env) {
            Ok(scans) => print!("{}", format_scan(&volume.name, space, &scans)),
            Err(e) => {
                eprintln!("picard: {}: {e:#}", volume.name);
                code = 1;
            }
        }
    }
    code
}

fn run(config: &Config, config_path: &std::path::Path, env: &ToolEnv) -> Result<i32> {
    let paths = DataPaths::beside(config_path);
    let sizes = SizeCache::default();
    let mut code = 0;
    for (volume, _) in mounted_volumes(config) {
        match runner::clean_volume(
            config,
            &volume,
            &sizes,
            &paths,
            env,
            CleanRequest::UntilTarget,
        ) {
            Ok(result) => {
                print!("{}", format_run(&volume.name, &result));
                if !result.failed.is_empty() {
                    code = 1;
                }
            }
            Err(e) => {
                println!("{}: cleanup failed: {e:#}", volume.name);
                code = 1;
            }
        }
    }
    println!("history: {}", paths.history.display());
    Ok(code)
}

fn format_scan(name: &str, space: DiskSpace, scans: &[TaskScan]) -> String {
    let mut out = format!("{name}: {}\n", disk::format_free(space));
    let mut total = 0;
    for s in scans {
        match &s.result {
            Err(e) => out.push_str(&format!("  {}: scan failed: {e}\n", s.task)),
            Ok(c) if c.is_empty() => out.push_str(&format!("  {}: nothing to remove\n", s.task)),
            Ok(c) => {
                for cand in c {
                    total += cand.bytes;
                    out.push_str(&format!(
                        "  {:<13} {:>9}  {}  ({})\n",
                        s.task,
                        cand.size_text(),
                        cand.path.display(),
                        cand.reason
                    ));
                }
            }
        }
    }
    out.push_str(&format!("  total: {}\n", disk::format_bytes(total)));
    out
}

fn format_run(name: &str, run: &VolumeRun) -> String {
    let mut out = format!("{name}:\n");
    for (task, r) in &run.removed {
        out.push_str(&format!(
            "  removed  {:<13} {:>9}  {}\n",
            task,
            disk::format_bytes(r.bytes),
            r.path.display()
        ));
    }
    for (task, path, why) in &run.failed {
        out.push_str(&format!(
            "  FAILED   {:<13} {}: {why}\n",
            task,
            path.display()
        ));
    }
    if let Some(why) = &run.stopped {
        out.push_str(&format!("  stopped: {why}\n"));
    }
    out.push_str(&format!(
        "  freed: {}\n",
        disk::format_bytes(run.freed_bytes())
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{Candidate, Estimate, Removed};

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn only_known_commands_are_cli() {
        assert!(is_cli(&s(&["scan"])));
        assert!(is_cli(&s(&["run", "--yes"])));
        assert!(!is_cli(&s(&[])));
        assert!(!is_cli(&s(&["-psn_0_12345"])));
    }

    #[test]
    fn parses_flags() {
        assert_eq!(
            parse_args(&s(&["run", "--yes", "--config", "/tmp/c.toml"])).unwrap(),
            Args {
                command: Command::Run,
                config: Some(PathBuf::from("/tmp/c.toml")),
                yes: true
            }
        );
        assert!(parse_args(&s(&["scan", "--config"])).is_err());
        assert!(parse_args(&s(&["scan", "--force"])).is_err());
    }

    #[test]
    fn run_without_yes_is_refused() {
        assert_eq!(main(&s(&["run"])), 2);
    }

    #[test]
    fn formats_scan_with_total() {
        let scans = vec![
            TaskScan {
                task: "node_modules",
                result: Ok(vec![Candidate {
                    path: PathBuf::from("/w/a/node_modules"),
                    bytes: 2_000_000_000,
                    reason: "untouched for 40 days".into(),
                    estimate: Estimate::Exact,
                }]),
            },
            TaskScan {
                task: "command",
                result: Ok(vec![Candidate {
                    path: PathBuf::from("command: docker"),
                    bytes: 0,
                    reason: "will run".into(),
                    estimate: Estimate::Unknown,
                }]),
            },
            TaskScan {
                task: "cargo_clean",
                result: Ok(vec![]),
            },
        ];
        let text = format_scan(
            "Startup disk",
            DiskSpace {
                free_bytes: 12_000_000_000,
                total_bytes: 994_000_000_000,
            },
            &scans,
        );
        assert_eq!(
            text,
            "Startup disk: 12 GB free (1%)\n  node_modules     2.0 GB  /w/a/node_modules  (untouched for 40 days)\n  command         unknown  command: docker  (will run)\n  cargo_clean: nothing to remove\n  total: 2.0 GB\n"
        );
    }

    #[test]
    fn formats_run_with_stop_reason() {
        let run = VolumeRun {
            removed: vec![(
                "cargo_clean",
                Removed {
                    path: PathBuf::from("/w/r/target"),
                    bytes: 5_000_000_000,
                },
            )],
            failed: vec![],
            stopped: Some("enough space is free".into()),
        };
        let text = format_run("X", &run);
        assert!(text.contains("removed  cargo_clean"));
        assert!(text.contains("stopped: enough space is free"));
        assert!(text.ends_with("freed: 5.0 GB\n"));
    }
}
