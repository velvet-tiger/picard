import { fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import Preview from './Preview.svelte'
import {
  notifyIgnoreChanged,
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

async function renderScanned() {
  render(Preview)
  await screen.findByText('Macintosh HD')
}

describe('Preview', () => {
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
    expect(await screen.findByText('Could not read config.toml')).toBeInTheDocument()
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
    await renderScanned()

    const since = new Date('2026-09-03T08:30:00Z').toLocaleDateString(undefined, {
      day: 'numeric',
      month: 'short',
      year: 'numeric',
    })
    expect(
      await screen.findByText(
        (_, el) =>
          el?.tagName === 'P' &&
          el.textContent?.replace(/\s+/g, ' ').trim() ===
            `Picard has freed 42 GB in total, 318 items since ${since}.`,
      ),
    ).toBeInTheDocument()
  })

  it('hides the total until something has been removed', async () => {
    const { sent } = backend()
    await renderScanned()
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
    await renderScanned()
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
    expect(await screen.findByText('1.0 GB')).toBeInTheDocument()
    expect(screen.getByText(/1\s+item\s+since/)).toBeInTheDocument()

    await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))

    expect(await screen.findByText('4.5 GB')).toBeInTheDocument()
    expect(screen.getByText(/4\s+items\s+since/)).toBeInTheDocument()
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
})
