// Typed wrappers for the Rust commands in src-tauri/src/commands.rs.
// The shapes here mirror the serde structs there; keep them in step.
import { getVersion } from '@tauri-apps/api/app'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow'

export type Mode = 'auto' | 'ask' | 'off'
export type CargoMode = 'stale' | 'all'
export type Appearance = 'system' | 'light' | 'dark'

export type Threshold = { unit: 'gb'; value: number } | { unit: 'pct'; value: number }

export type Task =
  | { type: 'git_worktrees'; enabled: boolean; stale_days: number; min_idle_minutes: number }
  | { type: 'node_modules'; enabled: boolean; stale_days: number; prune_package_caches: boolean }
  | { type: 'cargo_clean'; enabled: boolean; mode: CargoMode; stale_days: number; depth?: number }
  | { type: 'python_venv'; enabled: boolean; stale_days: number }
  | { type: 'build_output'; enabled: boolean; stale_days: number; dirs: string[] }
  | {
      type: 'tool_caches'
      enabled: boolean
      uv: boolean
      homebrew: boolean
      simulators: boolean
      bun: boolean
    }
  | { type: 'command'; enabled: boolean; name: string; run: string; timeout_secs: number }

export type TaskType = Task['type']

export interface Settings {
  poll_minutes: number
  threshold: Threshold
  recover_margin_gb: number
  cooldown_minutes: number
  mode: Mode
  /** Daily scan time as `HH:MM`, or null when the daily scan is off. */
  daily_scan: string | null
  appearance: Appearance
  roots: string[]
  exclude: string[]
  /** Names whose changes do not count as work: the defaults unless the file sets its own. */
  activity_ignore: string[]
  tasks: Task[]
}

export interface PreviewItem {
  path: string
  bytes: number
  size_text: string
  reason: string
  /** In the config's `ignore` list: shown, but never removed. */
  ignored: boolean
  /** Holds a folder in the `ignore` list, so it is never removed either. */
  holds_ignored: boolean
}

export interface PreviewTask {
  task: TaskType
  items: PreviewItem[]
  error: string | null
}

export interface VolumePreview {
  name: string
  mount: string
  free_text: string
  tasks: PreviewTask[]
  total_bytes: number
  total_text: string
}

export interface Selection {
  mount: string
  paths: string[]
}

export interface RunResult {
  name: string
  freed_text: string
  removed: { task: TaskType; path: string; size_text: string }[]
  failed: { task: TaskType; path: string; reason: string }[]
  /** Left alone because the ignore list protects them. */
  skipped: { task: TaskType; path: string; reason: string }[]
  stopped: string | null
  error: string | null
}

export interface HistoryEntry {
  time: string
  task: TaskType
  volume: string
  path: string
  bytes: number
}

/** Everything Picard has removed, summed from history.jsonl. */
export interface Totals {
  bytes: number
  items: number
  /** The oldest removal, or null when nothing has been removed. */
  since: string | null
}

export const getSettings = () => invoke<Settings>('get_settings')
/** The config's `appearance`, read without loading the whole form. */
export const getAppearance = () => invoke<Appearance>('get_appearance')
export const saveSettings = (settings: Settings) => invoke<void>('save_settings', { settings })
export const preview = () => invoke<VolumePreview[]>('preview')
export const runSelected = (selections: Selection[]) =>
  invoke<RunResult[]>('run_selected', { selections })
export const getHistory = (limit: number) => invoke<HistoryEntry[]>('get_history', { limit })
export const getTotals = () => invoke<Totals>('get_totals')
/** The default `activity_ignore` names, for Restore defaults. */
export const getNotActivity = () => invoke<string[]>('get_not_activity')
/** The config's `ignore` list as written, so `~/…` stays `~/…`. */
export const getIgnored = () => invoke<string[]>('get_ignored')
/** Adds a folder to the `ignore` list, or takes it off. Safe to repeat. */
export const setIgnored = (path: string, ignored: boolean) =>
  invoke<void>('set_ignored', { path, ignored })

export type Protection = 'ignored' | 'holds_ignored' | null

/** How the current ignore list protects each path, in order. Reads the config only; no scan. */
export const getProtection = (paths: string[]) =>
  invoke<Protection[]>('get_protection', { paths })

// The Clean and Settings tabs share one window and both edit the ignore list,
// so each tells the other when it changes. A DOM event is enough: they run in
// the same page.
const IGNORE_CHANGED = 'picard:ignore-changed'

export interface IgnoreChange {
  /** The path as the sending tab knows it: absolute from Clean, as written from Settings. */
  path: string
  ignored: boolean
  source: 'clean' | 'settings'
}

export function notifyIgnoreChanged(change: IgnoreChange): void {
  window.dispatchEvent(new CustomEvent<IgnoreChange>(IGNORE_CHANGED, { detail: change }))
}

/** Calls `handler` when either tab changes the ignore list. Returns an unlisten function. */
export function onIgnoreChanged(handler: (change: IgnoreChange) => void): () => void {
  const listener = (e: Event) => handler((e as CustomEvent<IgnoreChange>).detail)
  window.addEventListener(IGNORE_CHANGED, listener)
  return () => window.removeEventListener(IGNORE_CHANGED, listener)
}

/** Opens the folder picker; resolves to `~/…` or an absolute path, or null if cancelled. */
export const pickFolder = () => invoke<string | null>('pick_folder')
export const getLaunchAtLogin = () => invoke<boolean>('get_launch_at_login')
export const setLaunchAtLogin = (enabled: boolean) =>
  invoke<void>('set_launch_at_login', { enabled })

