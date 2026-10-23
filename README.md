# Picard

Picard is a macOS menu bar app that watches free disk space. When a drive runs low, it finds build output and finished work you can safely remove: stale `node_modules`, Cargo `target/` folders, Python virtualenvs, git-ignored build folders, and merged git worktrees. It can also ask uv, Homebrew, the iOS Simulator and bun to prune their caches, and it runs any cleanup scripts you add.

It lives only in the menu bar. There is no Dock icon and no main window.

## What it removes, and what it never touches

Picard only works inside the folders you list as **roots**, such as `~/working`.

| Task | Removes | Only when |
|---|---|---|
| `git_worktrees` | Linked git worktrees | Clean, not locked, and not in use by any running process. Nothing is paused in them: no rebase, `git am`, merge, cherry-pick, revert, sequence of cherry-picks or reverts, bisect or notes merge. Done ones go after `min_idle_minutes`: another local branch holds their HEAD, or other local branches hold a cherry-picked copy of each commit that no other branch holds. The copies can be spread over several branches. Ones whose commits are only pushed, and done ones that made commits now on no branch, go after `stale_days`. |
| `node_modules` | `node_modules` next to a `package.json` | The project has not changed for `stale_days`. |
| `cargo_clean` | `target/` next to a `Cargo.toml` | The project has not changed for `stale_days`, or every project with `mode = "all"`. |
| `python_venv` | `.venv` or `venv` holding a `pyvenv.cfg`, next to a `pyproject.toml`, `requirements.txt`, `setup.py`, `setup.cfg` or `Pipfile` | The project has not changed for `stale_days`. |
| `build_output` | Folders named in `dirs` (`dist`, `build`, `.next`, `vendor` by default) | The project has not changed for `stale_days`, git ignores the folder, git tracks nothing inside it, and it holds no git repo of its own. |
| `tool_caches` | Whatever each tool's own prune command removes | The tool is installed, switched on, and keeps its cache on the low drive. Off unless you add it. |
| `command` | Whatever your script removes | It is enabled. |

These rules always hold:

- Nothing outside your roots is removed, and symlinks are never followed out of them. Tool caches are the exception: Picard never deletes them itself. It asks each tool to prune its own cache.
- Git branches are never deleted. Worktrees are removed with `git worktree remove`. A worktree with checked-out submodules needs `--force`, so Picard first checks each submodule itself: it must be clean, and every commit in it must be on a remote branch.
- A worktree with uncommitted or untracked changes is never removed. Neither is one that holds another git checkout, such as a Claude Code worktree in `.claude/worktrees/`.
- A worktree with a paused rebase, `git am`, merge, cherry-pick, revert, bisect or notes merge is never removed. Removing it would delete that operation's state.
- A worktree's own refs, under `refs/worktree/` and `refs/bisect/`, are deleted with it. A worktree is never removed while one of them points at a commit that no other local branch holds, or at anything that is not a commit, such as a tag object.
- A copied repo, made with `cp -R` or Finder's Duplicate, still lists the original repo's worktrees. Picard judges a worktree only through the repo it belongs to, and only by the HEAD it has now.
- Every item is checked again just before it is removed. A worktree is judged again from scratch, with its repo's branches read afresh. Its HEAD is read once more right before removal, and a HEAD that moved keeps it.
- A folder you ignore is never removed, in any mode. Neither is a folder that holds one, such as a worktree with an ignored `node_modules` inside it.
- Every removal is logged to `history.jsonl`.
- Only one cleanup runs at a time.

A project "has not changed" when no file in it has a newer modification time. Build output does not count. Neither does agent config that tools such as Automatic rewrite on every sync: `.agents`, `.automatic`, `.automatic.json`, `.claude`, `.codex`, `.cursor`, `.gemini`, `.junie`, `.mcp.json`, `.opencode`, `.windsurf`, `AGENTS.md`, `CLAUDE.md`, `GEMINI.md` and `opencode.json`. Edits you make to those by hand do not count either. Git checkouts inside them, such as `.claude/worktrees/*`, still count. Those are the defaults. Change the list in **Settings…** under "Changes that don't count as work", or set `activity_ignore` in the config. A list you set replaces the defaults, so it must name every default you want to keep. An empty list makes every change count. **Restore defaults** puts the default list back. When you save a list that holds exactly the defaults, Picard takes the key out of the config, with any comment above it, so later releases can update the defaults. This happens on any save from **Settings…**.

A worktree is done when another local branch holds its HEAD. Any branch counts except the worktree's own, even one checked out in another worktree. Remote-tracking branches do not count. Neither does a symbolic ref under `refs/heads`, because it is only another name for some other ref. Picard needs no default branch. A repo that integrates into `develop`, `2.0-dev` or `trunk` works without any setting.

