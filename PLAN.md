# Picard — Plan

A macOS menu bar app (Tauri v2) that watches free disk space and, when it falls below a configurable threshold, runs a configurable list of cleanup tasks.

## 1. Behaviour

- Lives only in the menu bar: no Dock icon, no main window (`ActivationPolicy::Accessory`).
- Polls free space on **every volume that holds a configured root** every N minutes (default 5). This includes external drives under `/Volumes/...`.
- **Volumes:** each root is mapped to its volume automatically (by comparing `st_dev`). Volumes can override the default threshold in a `[[volume]]` entry. A volume that isn't mounted is skipped quietly and greyed out in the menu.
- **Trigger:** a volume's free space drops below its threshold, given as GB (`min_free_gb = 30`) or percent (`min_free_pct = 10`). Only the roots on **that** volume are cleaned; cleaning other drives wouldn't help.
- Volume mounts and unmounts are picked up by watching `/Volumes` (`notify`), so a newly plugged-in drive is checked right away.
- **Hysteresis and cooldown:** after a run, Picard won't trigger again until free space has recovered above `threshold + margin`, or `cooldown_minutes` has passed. This stops it re-running endlessly when cleanup can't free enough.
- **Modes:**
  - `auto`: runs the tasks, then sends a notification with how much space was freed.
  - `ask`: sends a notification ("Disk at 8% free, about 42 GB can be cleaned") with a Clean button. **This is the default, and it only changes when you switch it yourself.** Picard never promotes itself to `auto`.
  - `off`: only monitors.
- **Manual actions** from the tray: Run now, Preview (dry run), and Open log.

## 2. Cleanup tasks

Every task has the same interface:

```rust
trait CleanupTask {
    fn id(&self) -> &str;
    fn scan(&self, ctx: &Ctx) -> Result<Vec<Candidate>>;      // what would be removed + estimated bytes
    fn run(&self, ctx: &Ctx, c: &[Candidate]) -> Result<Report>; // actually remove
}
```

Splitting `scan` from `run` gives us dry-run/preview, the size estimate in `ask` mode, and a log of every removal at no extra cost.

All tasks only work inside configured **roots** (default `~/working`) and skip anything matching `exclude` globs. Directory walking uses the `ignore`/`walkdir` crates with a depth limit, and never descends into `node_modules`, `target` or `.git`.

### 2.1 Stale git worktrees (`git_worktrees`)
1. Find repos under the roots (directories containing `.git`).
2. Run `git worktree prune` to clear metadata for worktrees whose directory no longer exists. This is always safe.
3. Parse `git worktree list --porcelain`. A linked worktree is a **candidate** when all of these hold:
   - it isn't the main worktree and isn't `locked`
   - `git status --porcelain` is empty (it never deletes uncommitted work)
   - its branch is merged into the default branch, **or** its upstream is gone, **or** nothing in it has changed for more than `stale_days` (default 14)
   - it has no unpushed commits (`git rev-list @{u}..HEAD` is empty, or the branch is merged)
4. Remove it with `git worktree remove <path>`, **never `--force`**. Only the worktree directory is removed; **branches are never deleted**.
5. Repos and worktrees can live on different volumes. The candidate is counted against the volume where the **worktree directory** is, since that's where the space is freed.

### 2.2 Cargo targets (`cargo_clean`)
There are two modes:
- `mode = "stale"` (default): find `Cargo.toml` + `target/` pairs whose sources haven't changed in `stale_days`, then run `cargo clean` in each one (or delete `target/`). This leaves the project you're working on today alone.
- `mode = "all"`: shell out to `cargo clean-recursive <root>` (already installed at `~/.cargo/bin`) with optional `--depth`.

### 2.3 Stale node_modules (`node_modules`)
- Find `node_modules` directories that sit next to a `package.json`, stopping at the first one found so nested copies aren't scanned.
- A directory is stale when the project (package.json, lockfile and source mtimes) hasn't changed in `stale_days` (default 30).
- Delete the directory directly; it can be regenerated, and moving it to the Trash would free nothing.
- An optional extra, off by default, runs `npm cache clean --force` / `pnpm store prune`.

### 2.4 Custom commands / shell scripts (`command`)
```toml
[[task]]
type = "command"
name = "Docker prune"
run = "docker system prune -f"
timeout_secs = 600
```
- Runs through `/bin/zsh -lc` with environment variables `PICARD_ROOTS`, `PICARD_DRY_RUN=1|0` and `PICARD_FREE_BYTES`.
- Picard measures freed bytes by comparing free space before and after. The script can also print `PICARD_FREED=<bytes>`.
- A script can't preview itself unless it honours `PICARD_DRY_RUN`, so in Preview it's listed as "will run" rather than with a size.

Tasks run **in the order listed** and the run **stops early** once free space is back above `threshold + margin`. Cheap or safe tasks go first.

## 3. Config

TOML at `~/Library/Application Support/Picard/config.toml`. It can be edited by hand or through the Settings window, and Picard reloads it when the file changes (the `notify` crate).

```toml
poll_minutes = 5
min_free_gb = 30          # default threshold; or min_free_pct = 10
recover_margin_gb = 10
cooldown_minutes = 60
mode = "ask"              # auto | ask | off — only changed by you
roots = ["~/working", "/Volumes/Projects/code"]
exclude = ["**/keep-*/**"]

# Optional per-volume overrides, matched by mount point (volume UUID is stored too,
# so a renamed or remounted drive keeps its settings)
[[volume]]
mount = "/Volumes/Projects"
min_free_pct = 5

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

[[task]]
type = "command"
name = "Custom script"
run = "~/bin/cleanup.sh"
enabled = false
```

