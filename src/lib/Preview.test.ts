import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte'
// The Tauri mock does not deliver `emitTo('main', …)` to a window's listener,
// so these tests send the global event. The Rust side targets the window.
import { emit } from '@tauri-apps/api/event'
import { mockWindows } from '@tauri-apps/api/mocks'
import { afterEach, describe, expect, it } from 'vitest'
import Preview from './Preview.svelte'
import {
  notifyIgnoreChanged,
  type HistoryEntry,
  type PreviewItem,
  type Protection,
  type RunResult,
  type Totals,
  type VolumePreview,
} from './api'
import { deferred, fakeBackend, type Handler } from './fake-backend'

const SHOP = '/Users/a/w/shop/web/node_modules'
const API = '/Users/a/w/api/core/node_modules'
const BLOG = '/Users/a/w/blog/site/node_modules'
const WORKTREE = '/Users/a/w/shop/.claude/worktrees/old-branch'

const item = (path: string, bytes: number, extra: Partial<PreviewItem> = {}): PreviewItem => ({
  path,
  bytes,
  size_text: `${bytes / 1e9} GB`,
  reason: 'untouched for 40 days',
  ignored: false,
  holds_ignored: false,
  ...extra,
})

/** One volume; BLOG starts ignored. Rows, in order: SHOP, API, BLOG, WORKTREE. */
function volumes(): VolumePreview[] {
  return [
    {
      name: 'Macintosh HD',
      mount: '/',
      free_text: '12 GB free',
      total_bytes: 0,
      total_text: '',
      tasks: [
        {
          task: 'node_modules',
          error: null,
          items: [item(SHOP, 2e9), item(API, 1e9), item(BLOG, 5e9, { ignored: true })],
        },
        { task: 'git_worktrees', error: null, items: [item(WORKTREE, 0.5e9)] },
      ],
    },
  ]
}

/** The scan above, with replies that change nothing. `handlers` override them per command. */
function backend(handlers: Record<string, Handler> = {}) {
  // The page listens for `history-changed` on its own window.
  mockWindows('main')
  return fakeBackend({
    preview: () => volumes(),
    get_history: () => [],
    get_totals: () => ({ bytes: 0, items: 0, since: null }) satisfies Totals,
    set_ignored: () => null,
    get_protection: () => [null, null, 'ignored', null] satisfies Protection[],
    run_selected: () => [],
    ...handlers,
  })
}

const box = (title: string) =>
  screen.getByRole<HTMLInputElement>('checkbox', { name: new RegExp(title) })

/**
 * Waits one macrotask. mockIPC answers inside `invoke`, so every reply and
 * Svelte's flush that follows are microtasks, and all of them have run by then.
 * Used to show that nothing more happens.
 */
const settle = () => new Promise((r) => setTimeout(r, 0))

/** The totals line, matched on its whole text; `start` is everything before the date. */
function findTotal(start: string) {
  const since = new Date('2026-09-03T08:30:00Z').toLocaleDateString(undefined, {
    day: 'numeric',
    month: 'short',
    year: 'numeric',
  })
  return screen.findByText(
    (_, el) =>
      el?.tagName === 'P' &&
      el.textContent?.replace(/\s+/g, ' ').trim() === `${start} since ${since}.`,
  )
}

async function renderScanned() {
  render(Preview)
  await fireEvent.click(screen.getByRole('button', { name: 'Scan' }))
  await screen.findByText('Macintosh HD')
}

