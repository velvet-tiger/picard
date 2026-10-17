---
name: portly
description: >-
  Find and manage development servers running on this machine using the `portly` CLI.
  Use when the user asks what's running on a port, wants to find a dev server, needs
  to free up or kill a port, is debugging "port already in use" or "address already
  in use" errors, or wants to see what an agent (Claude, Cursor, etc.) left running
  in the background. Triggers on "what's on port", "what's running on port", "find
  dev servers", "kill the process on port", "port already in use", "free up port",
  "check running servers", or "portly".
---

# portly — find and manage dev servers

`portly` reads the local socket table and works out which listening ports are
dev servers (as opposed to editors, chat apps, and OS daemons), so you don't
have to grep `lsof` output by hand.

Check `portly --version` or `which portly` first — if it's missing, install
with `brew install velvet-tiger/tap/portly` (macOS/Linux x86_64) or
`go install github.com/velvet-tiger/portly@latest`, and ask before installing
if the user hasn't already asked for it.

## Listing servers

```bash
portly
```

Shows only rows portly is confident are dev servers, as a table:

```
╭───────┬──────────────────┬────────────┬───────┬─────┬──────────────────────────────────────────────────────────╮
│ PORT  │ WHAT             │ PROJECT    │ PID   │ UP  │ WHY                                                      │
├───────┼──────────────────┼────────────┼───────┼─────┼──────────────────────────────────────────────────────────┤
│ 5174  │ node vite        │ storefront │ 28050 │ 11m │ launched by cursor (pid 12470)                           │
│ 5432  │ storefront-db-1  │ storefront │ 13546 │ 28m │ container storefront-db-1 in compose project storefront  │
╰───────┴──────────────────┴────────────┴───────┴─────┴──────────────────────────────────────────────────────────╯
2 shown, 40 hidden (38 application, 1 system, 1 unattributed). Use --all to see them.
```

The last line always prints — it names how many rows were hidden and why, so a
short list is never mistaken for "nothing found." If the port you're looking
for isn't in the default view, rerun with `--all`.

Flags:

| Flag | Effect |
|---|---|
| `--all` | Show every listening port, including applications and system daemons |
| `--json` | Emit JSON instead of a table (use this when parsing output programmatically) |
| `--probe` | Send a real HTTP GET to each port shown and report what it serves. Wakes idle servers and shows in their access logs, so it's off by default — mention this if suggesting it |
| `--probe-timeout` | How long each probe waits, default `400ms` |
| `--no-color` | Disable colour |
| `--version` | Print the version and exit |

`--json` output shape (one object): `scanned_at`, `shown`, `hidden` (counts by
category), and `listeners[]` — each with `port`, `protocol`, `addresses`,
`relevance`, `reason`, `runtime`, `framework`, `agent`, `pid`, `parent_pid`,
`process`, `command_line`, `user`, `working_directory`, `project`.

## Killing a server

```bash
portly kill <port>
portly kill --force <port>   # SIGKILL instead of SIGTERM; flags come before the port
```

Finds the process listening on that port and signals it. A worker pool (e.g.
php-fpm master + workers sharing one port) is killed together.

If the port is published by a Docker container, portly refuses to signal it —
the process behind it is Docker's shared backend, so killing it would affect
every container, not just this port. It prints the `docker stop <container>`
command to run instead; run that command rather than trying to kill the PID
yourself.

**Killing a process is a real, hard-to-reverse action against the user's
running environment** — confirm the port/process with the user before running
`portly kill`, unless they already named the specific port to kill.

## How classification works (for explaining a surprising result)

Rules run in confidence order, first match wins, and the `WHY` column names
which rule fired:

1. A container publishes this host port (`docker ps` supplies name/project).
2. A coding agent is in the parent process chain — strongest signal, beats everything below.
3. The process is a desktop app (executable lives in a `.app` bundle).
4. Installed software under `~/Library`, `~/.cursor`, etc. (version-manager interpreters like nvm are exempt — judged by the script they run).
5. The process runs from a system directory like `/usr/libexec`.
6. A dev runtime started from an editor, terminal, or shell.
7. A dev runtime whose working directory sits inside a project (nearest `.git`/`package.json`/`go.mod`/etc. wins in a monorepo).
8. A recognised framework in the command line.

Anything left over is `unattributed` — no rule explained it, but it may still
be a real server; check with `--all`.

## Known limits

- macOS and Linux only.
- Without `sudo`, ports held by another user or root are invisible — rerun under `sudo` if something expected is missing.
- If an agent backgrounds a server and its shell exits, the server reparents to `launchd` and rule 2 (agent attribution) no longer applies — it usually still surfaces via rule 7 (project directory), but `WHY` will name the project instead of the agent.