export const TASK_LABELS: Record<TaskType, string> = {
  git_worktrees: 'Git worktrees',
  node_modules: 'node_modules',
  cargo_clean: 'Cargo target folders',
  python_venv: 'Python virtualenvs',
  build_output: 'Ignored build output',
  tool_caches: 'Tool caches',
  command: 'Custom command',
}

/** Defaults for a newly added task, matching the Rust config defaults. */
export function newTask(type: TaskType): Task {
  switch (type) {
    case 'git_worktrees':
      return { type, enabled: true, stale_days: 14, min_idle_minutes: 30 }
    case 'node_modules':
      return { type, enabled: true, stale_days: 30, prune_package_caches: false }
    case 'cargo_clean':
      return { type, enabled: true, mode: 'stale', stale_days: 14 }
    case 'python_venv':
      return { type, enabled: true, stale_days: 30 }
    case 'build_output':
      return { type, enabled: true, stale_days: 30, dirs: ['dist', 'build', '.next', 'vendor'] }
    case 'tool_caches':
      return { type, enabled: true, uv: true, homebrew: true, simulators: true, bun: true }
    case 'command':
      return { type, enabled: false, name: 'My cleanup', run: '', timeout_secs: 600 }
  }
}

/**
 * Task types that Settings always lists, once each. `command` is the
 * exception: you can have none or several, so it is added by hand.
 */
export const BUILT_IN_TASKS = [
  'git_worktrees',
  'node_modules',
  'cargo_clean',
  'python_venv',
  'build_output',
  'tool_caches',
] as const satisfies readonly TaskType[]

/**
 * Appends each built-in task missing from `tasks`, switched off and with its
 * defaults, so Settings shows every task with a checkbox. Existing tasks keep
 * their order and options.
 */
export function withEveryBuiltIn(tasks: Task[]): Task[] {
  const present = new Set(tasks.map((t) => t.type))
  const missing = BUILT_IN_TASKS.filter((type) => !present.has(type)).map(
    (type): Task => ({ ...newTask(type), enabled: false }),
  )
  return [...tasks, ...missing]
}

/** The running version beside the latest GitHub release. Mirrors `UpdateCheck` in about.rs. */
export interface UpdateCheck {
  current: string
  latest: string
  /** True when `latest` is newer than `current`. */
  newer: boolean
}

/** The pages the About tab may open. The URLs live in about.rs, not here. */
export type Link = 'website' | 'issues' | 'latest_release'

/** The version Picard was built as, from tauri.conf.json. */
export const appVersion = () => getVersion()
/** Asks GitHub for the latest release. Rejects with a readable reason when it cannot. */
export const checkForUpdate = () => invoke<UpdateCheck>('check_for_update')
export const openLink = (link: Link) => invoke<void>('open_link', { link })

/**
 * Makes the page light, dark, or follow macOS. app.css keys its palettes on
 * `data-theme`. The Rust side sets the title bar to match.
 */
export function applyAppearance(appearance: Appearance): void {
  const root = document.documentElement
  if (appearance === 'system') delete root.dataset.theme
  else root.dataset.theme = appearance
}

/** Closes the window this page is in. */
export const closeWindow = () => getCurrentWindow().close()

/**
 * Calls `handler` with a tab id when the menu bar asks the open window to
 * switch tabs (see src-tauri/src/windows.rs). Resolves to an unlisten function.
 */
export const onShowTab = (handler: (tab: string) => void) =>
  getCurrentWebviewWindow().listen<string>('show-tab', (e) => handler(e.payload))

/** Errors from invoke arrive as strings; anything else is shown as-is. */
export function errorText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  // JSON.stringify gives undefined for undefined, functions and symbols, and
  // throws a TypeError for a BigInt or a circular object. This runs inside
  // catch blocks, so it must not throw.
  try {
    return JSON.stringify(e) ?? String(e)
  } catch (err) {
    if (err instanceof TypeError) return String(e)
    throw err
  }
}

/** Replaces the home folder with `~`. */
export function tildePath(path: string): string {
  return path.replace(/^\/Users\/[^/]+(?=\/|$)/, '~')
}

/**
 * A path short enough for one line: the first two parts after the home
 * folder, then `…`, then the last four, which keep the repo or project name
 * (for example `harmony-proxy/.claude/worktrees/agent-…`).
 */
export function shortPath(path: string): string {
  const parts = tildePath(path).split('/')
  if (parts.length <= 7) return parts.join('/')
  return [...parts.slice(0, 2), '…', ...parts.slice(-4)].join('/')
}

/** Task types whose items are folders, and so can be ignored. */
export const IGNORABLE_TASKS: readonly TaskType[] = [
  'git_worktrees',
  'node_modules',
  'cargo_clean',
  'python_venv',
  'build_output',
]

/** The most telling name for a candidate: its project, or the worktree itself. */
export function itemTitle(task: TaskType, path: string): string {
  const parts = path.split('/').filter(Boolean)
  if (task === 'command') return path.replace(/^command: /, '')
  if (task === 'tool_caches') return path
  if (task === 'git_worktrees') return parts.at(-1) ?? path
  // node_modules, target, virtualenvs and build output live directly inside the project folder; its
  // parent is included because names like `ts` or `web` say little alone.
  return parts.slice(-3, -1).join('/') || path
}

export function formatBytes(bytes: number): string {
  const GB = 1e9
  if (bytes >= 1e12) return `${(bytes / 1e12).toFixed(1)} TB`
  if (bytes >= 10 * GB) return `${Math.round(bytes / GB)} GB`
  if (bytes >= GB) return `${(bytes / GB).toFixed(1)} GB`
  return `${Math.round(bytes / 1e6)} MB`
}
