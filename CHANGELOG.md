# Changelog

All notable changes to Picard are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The Clean page shows how much Picard has freed in total, and since when. The total is summed from `history.jsonl` (`get_totals` command).

### Changed

- Worktrees merged into a branch other than the default one are now found. A merge commit or a reflog `merge` entry in any local branch counts as merged, so repos that integrate into a branch such as `2.0-dev` are cleaned after `min_idle_minutes`. A worktree whose commits another local branch holds, with no record of a merge, is cleaned after `stale_days`.

## [0.2.0] - 2026-10-21

### Added

- About tab with the running version, links to the Velvet Tiger website and GitHub issues, and a check for a newer release on GitHub. The check runs only when you open the tab or press Check again (debf414, a409677, 4ec3979).
- Appearance setting to make the window light or dark, or follow macOS (`appearance` in the config) (e13c16d).
- `activity_ignore` config key, edited in Settings under "Changes that don't count as work". It lists the names whose changes do not make a project look active. The defaults can be removed or restored (d3e1d68, a080985, a43ead0).

### Changed

- Changes to agent config such as `AGENTS.md`, `CLAUDE.md`, `.mcp.json`, `.automatic/` and `.claude/` no longer make a project look active. Stale build output in projects that tools like Automatic sync is now cleaned. Git checkouts inside those folders still count as work (d3e1d68).
- Worktrees with checked-out submodules are removed when nothing would be lost: every submodule is clean, filter-free and has no commit missing from its remote (622eea2).

### Security

- Every git call sets `GIT_NO_LAZY_FETCH=1` and `GIT_ALLOW_PROTOCOL=none`, so a crafted repository cannot make a status check fetch or run a command (622eea2).
- Just before a worktree is removed, Picard refuses it if anything inside it or its submodules changed since the run started (622eea2).
- A worktree that holds another git checkout is never removed (d3e1d68).

### Fixed

- A preview started while a clean is running is refused, instead of listing folders about to be deleted (7cf3d4e).

### Build / CI

- The release workflow publishes the release with this changelog as its notes. It refuses tags that are not on `main`, never replaces a published release, and checks the notarisation ticket on signed builds (3e22ac4, 831ccdd).

## [0.1.0] - 2026-10-20

The first release of Picard, a macOS menu bar app that watches free disk space
and removes build output and finished work when a drive runs low.

### Added

- Menu bar app with a live free-space line and an icon for the ok, low and cleaning states (ba31f0d, fe5c4ee, 20417e3).
- `config.toml` with roots, thresholds, hysteresis and a cooldown. Picard reloads it while running and maps each root to its volume (411290d, df30927, 692b508, 313c6af).
- Per-volume overrides in `[[volume]]`, matched by volume UUID. An override for `/` also applies to the APFS Data volume (d0af5de, e14df92).
- Ask, Auto and Off modes. Ask-mode alerts estimate how much space can be cleaned and offer a Clean button (b2cb955, 2df9d0f).
- Cleanup tasks:
  - `node_modules` and `cargo_clean` for stale build folders (a2d7d78).
  - `git_worktrees` for merged or pushed worktrees, never with `--force` (ea03a4d).
  - `python_venv` for stale virtualenvs (ecd8212).
  - `build_output` for git-ignored build folders such as `dist` and `.next` (7b1e821).
  - `tool_caches`, which asks uv, Homebrew, the iOS Simulator and bun to prune their own caches on the low drive (6aed048).
  - `command` for your own cleanup scripts, with timeouts (bd02014, c27a621).
  - Optional npm and pnpm cache pruning after `node_modules` cleanup (dccbab5).
- Runner with early stop, a cross-process run lock and a `history.jsonl` log of every removal (d60d5c8).
- `picard scan` and `picard run --yes` on the command line (c464676).
- Tool `PATH` taken from the login shell, with `[tools]` overrides (6e6dd50).
- One window with Clean and Settings tabs. Settings lists every task with a checkbox and keeps the comments in `config.toml` when it saves (2213dbe, 8197759, 49f0aa1, 8a2d7b1).
- Clean tab with an animated scanning panel, per-task select-all checkboxes and path tooltips (842a584, c04fb74).
- Ignore a folder from the Clean tab. Picard never removes an ignored folder or any folder that holds one (ac7bb8b, 5ffd047, cb9158a, 4e8f08a).
- Optional once-a-day scan at a set local time (215d9dc, 73d4f91, e1a22ab, b53eaff).
- Launch at login and a native folder picker for roots (8197759, ea4fa1a).
- Picard is released under the MIT License (37b48a3).

### Security

- Repository config cannot make git run programs during scans, including through submodules (582468f, c0875e0).
- Picard refuses `config.toml` unless only the current user can change it, and reads it through one `O_NOFOLLOW` handle (8aa6f47, d1439db).
- The `command` task only runs from a private `config.toml` (c27a621).

### Fixed

- Removals retry on `ENOTEMPTY`, and package cache pruning runs from the home folder (dc53850).
- Staleness checks ignore folder modification times and `.DS_Store` (8453a19).
- Worktrees are skipped only when their submodules are checked out (2c039c7).
- `build_output` reads folder names literally in git checks (a222924).
- Tool cache sizes show as a ceiling instead of 0 MB (3de5599).
- Long paths and option text no longer wrap in the window (261db4d, 327364d).
- Reloading the window stays on the active tab (b58136d).
- The Cargo mode dropdown and day count sit on one row in Settings (534ad69).

### Build / CI

- Makefile for checks and local release builds (8e8bc24).
- Release workflow that builds the `.dmg` and attaches it to a draft GitHub release. Actions are pinned to commit SHAs. Lint and tests run in a separate job with no signing secrets, and neither job uses the Actions cache (9c645fb, a411ea3, 8817cd4, f162bde, d6fe9ea).
- Dependabot updates for `actions/checkout`, `actions/setup-node` and `pnpm/action-setup` (0758dc7, 65fcd6a, 5083ae1).

### Testing

- Disk image volume test (a918137).
- Vitest tests for `api.ts`, Preview and Settings (5eeb28a).

[0.2.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.2.0
[0.1.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.1.0