## 4. Architecture

```
picard/
├── src-tauri/
│   ├── src/
│   │   ├── main.rs / lib.rs     # Tauri setup, tray, accessory policy
│   │   ├── tray.rs              # menu, icon state, handlers
│   │   ├── monitor.rs           # tokio poll loop, threshold/hysteresis state machine
│   │   ├── disk.rs              # free-space query
│   │   ├── config.rs            # serde TOML, defaults, hot-reload
│   │   ├── env.rs               # resolve login-shell PATH
│   │   ├── runner.rs            # orchestrates scan/run, early stop, reporting
│   │   ├── history.rs           # JSONL run log
│   │   └── tasks/{mod,git_worktrees,cargo_clean,node_modules,command}.rs
│   └── tauri.conf.json
└── src/                          # Settings + Preview window (Svelte 5 + Vite, TS)
```

- **Crates:** `tauri` v2, `tauri-plugin-notification`, `tauri-plugin-autostart` (launch at login), `tauri-plugin-log`, `tokio`, `serde`/`toml`, `walkdir`/`ignore`, `globset`, `notify`, `anyhow`, `chrono`, `libc` (statvfs).
- **Commands run from Rust** with `std::process::Command`/`tokio::process`. The frontend can't run commands (there's no shell plugin), so the IPC surface stays small: `get_status`, `get_config`, `save_config`, `preview`, `run_now`, `get_history`.
- **Tray icon:** template images in three states (ok / low / cleaning). The menu's title line shows "112 GB free (23%)".
- **Windows:** a Settings window (roots, threshold, mode, a reorderable task list with per-task options) and a Preview window (candidates grouped by task, with sizes and checkboxes). Both are created when needed, so Picard uses next to no memory while idle.

## 5. macOS gotchas to handle up front

1. **PATH:** apps launched from Finder or at login get a minimal PATH, so `git`, `cargo` and `npm` won't be found. At startup, read `zsh -lc 'echo $PATH'` once, merge it with `~/.cargo/bin` and `/opt/homebrew/bin`, and use the result for every child process. Also allow per-tool path overrides in config.
2. **What "free space" means:** APFS reports purgeable space differently. `statvfs` `f_bavail` gives a conservative number. A later improvement is to use `NSURLVolumeAvailableCapacityForImportantUsageKey` (through `objc2`) so the number matches Finder.
3. **Privacy prompts (TCC):** scanning `~/Documents`, `~/Desktop` or `~/Downloads` triggers permission prompts. Default roots should avoid those folders, and the docs should explain Full Disk Access for anyone who wants them.
4. **Sizing directories is slow:** large `node_modules` trees take time to measure. Size them in parallel on a blocking thread pool (`rayon`) and cache results by path+mtime.
5. **External volumes:** drives can be unplugged in the middle of a run, so every removal checks that the volume is still mounted and treats I/O errors as "stop this volume", not a crash. exFAT/FAT drives have coarse mtimes and no symlink semantics, so the staleness checks need to tolerate that. Volume UUIDs come from `diskutil info -plist` (or `DADiskCopyDescription`), so a renamed drive keeps its config.
6. Not sandboxed, and not for the App Store. Distribute signed and notarized (or unsigned, for personal use).

## 6. Safety rules (non-negotiable)

- Never act outside the configured roots, after canonicalising paths and refusing symlink escapes.
- Never `git worktree remove --force`, and never touch a dirty worktree or one with unpushed commits.
- `ask` mode is the default and stays on until you change it, so you see a preview before anything is deleted.
- Git branches are never deleted.
- Every removal is logged (path, task, bytes, time) to `history.jsonl`.
- A global lock means only one cleanup runs at a time.

## 7. Milestones

| # | Deliverable | Done when |
|---|---|---|
| M1 | Scaffold: Tauri v2 + Svelte, accessory app, tray with free-space line + Quit | Icon appears in the menu bar and shows live free space |
| M2 | Config + multi-volume monitor loop + threshold/hysteresis + notifications | Lowering the threshold above current usage fires one notification, not repeated ones; plugging in an external drive with a root on it adds it to the menu |
| M3 | Task trait + runner + `node_modules` + `cargo_clean` with dry run | Preview lists stale dirs with sizes; Run removes them; history logged |
| M4 | `git_worktrees` task | Merged/clean worktrees removed; dirty/locked/unpushed ones kept (tested) |
| M5 | `command` task + PATH resolution | Custom script runs from a login-launched app |
| M6 | Settings + Preview windows, launch-at-login, `ask` notification action | Everything configurable without editing TOML |
| M7 | Icons, signing/notarisation, README | `.dmg` that installs and runs |

## 8. Testing

- Unit tests for the threshold state machine (feed a sequence of free-space readings per volume and assert when it triggers).
- Mapping roots to volumes, tested on a disk image (`hdiutil create -size 200m` + attach) to stand in for an external drive, including unmounting it partway through a run.
- Task tests against `tempfile` fixtures: fake projects with backdated mtimes (`filetime` crate); real `git` repos with worktrees in clean, dirty, merged, locked and unpushed states.
- A `--dry-run` CLI entry point (`picard scan`) that reuses the same runner without the UI, which is handy for debugging and CI.

## 9. Decisions

1. **Volumes:** Picard monitors every volume that holds a root, external drives included, and cleans only the volume that's low.
2. **Mode:** `ask` stays the default until you change it.
3. **Worktrees:** only the directories are removed; branches are kept.
4. **Frontend:** Svelte 5 + Vite + TS.