A worktree is also done when other local branches hold a cherry-picked copy of each commit that no other branch holds. Commits that another branch already holds are not checked. The copies can be spread over several branches. Git's patch ids decide what is a copy, as when `git rebase` drops commits that are already upstream. Patch ids have limits:

- Only the change is compared. Commit messages, authors and dates are not.
- Whitespace is ignored everywhere, including indentation and the inside of string literals. An empty commit matches any other empty commit.
- A binary file that `.gitattributes` marks as text is compared as text.
- A merge commit has no patch id, so a merge among those commits fails this check.
- One copy matches every commit that makes the same change, such as a change made, reverted and made again. Two such commits fail this check.
- A copy still counts after its branch reverts it. A commit that reapplies a reverted change also counts as a copy of the original.

A failed check only means the worktree is not done this way. A worktree whose branch is pushed still goes after `stale_days`. The same holds when git cannot run the check, for example in a partial clone that lacks some files' contents.

A worktree can make a commit and then check out another one. That commit is then on no branch. Only the worktree's HEAD reflog reaches it, and removing the worktree deletes that reflog. So Picard reads the HEAD reflog of each done worktree. A commit made in the worktree that no other local branch holds, and that has no copy on one, makes the worktree wait for `stale_days`. The **Clean…** list then says "has commits on no branch". Commits the worktree only checked out or reset to do not count, because they existed already. That includes a commit made outside the reflog, for example with `git commit-tree`, and then reached with `checkout` or `reset`. A draft with a copy on a branch does not count either, such as a commit from before a plain rebase, or the first version of a commit whose message was amended. A squash or fixup rebase is different. It folds several commits into one, so the originals have no copy, and the worktree waits for `stale_days`. A reflog message Picard does not recognise counts as a commit made here, so it can only make a worktree wait. So does an empty message, except on the entry that created the worktree, which `git worktree add` writes with an old value of all zeros. Picard reads that old value from the reflog file itself, so a later empty message still counts after the creation entry expires. If git cannot look for copies, or the search would be too large, the worktree waits too. Without a reflog, for example with `core.logAllRefUpdates` off, there is nothing to read, and such commits are lost on removal.

The **Clean…** list says why a worktree is done. When git recorded a merge, it names it, for example "merged into 2.0-dev by 374877e". Otherwise it names the branch, as in "on main", or says "cherry-picked to another branch". A worktree whose commits are only pushed waits for `stale_days`. Any other worktree is kept. A squash merge of several commits is not a copy of any of them, so that worktree waits for `stale_days` if it was pushed, and is kept if not.

A branch that holds the commits does not prove the work is finished. A branch stacked on top of the worktree's branch counts. So does a branch that merged it in only to sync. For example, if feature branch B merges in unfinished work from feature branch A, A's worktree is done. A worktree's own branch is kept, so no commit is lost. A detached worktree has no branch. Once it is removed, its own commits are unreachable, and the next `git gc` can delete them. Only the copies remain. Picard logs the HEAD of each detached worktree it removes, so `git fsck --lost-found` can find that commit until then. Ignored files in the worktree, such as a local `.env`, are lost. In **Ask** mode you can untick or **Ignore** it.

One thing to know: removing a worktree also deletes files git ignores inside it, such as a local `.env`. Git's own `git worktree remove` behaves the same way. The same goes for `build_output`: it trusts your `.gitignore`. If you keep files you cannot rebuild in an ignored `build` folder, add that folder to `exclude`. Picard keeps a worktree that holds another checkout by finding its `.git`. A checkout made with `--separate-git-dir`, or one whose git folder lives elsewhere, has no `.git` inside the worktree, so exclude it yourself.

## Modes

- **Ask** (the default): Picard notifies you, for example "Startup disk is low: 18 GB free (2%). About 8.4 GB can be cleaned." Choose **Clean…** to see the list, untick anything you want to keep, then press **Clean selected**.
- **Auto**: Picard cleans straight away and tells you how much it freed.
- **Off**: Picard only shows free space.

Picard never switches modes by itself.

The **Clean…** page shows how much Picard has freed, and how many items, across every drive and mode since the first removal. It adds up `history.jsonl`, so deleting that file starts the total again from zero.

### Ignoring a folder

Each folder in the **Clean…** list has an **Ignore** button. An ignored folder stays in the list, greyed out and unticked, so you can find it again. No cleanup removes it, in any mode, and it is left out of the totals. Press **Unignore** to undo it.

