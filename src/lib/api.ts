// Typed wrappers for the Rust commands in src-tauri/src/commands.rs.
// The shapes here mirror the serde structs there; keep them in step.
import { invoke } from '@tauri-apps/api/core'

export type Mode = 'auto' | 'ask' | 'off'
export type CargoMode = 'stale' | 'all'

export type Threshold = { unit: 'gb'; value: number } | { unit: 'pct'; value: number }

export type Task =
  | { type: 'git_worktrees'; enabled: boolean; stale_days: number; min_idle_minutes: number }
  | { type: 'node_modules'; enabled: boolean; stale_days: number; prune_package_caches: boolean }
  | { type: 'cargo_clean'; enabled: boolean; mode: CargoMode; stale_days: number; depth?: number }
  | { type: 'command'; enabled: boolean; name: string; run: string; timeout_secs: number }

export type TaskType = Task['type']

export interface Settings {
  poll_minutes: number
  threshold: Threshold
  recover_margin_gb: number
  cooldown_minutes: number
  mode: Mode
  roots: string[]
  exclude: string[]
  tasks: Task[]
}

export interface PreviewItem {
  path: string
  bytes: number
  size_text: string
  reason: string
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

export const getSettings = () => invoke<Settings>('get_settings')
export const saveSettings = (settings: Settings) => invoke<void>('save_settings', { settings })
export const preview = () => invoke<VolumePreview[]>('preview')
export const runSelected = (selections: Selection[]) =>
  invoke<RunResult[]>('run_selected', { selections })
export const getHistory = (limit: number) => invoke<HistoryEntry[]>('get_history', { limit })
export const getLaunchAtLogin = () => invoke<boolean>('get_launch_at_login')
export const setLaunchAtLogin = (enabled: boolean) =>
  invoke<void>('set_launch_at_login', { enabled })

export const TASK_LABELS: Record<TaskType, string> = {
  git_worktrees: 'Git worktrees',
  node_modules: 'node_modules',
  cargo_clean: 'Cargo target folders',
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
    case 'command':
      return { type, enabled: false, name: 'My cleanup', run: '', timeout_secs: 600 }
  }
}

/** Errors from invoke arrive as strings; anything else is shown as-is. */
export function errorText(e: unknown): string {
  return typeof e === 'string' ? e : e instanceof Error ? e.message : JSON.stringify(e)
}

export function formatBytes(bytes: number): string {
  const GB = 1e9
  if (bytes >= 1e12) return `${(bytes / 1e12).toFixed(1)} TB`
  if (bytes >= 10 * GB) return `${Math.round(bytes / GB)} GB`
  if (bytes >= GB) return `${(bytes / GB).toFixed(1)} GB`
  return `${Math.round(bytes / 1e6)} MB`
}
