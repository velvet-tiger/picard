# Picard

Picard is a macOS menu bar app that watches free disk space. When a drive runs low, it finds build output and finished work you can safely remove: stale `node_modules`, Cargo `target/` folders, and merged git worktrees. It also runs any cleanup scripts you add.

It lives only in the menu bar. There is no Dock icon and no main window.

## What it removes, and what it never touches

Picard only works inside the folders you list as **roots**, such as `~/working`.

| Task | Removes | Only when |
|---|---|---|
| `git_worktrees` | Linked git worktrees | Clean, not locked, not in use by any running process, and their commits are already in `main` (or `master`) or fully pushed. Merged ones go after `min_idle_minutes`, others after `stale_days`. |
| `node_modules` | `node_modules` next to a `package.json` | The project has not changed for `stale_days`. |
| `cargo_clean` | `target/` next to a `Cargo.toml` | The project has not changed for `stale_days`, or every project with `mode = "all"`. |
| `command` | Whatever your script removes | It is enabled. |

These rules always hold:

- Nothing outside your roots is removed, and symlinks are never followed out of them.
- Git branches are never deleted. Worktrees are removed with `git worktree remove`, never with `--force`.
- A worktree with uncommitted or untracked changes is never removed.
- Every item is checked again just before it is removed.
- Every removal is logged to `history.jsonl`.
- Only one cleanup runs at a time.

One thing to know: removing a worktree also deletes files git ignores inside it, such as a local `.env`. Git's own `git worktree remove` behaves the same way.

## Modes

- **Ask** (the default): Picard notifies you, for example "Startup disk is low: 18 GB free (2%). About 8.4 GB can be cleaned." Choose **Clean…** to see the list, untick anything you want to keep, then press **Clean selected**.
- **Auto**: Picard cleans straight away and tells you how much it freed.
- **Off**: Picard only shows free space.

Picard never switches modes by itself.

## The menu bar icon

| Icon | Meaning |
|---|---|
| Empty drive | Every watched drive has enough space. |
| Half-filled drive | A cleanup is running. |
| Solid drive | A drive is below its threshold. |

The menu lists each drive with its free space, then **Clean…**, **Settings…** and **Open Log**.

## Install

1. Build it with `make dmg`, as described under [Building](#building).
2. Open `src-tauri/target/release/bundle/dmg/Picard_0.1.0_aarch64.dmg` and drag Picard to Applications.
3. Open Picard. macOS asks whether Picard may send notifications. Allow it, or the low-space alerts will not appear.
4. To start Picard when you log in, turn it on in **Settings…**.

## Settings

Everything is in **Settings…**. The settings live in `~/Library/Application Support/Picard/config.toml`. You can edit that file directly, and Picard reloads it when it changes. Saving from the Settings window keeps your comments.

```toml
poll_minutes = 5          # how often to check free space
min_free_gb = 30          # low below this; or use min_free_pct = 10 instead
recover_margin_gb = 10    # stop cleaning once threshold + margin is free
cooldown_minutes = 60     # wait before alerting again while still low
mode = "ask"              # auto | ask | off
roots = ["~/working"]     # Picard only removes things inside these folders
exclude = ["**/keep-*/**"] # glob patterns Picard never touches

[[task]]                  # tasks run in this order
type = "git_worktrees"
stale_days = 14
min_idle_minutes = 30

[[task]]
type = "node_modules"
stale_days = 30
prune_package_caches = false   # also run npm cache clean and pnpm store prune

[[task]]
type = "cargo_clean"
mode = "stale"            # or "all"
stale_days = 14

[[task]]
type = "command"
name = "Docker prune"
run = "docker system prune -f"
timeout_secs = 600
enabled = false
```

Every task also accepts `enabled = false`.

### Drives

Picard watches every drive that holds a root, external drives included. It cleans only the drive that is low. To give a drive its own threshold:

```toml
[[volume]]
mount = "/Volumes/Projects"
uuid = "2628355E-9A10-43A3-A8F9-B992912277BA"   # optional; keeps the setting if the drive is renamed
min_free_pct = 5
```

`diskutil info /Volumes/Projects` shows a drive's UUID. For the startup disk, use `mount = "/"`.

### Custom commands

A `command` task runs through your login shell (`/bin/zsh -lc`) from your home folder, with a time limit of `timeout_secs`. It receives:

- `PICARD_ROOTS`: the low drive's roots, separated by `:`
- `PICARD_FREE_BYTES`: free space before it started
- `PICARD_DRY_RUN`: always `0`, because Preview never runs scripts

Picard measures the space freed. A script can also print `PICARD_FREED=<bytes>`.

### Tool paths

Apps started from Finder do not see your shell's `PATH`. Picard asks your login shell for it once at startup. If it still picks the wrong tool, set it:

```toml
[tools]
git = "/opt/homebrew/bin/git"
npm = "~/.nvm/versions/node/v24.19.0/bin/npm"
pnpm = "/opt/homebrew/bin/pnpm"
```

## Command line

The app binary also works as a command-line tool:

```bash
/Applications/Picard.app/Contents/MacOS/picard scan
```

```bash
/Applications/Picard.app/Contents/MacOS/picard run --yes
```

`scan` lists what would be removed and changes nothing. `run --yes` cleans each drive that is below its target. Both accept `--config <path>`.

## Troubleshooting

**The menu says "Config error, see log".** Open the log from the menu. A common cause is a config file that other users could change. Picard refuses to use `config.toml` unless only you can change it and the folders above it. To fix it:

```bash
chmod go-w ~/Library/Application\ Support/Picard ~/Library/Application\ Support/Picard/config.toml
```

**No notifications appear.** Check System Settings, then Notifications, then Picard. In a development build (`make dev`), notifications come from "Terminal" instead, so allow them for Terminal.

**A worktree is never offered.** Picard keeps worktrees that a running process is using, such as an open Claude Code session or terminal. It also keeps any with uncommitted changes, unmerged commits, submodules, or filters set in the repo's own git config.

**Roots in Documents, Desktop or Downloads.** macOS asks for permission before an app reads these folders. Allow it when asked, or give Picard Full Disk Access in System Settings, then Privacy & Security.

**The log** is at `~/Library/Logs/com.velvettiger.picard/Picard.log`.

## Building

You need Rust, Node.js, pnpm and the Xcode command-line tools.

| Command | Does |
|---|---|
| `make setup` | Install JavaScript dependencies |
| `make dev` | Run Picard from source, rebuilding on change |
| `make check` | rustfmt, clippy, svelte-check and the tests |
| `make test-all` | The tests plus one that attaches a 200 MB disk image |
| `make scan` | Show what Picard would remove with your config |
| `make dmg` | Run the checks, then build the release `.app` and `.dmg` |
| `make verify` | Check the signature and the `.dmg` contents |

Builds are ad-hoc signed, which is enough to run them on the Mac that built them. To sign with a Developer ID instead:

```bash
make dmg APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
```

`security find-identity -v -p codesigning` lists your identities. Builds are for Apple Silicon only, and they are not notarised. Notarising is needed only to share the `.dmg` with other Macs.

The menu bar icons are drawn by `src-tauri/icons/make_tray_icons.py`. The app icon comes from `src-tauri/icons/app-icon.svg` through `pnpm tauri icon`.