A folder that holds an ignored folder is skipped too, because removing it would remove the ignored one. For example, ignoring `node_modules` inside a merged worktree keeps the whole worktree. Its row is greyed out with the note "holds an ignored folder". If you delete an ignored folder yourself, its entry stays until you remove it in **Settings…**. The folder stays ignored if it comes back, but it no longer protects the folder above it. After a cleanup, the results list each protected folder as "Skipped", with the reason.

A cleanup that is already running reads the list again before each task, so a folder you ignore mid-run is safe from the next task on. A folder you stop ignoring mid-run stays protected until the next run.

Ignored folders are saved to `ignore` in the config. **Settings…** lists them, with a **Remove** button on each. Use that list when a folder has dropped out of the **Clean…** list, for example because you worked on that project again. Ignoring matches one exact folder. Ignoring a project folder does not protect the `node_modules` inside it. To protect everything below a folder, use `exclude`, which hides matching folders from the list completely.

Tool caches and custom commands have no **Ignore** button. Switch them off in **Settings…** instead.

### Daily scan

Picard normally acts only when a drive drops below its threshold. Set `daily_scan` to also scan once a day at a set local time, even when space is fine. It follows the mode:

- **Ask**: Picard notifies you if anything can be cleaned, for example "Daily scan of Startup disk: 120 GB free (12%). About 8.5 GB can be cleaned." If nothing can be cleaned, it stays quiet. A drive that is already low gets its low-space alert instead.
- **Auto**: Picard runs every task in full on each drive. It does not stop early once space is free, as a low-space cleanup does. So tasks at the end of the list, including your `command` scripts, run every day. Picard tells you how much it freed, and tells you if every removal failed. If another cleanup is running at that moment, Picard tries that drive again at the next regular `poll_minutes` check. If a drive is low at the scan time, it gets a low-space cleanup and then the full one, so you may see two notifications.
- **Off**: nothing happens.

If your Mac is asleep or Picard is closed at that time, the scan runs at the next check that day. That can be up to `poll_minutes` after the Mac wakes. It never runs twice in one day. If you turn the daily scan on, or change its time, after that time has passed today, the first scan runs tomorrow. Picard records the date of the last scan, and the time it was set for, in `daily-scan.txt` next to the config. That is how it notices a change you make to `config.toml` while it is closed.

## The menu bar icon

| Icon | Meaning |
|---|---|
| Empty drive | Every watched drive has enough space. |
| Half-filled drive | A cleanup is running. |
| Solid drive | A drive is below its threshold. |

The menu lists each drive with its free space, then **Clean…**, **Settings…** and **Open Log**.

Picard checks free space every `poll_minutes`. It also checks when you point at or click the icon, when a scan ends, and when a cleanup is recorded. While the Picard window is open, it checks every 30 seconds. Each check updates the menu, the icon and the Clean tab. Any check notices a drive that drops below its threshold. While a drive stays low, its alert repeats only on the regular `poll_minutes` check, once `cooldown_minutes` has passed. A drive that recovers past its threshold plus `recover_margin_gb` is ready to alert again from the next regular `poll_minutes` check.

## About and updates

The **About** tab in Picard's window shows the version you are running. It also links to the Velvet Tiger website and to [GitHub issues](https://github.com/velvet-tiger/picard/issues).

Picard updates itself. About 30 seconds after it starts, and then once a day, it asks GitHub whether a newer release exists. If one does, Picard downloads it from a GitHub release, checks it against the public key built into the app, and replaces `Picard.app`. It never restarts by itself. The About tab and the menu then show **Restart to finish updating**. Until you restart, the old version keeps running, so an update never interrupts a cleanup. Picard refuses to restart while a cleanup is running.

The check is a plain download of one small file, `latest.json`. The request names the updater library and nothing about you, your Mac or your drives. GitHub sees your IP address, as it does for any request. A failed check is tried again after an hour, then two, then four, up to once a day. Picard contacts GitHub only for this check, for the download, and when you open the About tab or click **Check again**. The cleanup tools Picard runs, such as Homebrew, may use the network on their own.

To turn it off, clear **Check for updates daily and install them** in **Settings…**, or set `auto_update = false` in the config. The About tab then still checks when you open it. If it finds a newer version, it offers **Install it** instead of installing. Picard also does not install by itself while the config file has an error, or when it cannot write to the folder that holds `Picard.app`. Installing there would make macOS ask for an administrator password. Picard updates itself only when it runs from an installed `Picard.app`, so `make dev` never touches your build folder.

