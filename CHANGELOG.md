# Changelog

All notable changes to Picard are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.7.0] - 2026-10-23

### Added

- When Picard removes a worktree with a detached HEAD, it logs that commit's id. No branch holds the commit, so `git gc` can delete it later. Until then, `git fsck --lost-found` can find it.

### Changed

- A worktree is done when another local branch holds its HEAD, even with no record of a merge. It is cleaned after `min_idle_minutes`, and the reason names the branch, for example "on dev". When that branch was not the default branch and git had no record of a merge, the worktree used to wait for `stale_days`. Symbolic refs under `refs/heads` do not count as branches.
- A worktree is also done when other local branches hold a cherry-picked copy of each commit that no other branch holds. The copies can be spread over several branches. The reason says "cherry-picked to another branch". A merge commit among those commits fails this check. So do two commits that make the same change, such as a change made, reverted and made again. A detached worktree whose commit was cherry-picked into `main` used to be kept forever.
- When git cannot run the cherry-pick check, for example in a partial clone that lacks some files' contents, Picard logs a warning. A pushed worktree then still goes after `stale_days`.
- Picard no longer looks for a default branch. A repo used to be skipped when `origin/HEAD` named no local branch and the repo had neither `main` nor `master`, such as a repo whose only branch is `trunk`. Its worktrees are now checked.
- A worktree whose HEAD is in `main` used to show "merged into main". It now names the merge when git recorded one, as in "merged into main (reflog)", and shows "on main" otherwise.
- Free space is checked more often. Picard checks when you point at or click the menu bar icon, when a scan ends, when a cleanup is recorded (including one from the `picard` command), and every 30 seconds while its window is open. Any of these checks notices a drive that drops below its threshold. A repeat alert for a drive that stays low still waits for the regular `poll_minutes` check after the cooldown. A drive that recovers is ready to alert again from the next regular `poll_minutes` check.
- An error reading a folder is logged when it first appears, and again only if the error changes. It used to be logged on every check.

### Fixed

- A worktree with a paused rebase, `git am`, merge, cherry-pick, revert, bisect or notes merge is never removed. Removing it would delete that operation's state.
- A done worktree whose HEAD reflog shows commits made in it, which no other local branch holds and which have no copy on one, now waits for `stale_days`. Its reason says "has commits on no branch". This covers a detached worktree that made a commit and then checked out `main`. Picard used to remove it after `min_idle_minutes`, which lost that commit. Commits the worktree only checked out or reset to do not count. An empty reflog message counts as a commit made here, except on the entry that created the worktree (old value all zeros), which `git worktree add` writes. A squash or fixup rebase leaves its original commits with no copy, so that worktree waits too. So does a worktree whose copies git cannot look for, or whose search would be too large. Without a reflog there is nothing to check.
- A worktree is kept while one of its own refs, under `refs/worktree/` or `refs/bisect/`, points at a commit that no other local branch holds, or at anything that is not a commit. Removing the worktree would delete that ref.
- Picard makes git write `log` output in UTF-8. Repo config such as `i18n.logOutputEncoding=UTF-16` made that output unreadable, so every done worktree in the repo was kept.
- A failed git command logs only the end of a long stderr.
- A copied repo, made with `cp -R` or Finder's Duplicate, still lists the original repo's worktrees, with the HEAD each had when copied. Picard judged those worktrees by that old HEAD and could remove one that held newer work. It now judges a worktree only through its own repo, by the HEAD it has now.
- Picard tells git not to read commit-graph files. A forged commit-graph could make a commit look as if a branch held it. Without the commit-graph, scans can be slower on very large repos.
- Every worktree is judged again just before it is removed, with its repo's branches read afresh. A worktree that is no longer done, or whose commits are no longer anywhere else, is kept. Its HEAD is read once more right before removal, and a worktree whose HEAD moved is kept.
- Picard passes branch lists to git on stdin. A repo with many thousands of branches no longer fails with an argument list that is too long. A failed git command logs only the start of a long argument list.
- A branch deleted while Picard reads a repo is now an error. Git used to read its name as a path when a file of that name existed.