describe('Preview', () => {
  // The page stops listening for `history-changed` when it unmounts. That needs
  // the Tauri mocks, which test-setup.ts clears in an afterEach that runs after this one.
  afterEach(async () => {
    cleanup()
    await settle()
  })

  it('waits for Scan before scanning, but loads the total straight away', async () => {
    const scan = deferred<VolumePreview[]>()
    const { sent } = backend({
      preview: () => scan.promise,
      get_totals: () => ({ bytes: 42.3e9, items: 318, since: '2026-09-03T08:30:00Z' }),
    })
    const prompt = 'Press Scan to find build folders, caches and stale worktrees.'
    render(Preview)

    expect(await findTotal('Picard has freed 42 GB across 318 items')).toBeInTheDocument()
    expect(screen.getByText(prompt)).toBeInTheDocument()
    expect(sent('get_history')).toHaveLength(1)
    await settle()
    expect(sent('preview')).toHaveLength(0)

    await fireEvent.click(screen.getByRole('button', { name: 'Scan' }))
    expect(await screen.findByText('Scanning your folders…')).toBeInTheDocument()
    expect(screen.queryByText(prompt)).not.toBeInTheDocument()

    scan.resolve(volumes())
    await screen.findByText('Macintosh HD')
    expect(sent('preview')).toHaveLength(1)
    expect(screen.queryByText(prompt)).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Rescan' })).toBeInTheDocument()
  })

  it('ticks every item the ignore list does not protect', async () => {
    backend()
    await renderScanned()

    expect(box('shop/web')).toBeChecked()
    expect(box('api/core')).toBeChecked()
    expect(box('old-branch')).toBeChecked()
    expect(box('blog/site')).not.toBeChecked()
    expect(box('blog/site')).toBeDisabled()
    expect(screen.getByText('ignored, never cleaned')).toBeInTheDocument()
    expect(screen.getByText('3 selected, 3.5 GB')).toBeInTheDocument()
  })

  it('shows the scan error', async () => {
    backend({
      preview: () => {
        throw 'Could not read config.toml'
      },
    })
    render(Preview)
    await fireEvent.click(screen.getByRole('button', { name: 'Scan' }))
    expect(await screen.findByText('Could not read config.toml')).toBeInTheDocument()
    // Nothing was found because nothing was read, so neither empty-state line is true.
    expect(screen.queryByText(/No mounted volumes/)).not.toBeInTheDocument()
    expect(screen.queryByText(/Press Scan/)).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Scan' })).toBeInTheDocument()
  })

  it('cleans only the ticked items, then scans again', async () => {
    const result: RunResult = {
      name: 'Macintosh HD',
      freed_text: '2.5 GB',
      removed: [],
      failed: [],
      skipped: [],
      stopped: null,
      error: null,
    }
    const { sent } = backend({ run_selected: () => [result] })
    await renderScanned()

    await fireEvent.click(box('api/core'))
    await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))

    expect(await screen.findByText('Macintosh HD: freed 2.5 GB')).toBeInTheDocument()
    expect(sent('run_selected')).toEqual([
      { selections: [{ mount: '/', paths: [SHOP, WORKTREE] }] },
    ])
    await waitFor(() => expect(sent('preview')).toHaveLength(2))
  })

  it('shows how much Picard has freed in total', async () => {
    backend({
      get_totals: () => ({ bytes: 42.3e9, items: 318, since: '2026-09-03T08:30:00Z' }),
    })
    render(Preview)

    expect(await findTotal('Picard has freed 42 GB across 318 items')).toBeInTheDocument()
  })

  it('hides the total until something has been removed', async () => {
    const { sent } = backend()
    render(Preview)
    await waitFor(() => expect(sent('get_totals')).toHaveLength(1))
    await settle()

    expect(screen.queryByText(/Picard has freed/)).not.toBeInTheDocument()
  })

  it('hides the total when it cannot be read', async () => {
    const { sent } = backend({
      get_totals: () => {
        throw 'history.jsonl is unreadable'
      },
    })
    render(Preview)
    await waitFor(() => expect(sent('get_totals')).toHaveLength(1))
    await settle()

    expect(screen.queryByText(/Picard has freed/)).not.toBeInTheDocument()
    expect(screen.queryByText('history.jsonl is unreadable')).not.toBeInTheDocument()
  })

  it('updates the total after a clean', async () => {
    let totals: Totals = { bytes: 1e9, items: 1, since: '2026-09-03T08:30:00Z' }
    backend({
      get_totals: () => totals,
      run_selected: () => {
        totals = { bytes: 4.5e9, items: 4, since: '2026-09-03T08:30:00Z' }
        return []
      },
    })
    await renderScanned()
    expect(await findTotal('Picard has freed 1.0 GB across 1 item')).toBeInTheDocument()

    await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))

    expect(await findTotal('Picard has freed 4.5 GB across 4 items')).toBeInTheDocument()
  })

  it('reloads the total and recent list when a background clean writes history, without rescanning', async () => {
    let totals: Totals = { bytes: 0, items: 0, since: null }
    let history: HistoryEntry[] = []
    const { sent } = backend({ get_totals: () => totals, get_history: () => history })
    render(Preview)
    await waitFor(() => expect(sent('get_totals')).toHaveLength(1))
    expect(screen.queryByText('Recently removed')).not.toBeInTheDocument()

    // What auto mode, the daily scan or the CLI leaves behind.
    totals = { bytes: 2e9, items: 1, since: '2026-09-03T08:30:00Z' }
    history = [
      { time: '2026-09-03T08:30:00Z', task: 'node_modules', volume: 'Macintosh HD', path: API, bytes: 2e9 },
    ]
    await emit('history-changed')

    expect(await findTotal('Picard has freed 2.0 GB across 1 item')).toBeInTheDocument()
    expect(screen.getByText('Recently removed')).toBeInTheDocument()
    expect(sent('get_history')).toHaveLength(2)
    expect(sent('preview')).toHaveLength(0)
  })

  it('folds events that arrive during a reload into one more reload', async () => {
    const replies: ReturnType<typeof deferred<Totals>>[] = []
    const { sent } = backend({
      get_totals: () => {
        const reply = deferred<Totals>()
        replies.push(reply)
        return reply.promise
      },
    })
    render(Preview)
    await waitFor(() => expect(replies).toHaveLength(1))
    const none: Totals = { bytes: 0, items: 0, since: null }
    replies[0].resolve(none)
    await settle()

    // One append per task: three events while the first reload is still waiting.
    await emit('history-changed')
    await emit('history-changed')
    await emit('history-changed')
    await settle()
    expect(sent('get_totals')).toHaveLength(2)

    replies[1].resolve(none)
    await waitFor(() => expect(sent('get_totals')).toHaveLength(3))
    replies[2].resolve(none)
    await settle()
    expect(sent('get_totals')).toHaveLength(3)
    expect(sent('preview')).toHaveLength(0)
  })

  it('unticks only one task when its group box is unticked', async () => {
    backend()
    await renderScanned()

    await fireEvent.click(screen.getByRole('checkbox', { name: 'node_modules' }))

    expect(box('shop/web')).not.toBeChecked()
    expect(box('api/core')).not.toBeChecked()
    expect(box('old-branch')).toBeChecked()
  })

  it('unticks an item before its Ignore request goes out, and blocks Clean until it is done', async () => {
    const reply = deferred<null>()
    const { sent } = backend({
      set_ignored: () => reply.promise,
      get_protection: () => ['ignored', null, 'ignored', null],
    })
    await renderScanned()

    await fireEvent.click(screen.getByRole('button', { name: `Ignore ~/w/shop/web/node_modules` }))

    expect(box('shop/web')).not.toBeChecked()
    expect(screen.getByRole('button', { name: 'Clean selected' })).toBeDisabled()
    expect(sent('set_ignored')).toEqual([{ path: SHOP, ignored: true }])

    reply.resolve(null)
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Clean selected' })).toBeEnabled(),
    )
    expect(box('shop/web')).toBeDisabled()
    expect(sent('get_protection')).toEqual([{ paths: [SHOP, API, BLOG, WORKTREE] }])
    expect(
      screen.getByRole('button', { name: 'Stop ignoring ~/w/shop/web/node_modules' }),
    ).toBeInTheDocument()
  })

  it('shows the error when Ignore fails, and lets you clean again', async () => {
    const { sent } = backend({
      set_ignored: () => {
        throw 'config.toml is read-only'
      },
    })
    await renderScanned()

    await fireEvent.click(screen.getByRole('button', { name: 'Ignore ~/w/shop/web/node_modules' }))

    expect(await screen.findByText('config.toml is read-only')).toBeInTheDocument()
    expect(box('shop/web')).not.toBeChecked()
    expect(box('shop/web')).toBeEnabled()
    expect(screen.getByRole('button', { name: 'Ignore ~/w/shop/web/node_modules' })).toBeEnabled()
    expect(screen.getByRole('button', { name: 'Clean selected' })).toBeEnabled()
    expect(sent('get_protection')).toHaveLength(0)
  })

  it('keeps the newest protection reply when an older one arrives last', async () => {
    const replies = [deferred<Protection[]>(), deferred<Protection[]>()]
    let asked = 0
    backend({ get_protection: () => replies[asked++].promise })
    await renderScanned()

    await fireEvent.click(screen.getByRole('button', { name: 'Ignore ~/w/shop/web/node_modules' }))
    await fireEvent.click(screen.getByRole('button', { name: 'Ignore ~/w/api/core/node_modules' }))
    await waitFor(() => expect(asked).toBe(2))

    // The second refresh read the config after both writes.
    replies[1].resolve(['ignored', 'ignored', 'ignored', null])
    await waitFor(() => expect(box('api/core')).toBeDisabled())

    // The first refresh read it between them, so it does not know about API.
    replies[0].resolve(['ignored', null, 'ignored', null])
    await settle()
    expect(box('api/core')).toBeDisabled()
    expect(box('shop/web')).toBeDisabled()
  })

  it('refreshes protection when the Settings tab changes the ignore list', async () => {
    const { sent } = backend({ get_protection: () => [null, null, null, null] })
    await renderScanned()

    notifyIgnoreChanged({ path: '~/w/blog/site/node_modules', ignored: false, source: 'settings' })

    await waitFor(() => expect(box('blog/site')).toBeEnabled())
    // Freed, but left unticked until you tick it.
    expect(box('blog/site')).not.toBeChecked()
    expect(sent('get_protection')).toHaveLength(1)
  })

  it('ignores its own ignore-change events', async () => {
    const { sent } = backend()
    await renderScanned()

    notifyIgnoreChanged({ path: SHOP, ignored: true, source: 'clean' })
    await settle()

    expect(sent('get_protection')).toHaveLength(0)
  })

  it('shows an error when the protection reply does not match the rows', async () => {
    backend({ get_protection: () => [null] })
    await renderScanned()

    notifyIgnoreChanged({ path: '~/w', ignored: false, source: 'settings' })

    expect(
      await screen.findByText('get_protection returned 1 answers for 4 rows'),
    ).toBeInTheDocument()
  })

  describe('category toggle', () => {
    const VENV = '/Users/a/w/old/.venv'

    /** The usual scan plus an empty, an all-ignored and a failed category. */
    function mixed(): VolumePreview[] {
      const [v] = volumes()
      v.tasks.push(
        { task: 'cargo_clean', error: null, items: [] },
        { task: 'python_venv', error: null, items: [item(VENV, 1e9, { ignored: true })] },
        { task: 'build_output', error: 'permission denied', items: [] },
      )
      return [v]
    }

    const toggle = (label: string) => screen.getByRole('button', { name: `${label} list` })

    it('starts categories with something to clean open', async () => {
      backend({ preview: mixed })
      await renderScanned()

      expect(toggle('node_modules')).toHaveAttribute('aria-expanded', 'true')
      expect(toggle('Git worktrees')).toHaveAttribute('aria-expanded', 'true')
      expect(box('shop/web')).toBeVisible()
    })

    it('starts empty and all-ignored categories closed, with a summary', async () => {
      backend({ preview: mixed })
      await renderScanned()

      expect(toggle('Cargo target folders')).toHaveAttribute('aria-expanded', 'false')
      expect(toggle('Python virtualenvs')).toHaveAttribute('aria-expanded', 'false')
      expect(screen.getByText('nothing to remove')).toBeInTheDocument()
      expect(screen.getByText('all ignored')).toBeInTheDocument()
      expect(screen.queryByRole('checkbox', { name: /w\/old/ })).not.toBeInTheDocument()
    })

    it('keeps a category that failed to scan open, so its error shows', async () => {
      backend({ preview: mixed })
      await renderScanned()

      expect(toggle('Ignored build output')).toHaveAttribute('aria-expanded', 'true')
      expect(screen.getByText('Could not scan: permission denied')).toBeVisible()
    })

    it('opens and closes a category when its toggle is clicked', async () => {
      backend({ preview: mixed })
      await renderScanned()

      await fireEvent.click(toggle('Python virtualenvs'))
      expect(toggle('Python virtualenvs')).toHaveAttribute('aria-expanded', 'true')
      expect(screen.getByRole('checkbox', { name: /w\/old/ })).toBeDisabled()
      expect(screen.queryByText('all ignored')).not.toBeInTheDocument()

      await fireEvent.click(toggle('node_modules'))
      expect(toggle('node_modules')).toHaveAttribute('aria-expanded', 'false')
      expect(screen.getByText('2 items, 1 ignored')).toBeInTheDocument()
      expect(screen.queryByRole('checkbox', { name: /shop\/web/ })).not.toBeInTheDocument()
    })

    it('says a closed category failed to scan', async () => {
      backend({ preview: mixed })
      await renderScanned()

      await fireEvent.click(toggle('Ignored build output'))

      expect(screen.getByText('could not scan')).toBeInTheDocument()
      expect(screen.queryByText('Could not scan: permission denied')).not.toBeVisible()
    })

    it('names the heading after the category alone', async () => {
      backend()
      await renderScanned()

      expect(screen.getByRole('heading', { level: 3, name: 'node_modules' })).toBeInTheDocument()
    })

    it('keeps the open state of each volume apart', async () => {
      const [first] = volumes()
      const second: VolumePreview = {
        ...volumes()[0],
        name: 'Backup',
        mount: '/Volumes/Backup',
        tasks: [{ task: 'node_modules', error: null, items: [] }],
      }
      backend({ preview: () => [first, second] })
      await renderScanned()

      const [onFirst, onSecond] = screen.getAllByRole('button', { name: 'node_modules list' })
      expect(onFirst).toHaveAttribute('aria-expanded', 'true')
      expect(onSecond).toHaveAttribute('aria-expanded', 'false')
      expect(onFirst.getAttribute('aria-controls')).not.toBe(onSecond.getAttribute('aria-controls'))
    })

    it('still selects and cleans items in a closed category', async () => {
      const { sent } = backend()
      await renderScanned()

      await fireEvent.click(toggle('node_modules'))
      await fireEvent.click(screen.getByRole('button', { name: 'Select none' }))
      await fireEvent.click(screen.getByRole('button', { name: 'Select all' }))
      await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))

      await waitFor(() =>
        expect(sent('run_selected')).toEqual([
          { selections: [{ mount: '/', paths: [SHOP, API, WORKTREE] }] },
        ]),
      )
    })

    it('leaves a category open when Ignore protects its last item', async () => {
      backend({ get_protection: () => [null, null, 'ignored', 'ignored'] })
      await renderScanned()

      await fireEvent.click(screen.getByRole('button', { name: /^Ignore .*old-branch/ }))

      await waitFor(() => expect(box('old-branch')).toBeDisabled())
      expect(toggle('Git worktrees')).toHaveAttribute('aria-expanded', 'true')
    })

    it('closes a category on the next scan once a clean has emptied it', async () => {
      let scans = 0
      backend({
        preview: () => {
          const found = volumes()
          if (++scans > 1) found[0].tasks[1].items = []
          return found
        },
      })
      await renderScanned()
      expect(toggle('Git worktrees')).toHaveAttribute('aria-expanded', 'true')

      await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))

      await waitFor(() =>
        expect(toggle('Git worktrees')).toHaveAttribute('aria-expanded', 'false'),
      )
    })
  })
})