If an update fails, the About tab says why and links to the release page, where you can download the `.dmg`. Versions up to 0.3.0 do not update themselves, so install the first release that has automatic updates by hand.

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
daily_scan = "03:00"      # also scan once a day at this local time; leave out to turn off
appearance = "dark"       # system | light | dark; leave out to follow macOS
auto_update = false       # leave out to check daily and install updates
roots = ["~/working"]     # Picard only removes things inside these folders
exclude = ["**/keep-*/**"] # glob patterns Picard never touches or lists
ignore = ["~/working/site/node_modules"] # folders listed but never cleaned; set with Ignore
# activity_ignore = [...] # names whose changes never count as work; replaces the whole default list, so edit it in Settings

[[task]]                  # tasks run in this order
type = "git_worktrees"
stale_days = 14
min_idle_minutes = 30

[[task]]
type = "node_modules"
stale_days = 30
prune_package_caches = false   # also prune the npm and pnpm caches on the low drive

[[task]]
type = "cargo_clean"
mode = "stale"            # or "all"
stale_days = 14

[[task]]
type = "python_venv"
stale_days = 30

[[task]]
type = "build_output"      # only folders git ignores and tracks nothing in
stale_days = 30
dirs = ["dist", "build", ".next", "vendor"]

[[task]]
type = "tool_caches"       # not in the default config; add it to opt in
uv = true                 # uv cache prune
homebrew = true           # brew cleanup --prune=all
simulators = true         # xcrun simctl delete unavailable
bun = true                # bun pm cache rm

[[task]]
type = "command"
name = "Docker prune"
run = "docker system prune -f"
timeout_secs = 600
enabled = false
```

Every task also accepts `enabled = false`.

`dirs` entries must be plain folder names. Picard refuses paths, `.git`, and names with glob characters such as `*`.

`appearance` sets the Picard window to light or dark. Left out, the window follows the macOS setting. Saving **System** in Settings takes the key out of the config. A change from Settings applies when you press **Save**. A change made by editing the file applies the next time the window opens.

Preview shows each `tool_caches` step as "up to" its cache's current size, and leaves it out of the total. Each tool keeps what is still in use, so Picard cannot know in advance how much a prune frees. Picard measures the space freed after the run. A `command` task shows "unknown" for the same reason. Docker is not part of `tool_caches`. Use a `command` task for it, as in the example above.

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
uv = "~/.local/bin/uv"
brew = "/opt/homebrew/bin/brew"
bun = "~/.bun/bin/bun"
xcrun = "/usr/bin/xcrun"
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

**A worktree is never offered.** Picard keeps worktrees that a running process is using, such as an open Claude Code session or terminal. It also keeps any with uncommitted changes, commits that no other branch or remote holds, unsafe submodules, or filters set in the repo's own git config.

**Roots in Documents, Desktop or Downloads.** macOS asks for permission before an app reads these folders. Allow it when asked, or give Picard Full Disk Access in System Settings, then Privacy & Security.

**The log** is at `~/Library/Logs/com.velvettiger.picard/Picard.log`.

## Building

You need Rust, Node.js, pnpm and the Xcode command-line tools.

| Command | Does |
|---|---|
| `make setup` | Install JavaScript dependencies |
| `make dev` | Run Picard from source, rebuilding on change |
| `make check` | rustfmt, clippy, svelte-check and the tests |
| `make test` | The Rust tests, then the JavaScript tests |
| `make test-all` | The tests plus one that attaches a 200 MB disk image |
| `make scan` | Show what Picard would remove with your config |
| `make dmg` | Run the checks, then build the release `.app` and `.dmg` |
| `make bundle` | Build the release `.app` and `.dmg` without the checks |
| `make verify` | Check the signature and the `.dmg` contents |

The menu bar icons are drawn by `src-tauri/icons/make_tray_icons.py`. The app icon comes from `src-tauri/icons/app-icon.svg` through `pnpm tauri icon`.

## Update signing key

Installed copies of Picard accept an update only if it is signed with the update key. The public half is `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`. The private half and its password are the repository secrets `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Make a key with `pnpm tauri signer generate -w ~/.tauri/picard.key`.

Keep a backup of the private key and its password somewhere other than GitHub. If you lose them, no installed copy can accept another update, and everyone has to install a new `.dmg` by hand.

To change the key, ship the new public key first:

1. Make a new key and put its public half in `tauri.conf.json`.
2. Release a version signed with the old key. The release workflow refuses this, because it checks that the signing key matches the public key in the app. Skip the "Check the update signature matches the app" step for this one release.
3. Replace the secrets with the new key.
4. Later releases are signed with the new key. Copies that skipped step 2's release cannot update and need a new `.dmg`.

## Licence

Picard is released under the [MIT License](LICENSE).