## [0.6.0] - 2026-10-23

### Added

- The top of the Clean tab shows the disk size, the space available and the total freed.

## [0.5.0] - 2026-10-23

### Fixed

- Clicking a cleanup notification, such as "35 GB can be cleaned", opens the Clean tab on the scan behind that number. The tab used to open empty. It also shows when each volume was scanned.
- A newer background scan appears behind a **Show it** banner. It does not replace the list, so the ticks you changed stay.
- Picard refuses a scan that started before the volume's last clean, so folders it removed do not come back.
- Clean and Rescan wait until a stored scan is checked against the ignore list.
- **Show it** reads the stored scan again, so it shows the latest results.
- When a background clean removes folders on a volume, the open Clean tab says its list may be out of date.

## [0.4.0] - 2026-10-22

### Added

- Picard updates itself. It checks GitHub for a newer release shortly after it starts and then once a day, downloads it, verifies it against a public key built into the app, and replaces `Picard.app`. The menu and the About tab then offer **Restart to finish updating**. Picard never restarts by itself and refuses to restart during a cleanup. Turn it off with **Check for updates daily and install them** in Settings, or `auto_update = false` in the config. With it off, the About tab offers **Install it** when you check by hand.
- Updates are installed only from a GitHub release download, only when Picard runs from an installed `Picard.app`, and without an administrator password prompt. Otherwise the About tab offers **Install it**. A failed install is retried after 1, 2, 4 hours and so on, up to a day.
- The release workflow builds a signed update archive and a `latest.json` and attaches both to the release. It signs in a job of its own, so build scripts never see the key. It needs the `TAURI_SIGNING_PRIVATE_KEY` repository secret, and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if the key has a password. See "Update signing key" in the README.
- On first run, Picard shows a use-at-your-own-risk warning before it watches disk space or checks for updates. **I understand** continues, and **Quit** exits Picard. The warning shows once. Picard saves the acceptance as `warning-accepted.txt` next to the config, and existing installs see it once too.

### Changed

- The About tab shows the update progress: checking, downloading, installed, or failed with the reason. It follows a background download live.
- The update check uses the Tauri updater plugin instead of `curl`, which adds an HTTP and TLS client to the app. The README no longer says that Picard does not install updates.
- Versions before this one cannot update themselves. Install this release by hand once.

## [0.3.0] - 2026-10-21

### Added

- Each category on the Clean page has a chevron toggle. After every scan a category is closed when nothing in it can be cleaned, and open otherwise. It is also open when the scan failed, so the error shows. A closed heading shows a summary. Closing hides the list only, so Select all and Clean still reach it.
- The Clean page shows how much Picard has freed in total, and since when. The total is summed from `history.jsonl` (`get_totals` command).
- The total and the "Recently removed" list on the Clean page update while the window is open when auto mode, the daily scan or `picard run` removes something. Picard watches `history.jsonl` and sends the window a `history-changed` event. Only those two parts reload; the scan results stay as they are.

### Changed

- The Clean page no longer scans when it opens. Press Scan to find build folders, caches and stale worktrees. The total and the "Recently removed" list still load on open.
- Worktrees merged into a branch other than the default one are now found. A merge commit or a reflog `merge` entry in any local branch counts as merged, so repos that integrate into a branch such as `2.0-dev` are cleaned after `min_idle_minutes`. A worktree whose commits another local branch holds, with no record of a merge, is cleaned after `stale_days`.

### Fixed

- The scrollbar track is always reserved, so the tab bar and the cards keep the same width on every tab, whether or not the page scrolls.
- After a failed first scan the Clean page no longer shows an empty-state line. The button stays labelled Scan until a scan succeeds.

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

[0.7.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.7.0
[0.6.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.6.0
[0.5.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.5.0
[0.4.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.4.0
[0.3.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.3.0
[0.2.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.2.0
[0.1.0]: https://github.com/velvet-tiger/picard/releases/tag/v0.1.0
