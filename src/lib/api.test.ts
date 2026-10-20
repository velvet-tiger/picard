import { describe, expect, it, vi } from 'vitest'
import {
  BUILT_IN_TASKS,
  errorText,
  formatBytes,
  getProtection,
  itemTitle,
  newTask,
  notifyIgnoreChanged,
  onIgnoreChanged,
  runSelected,
  saveSettings,
  setIgnored,
  shortPath,
  tildePath,
  withEveryBuiltIn,
  type IgnoreChange,
  type Settings,
  type Task,
  type TaskType,
} from './api'
import { fakeBackend } from './fake-backend'

describe('tildePath', () => {
  it('replaces the home folder with ~', () => {
    expect(tildePath('/Users/alice/working/picard')).toBe('~/working/picard')
  })

  it('replaces the home folder itself', () => {
    expect(tildePath('/Users/alice')).toBe('~')
  })

  it('leaves paths outside /Users alone', () => {
    expect(tildePath('/Volumes/Work/Users/alice/x')).toBe('/Volumes/Work/Users/alice/x')
    expect(tildePath('/opt/homebrew')).toBe('/opt/homebrew')
  })
})

describe('shortPath', () => {
  it('keeps a path of up to seven parts whole', () => {
    expect(shortPath('/Users/alice/a/b/c/d/e/f')).toBe('~/a/b/c/d/e/f')
  })

  it('elides the middle of a longer path, keeping the last four parts', () => {
    expect(shortPath('/Users/alice/working/harmony-proxy/.claude/worktrees/agent-1/src/x')).toBe(
      '~/working/…/worktrees/agent-1/src/x',
    )
  })

  it('works on paths outside the home folder', () => {
    expect(shortPath('/Volumes/Work/a/b/c/d/e/f')).toBe('/Volumes/…/c/d/e/f')
  })
})

describe('itemTitle', () => {
  it('names a worktree by its own folder', () => {
    expect(itemTitle('git_worktrees', '/Users/a/repo/.claude/worktrees/feature-x')).toBe(
      'feature-x',
    )
  })

  it('names folder candidates by their project and its parent', () => {
    expect(itemTitle('node_modules', '/Users/a/working/shop/web/node_modules')).toBe('shop/web')
    expect(itemTitle('cargo_clean', '/Users/a/working/picard/target')).toBe('working/picard')
  })

  it('falls back to the path when there is no parent to name', () => {
    expect(itemTitle('node_modules', 'node_modules')).toBe('node_modules')
    expect(itemTitle('git_worktrees', '/')).toBe('/')
  })

  it('strips the command prefix and keeps tool caches as written', () => {
    expect(itemTitle('command', 'command: Docker prune')).toBe('Docker prune')
    expect(itemTitle('tool_caches', 'Homebrew')).toBe('Homebrew')
  })
})

describe('formatBytes', () => {
  it.each([
    [0, '0 MB'],
    [12_345_678, '12 MB'],
    [1e9, '1.0 GB'],
    [2.46e9, '2.5 GB'],
    [10e9, '10 GB'],
    [123.4e9, '123 GB'],
    [1e12, '1.0 TB'],
    [1.56e12, '1.6 TB'],
  ])('formats %d bytes as %s', (bytes, text) => {
    expect(formatBytes(bytes)).toBe(text)
  })
})

describe('errorText', () => {
  it('passes strings from invoke through', () => {
    expect(errorText('config.toml: bad threshold')).toBe('config.toml: bad threshold')
  })

  it('uses the message of an Error', () => {
    expect(errorText(new Error('boom'))).toBe('boom')
  })

  it('shows anything else as JSON', () => {
    expect(errorText({ code: 2 })).toBe('{"code":2}')
  })

  it('still returns text for values JSON cannot show', () => {
    expect(errorText(undefined)).toBe('undefined')
  })

  it('returns text instead of throwing for values JSON rejects', () => {
    const circular: { self?: unknown } = {}
    circular.self = circular
    expect(errorText(10n)).toBe('10')
    expect(errorText(circular)).toBe('[object Object]')
  })
})

describe('newTask', () => {
  const types: TaskType[] = [...BUILT_IN_TASKS, 'command']

  it.each(types)('makes a %s task of that type', (type) => {
    expect(newTask(type).type).toBe(type)
  })

  it('starts a custom command switched off, so an empty command never runs', () => {
    expect(newTask('command')).toMatchObject({ enabled: false, run: '' })
  })

  it('returns a fresh object each time', () => {
    const first = newTask('build_output')
    if (first.type !== 'build_output') throw new Error('expected a build_output task')
    first.dirs.push('out')
    const second = newTask('build_output')
    if (second.type !== 'build_output') throw new Error('expected a build_output task')
    expect(second.dirs).toEqual(['dist', 'build', '.next', 'vendor'])
  })
})

describe('withEveryBuiltIn', () => {
  it('adds every built-in task, switched off, to an empty list', () => {
    const tasks = withEveryBuiltIn([])
    expect(tasks.map((t) => t.type)).toEqual([...BUILT_IN_TASKS])
    expect(tasks.every((t) => !t.enabled)).toBe(true)
  })

  it('keeps existing tasks first, in order, with their options', () => {
    const existing: Task[] = [
      { type: 'command', enabled: true, name: 'Prune', run: 'docker system prune -f', timeout_secs: 60 },
      { type: 'node_modules', enabled: true, stale_days: 7, prune_package_caches: true },
    ]
    const tasks = withEveryBuiltIn(existing)
    expect(tasks.slice(0, 2)).toEqual(existing)
    expect(tasks.slice(2).map((t) => t.type)).toEqual([
      'git_worktrees',
      'cargo_clean',
      'python_venv',
      'build_output',
      'tool_caches',
    ])
  })

  it('adds nothing when every built-in task is present', () => {
    const all = BUILT_IN_TASKS.map(newTask)
    expect(withEveryBuiltIn(all)).toEqual(all)
  })
})

describe('ignore-change events', () => {
  it('delivers a change to every listener until it unlistens', () => {
    const handler = vi.fn<(change: IgnoreChange) => void>()
    const unlisten = onIgnoreChanged(handler)
    const change: IgnoreChange = { path: '/Users/a/p/node_modules', ignored: true, source: 'clean' }

    notifyIgnoreChanged(change)
    expect(handler).toHaveBeenCalledExactlyOnceWith(change)

    unlisten()
    notifyIgnoreChanged({ ...change, ignored: false })
    expect(handler).toHaveBeenCalledOnce()
  })
})

// The Rust side reads these argument names, so a rename here breaks the call.
describe('backend commands', () => {
  it('sends set_ignored with path and ignored', async () => {
    const { sent } = fakeBackend({ set_ignored: () => null })
    await setIgnored('~/keep', true)
    expect(sent('set_ignored')).toEqual([{ path: '~/keep', ignored: true }])
  })

  it('sends get_protection with the paths in order', async () => {
    const { sent } = fakeBackend({ get_protection: () => ['ignored', null] })
    await expect(getProtection(['/a', '/b'])).resolves.toEqual(['ignored', null])
    expect(sent('get_protection')).toEqual([{ paths: ['/a', '/b'] }])
  })

  it('sends run_selected with the selections', async () => {
    const { sent } = fakeBackend({ run_selected: () => [] })
    await runSelected([{ mount: '/', paths: ['/x'] }])
    expect(sent('run_selected')).toEqual([{ selections: [{ mount: '/', paths: ['/x'] }] }])
  })

  it('sends save_settings with the settings', async () => {
    const { sent } = fakeBackend({ save_settings: () => null })
    const settings: Settings = {
      poll_minutes: 5,
      threshold: { unit: 'gb', value: 30 },
      recover_margin_gb: 10,
      cooldown_minutes: 60,
      mode: 'ask',
      daily_scan: null,
      roots: ['~/working'],
      exclude: [],
      tasks: [],
    }
    await saveSettings(settings)
    expect(sent('save_settings')).toEqual([{ settings }])
  })
})
