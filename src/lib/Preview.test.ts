import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/svelte'
// The Tauri mock does not deliver `emitTo('main', …)` to a window's listener,
// so these tests send the global event. The Rust side targets the window.
import { emit } from '@tauri-apps/api/event'
import { mockWindows } from '@tauri-apps/api/mocks'
import { afterEach, describe, expect, it, vi } from 'vitest'
import Preview from './Preview.svelte'
import {
  notifyIgnoreChanged,
  type HistoryEntry,
  type PreviewItem,
  type Protection,
  type RunResult,
  type ScanPhase,
  type Status,
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
      scanned_at: '2026-10-22T09:00:00Z',
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

/**
 * One volume with 112 GB of 994 GB free. Named apart from the scan's volume so
 * `findByText('Macintosh HD')` still finds only the scan.
 */
function status(volumes: Partial<Status['volumes'][number]>[] = [{}]): Status {
  return {
    mode: 'ask',
    unavailable: [],
    cleaning: false,
    volumes: volumes.map((v) => ({
      name: 'Startup disk',
      mount: '/',
      free_text: '112 GB free (11%)',
      free_bytes: 112e9,
      total_bytes: 994e9,
      low: false,
      ...v,
    })),
  }
}

/** The scan above, with replies that change nothing. `handlers` override them per command. */
function backend(handlers: Record<string, Handler> = {}) {
  // The page listens for `history-changed` on its own window.
  mockWindows('main')
  return fakeBackend({
    preview: () => volumes(),
    get_last_preview: () => [],
    get_history: () => [],
    get_totals: () => ({ bytes: 0, items: 0, since: null }) satisfies Totals,
    get_status: () => status(),
    set_ignored: () => null,
    get_protection: () => [null, null, 'ignored', null] satisfies Protection[],
    run_selected: () => [],
    get_scan_state: () => ({ state: 'idle' }) satisfies ScanPhase,
    cancel_scan: () => null,
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

/** The value and caption of the stat tile labelled `label`, with whitespace collapsed. */
function tile(label: string): string[] {
  const stat = screen.getByText(label).parentElement!
  return within(stat)
    .getAllByRole('definition')
    .map((dd) => dd.textContent!.replace(/\s+/g, ' ').trim())
}

/** Waits for the freed tile to show `value` and `items` removed since 3 Sep 2026. */
async function findFreed(value: string, items: string) {
  const since = new Date('2026-09-03T08:30:00Z').toLocaleDateString(undefined, {
    day: 'numeric',
    month: 'short',
    year: 'numeric',
  })
  await waitFor(() => expect(tile('Freed by Picard')).toEqual([value, `${items} since ${since}`]))
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

    await findFreed('42 GB', '318 items')
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

    await findFreed('42 GB', '318 items')
  })

  it('says nothing has been removed before the first removal', async () => {
    backend()
    render(Preview)

    await waitFor(() => expect(tile('Freed by Picard')).toEqual(['0 MB', 'Nothing removed yet']))
  })

  it('shows disk size and available space in one row per volume', async () => {
    backend({
      get_status: () =>
        status([
          {},
          { name: 'Projects', mount: '/Volumes/Projects', free_bytes: 8e9, total_bytes: 500e9, low: true },
        ]),
    })
    render(Preview)

    await waitFor(() => expect(screen.getAllByText('Disk size')).toHaveLength(2))
    const sizes = screen.getAllByText('Disk size').map((dt) => dt.parentElement!)
    const available = screen.getAllByText('Available').map((dt) => dt.parentElement!)
    const text = (el: HTMLElement) =>
      within(el)
        .getAllByRole('definition')
        .map((dd) => dd.textContent!.replace(/\s+/g, ' ').trim())
    expect(sizes.map(text)).toEqual([
      ['994 GB', 'Startup disk'],
      ['500 GB', 'Projects'],
    ])
    expect(available.map(text)).toEqual([
      ['112 GB', '11% free'],
      ['8.0 GB', '2% free, below your threshold'],
    ])
    expect(available[1]).toHaveClass('low')
    // The freed tile covers every volume, so it spans both rows.
    expect(screen.getByText('Freed by Picard').parentElement!.getAttribute('style')).toContain(
      'grid-row: 1 / span 2',
    )
  })

  it('leaves out a volume that reports no capacity from the percentage', async () => {
    backend({ get_status: () => status([{ free_bytes: 0, total_bytes: 0 }]) })
    render(Preview)

    await waitFor(() => expect(tile('Available')).toEqual(['0 MB', '']))
  })

  it('hides the disk tiles when free space cannot be read, but keeps the total', async () => {
    const { sent } = backend({
      get_status: () => {
        throw 'statfs failed'
      },
      get_totals: () => ({ bytes: 42.3e9, items: 318, since: '2026-09-03T08:30:00Z' }),
    })
    render(Preview)
    await findFreed('42 GB', '318 items')

    expect(sent('get_status')).toHaveLength(1)
    expect(screen.queryByText('Disk size')).not.toBeInTheDocument()
    expect(screen.queryByText('statfs failed')).not.toBeInTheDocument()
    // Alone, the freed tile takes the whole strip rather than the third column.
    expect(screen.getByText('Freed by Picard').parentElement).toHaveClass('solo')
  })

  it('hides disk tiles that loaded earlier when a later read fails', async () => {
    let fail = false
    const { sent } = backend({
      get_status: () => {
        if (fail) throw 'statfs failed'
        return status()
      },
    })
    render(Preview)
    await waitFor(() => expect(tile('Available')).toEqual(['112 GB', '11% free']))
    expect(screen.getByText('Freed by Picard').parentElement).not.toHaveClass('solo')

    fail = true
    await emit('history-changed')
    await waitFor(() => expect(sent('get_status')).toHaveLength(2))
    await settle()

    expect(screen.queryByText('Disk size')).not.toBeInTheDocument()
  })

  it('names the summary for screen readers', async () => {
    backend()
    render(Preview)

    expect(await screen.findByRole('region', { name: 'Disk summary' })).toBeInTheDocument()
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

    expect(screen.queryByText('Freed by Picard')).not.toBeInTheDocument()
    expect(screen.getByText('Disk size')).toBeInTheDocument()
    expect(screen.queryByText('history.jsonl is unreadable')).not.toBeInTheDocument()
  })

  it('updates the total and available space after a clean', async () => {
    let totals: Totals = { bytes: 1e9, items: 1, since: '2026-09-03T08:30:00Z' }
    let free = 112e9
    backend({
      get_totals: () => totals,
      get_status: () => status([{ free_bytes: free }]),
      run_selected: () => {
        totals = { bytes: 4.5e9, items: 4, since: '2026-09-03T08:30:00Z' }
        free = 115.5e9
        return []
      },
    })
    await renderScanned()
    await findFreed('1.0 GB', '1 item')
    expect(tile('Available')).toEqual(['112 GB', '11% free'])

    await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))

    await findFreed('4.5 GB', '4 items')
    await waitFor(() => expect(tile('Available')).toEqual(['116 GB', '12% free']))
  })

  it('opens on the latest stored scan, such as the one behind a notification, without scanning', async () => {
    const { sent } = backend({ get_last_preview: () => volumes() })
    render(Preview)

    expect(await screen.findByText('Macintosh HD')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Rescan' })).toBeInTheDocument()
    expect(box('shop/web')).toBeChecked()
    expect(box('blog/site')).not.toBeChecked()
    // The ignore list may have changed since the stored scan.
    await waitFor(() => expect(sent('get_protection')).toHaveLength(1))
    await settle()
    expect(sent('preview')).toHaveLength(0)
  })

  it('shows the Scan prompt when the stored scan cannot be read', async () => {
    const logged = vi.spyOn(console, 'error').mockImplementation(() => {})
    backend({
      get_last_preview: () => {
        throw 'stored scan unavailable'
      },
    })
    render(Preview)

    await waitFor(() => expect(logged).toHaveBeenCalled())
    expect(
      screen.getByText('Press Scan to find build folders, caches and stale worktrees.'),
    ).toBeInTheDocument()
    expect(screen.queryByText('stored scan unavailable')).not.toBeInTheDocument()
    logged.mockRestore()
  })

  it('offers a newer background scan without touching the ticks, and shows it on request', async () => {
    let stored: VolumePreview[] = []
    const { sent } = backend({ get_last_preview: () => stored })
    await renderScanned()
    await fireEvent.click(box('shop/web'))
    expect(box('shop/web')).not.toBeChecked()

    // The scan already on the page: nothing to offer.
    stored = volumes()
    await emit('preview-changed')
    await settle()
    expect(screen.queryByRole('button', { name: 'Show it' })).not.toBeInTheDocument()

    const later = volumes()
    later[0].scanned_at = '2026-10-22T10:00:00Z'
    later[0].tasks[0].items.push(item('/Users/a/w/docs/node_modules', 3e9))
    stored = later
    await emit('preview-changed')

    const showIt = await screen.findByRole('button', { name: 'Show it' })
    expect(box('shop/web')).not.toBeChecked()
    expect(screen.queryByRole('checkbox', { name: /docs/ })).not.toBeInTheDocument()

    // The store moves on before the click; the page shows what it holds then.
    const latest = volumes()
    latest[0].scanned_at = '2026-10-22T11:00:00Z'
    latest[0].tasks[0].items.push(item('/Users/a/w/wiki/node_modules', 1e9))
    stored = latest
    await fireEvent.click(showIt)
    expect(await screen.findByRole('checkbox', { name: /wiki/ })).toBeChecked()
    expect(screen.queryByRole('checkbox', { name: /docs/ })).not.toBeInTheDocument()
    expect(box('shop/web')).toBeChecked()
    expect(screen.queryByRole('button', { name: 'Show it' })).not.toBeInTheDocument()
    expect(sent('preview')).toHaveLength(1)
  })

  it('reads the store again after a scan when a background scan landed during it', async () => {
    const scan = deferred<VolumePreview[]>()
    let stored: VolumePreview[] = []
    backend({ preview: () => scan.promise, get_last_preview: () => stored })
    render(Preview)
    await fireEvent.click(screen.getByRole('button', { name: 'Scan' }))
    await screen.findByText('Scanning your folders…')

    // An estimate that started after the manual scan finishes first.
    const later = volumes()
    later[0].scanned_at = '2026-10-22T10:00:00Z'
    stored = later
    await emit('preview-changed')
    await settle()

    scan.resolve(volumes())
    expect(await screen.findByRole('button', { name: 'Show it' })).toBeInTheDocument()
  })

  it('says the list may be out of date after a background clean of a volume on the page', async () => {
    let stored: VolumePreview[] = []
    backend({ get_last_preview: () => stored })
    await renderScanned()

    // What the store holds after auto mode cleans the only volume.
    stored = []
    await emit('preview-changed')

    expect(
      await screen.findByText('Picard cleaned in the background, so this list may be out of date.'),
    ).toBeInTheDocument()
    expect(box('shop/web')).toBeChecked()
  })

  describe('scans the page did not start', () => {
    const since = (secondsAgo: number) => new Date(Date.now() - secondsAgo * 1000).toISOString()

    it('shows a background scan like its own, then its result', async () => {
      let stored: VolumePreview[] = []
      const { sent } = backend({ get_last_preview: () => stored })
      render(Preview)
      await settle()

      await emit('scan-changed', { state: 'scanning', since: since(0), ticket: null })
      expect(await screen.findByText('Scanning your folders…')).toBeInTheDocument()
      expect(screen.getByRole('button', { name: 'Scan' })).toBeDisabled()

      stored = volumes()
      await emit('preview-changed')
      await emit('scan-changed', { state: 'idle' })

      expect(await screen.findByText('Macintosh HD')).toBeInTheDocument()
      expect(box('shop/web')).toBeChecked()
      expect(screen.queryByText('Scanning your folders…')).not.toBeInTheDocument()
      expect(screen.queryByRole('button', { name: 'Show it' })).not.toBeInTheDocument()
      expect(sent('preview')).toHaveLength(0)
    })

    it('shows a scan already running when the window opens, timed from its start', async () => {
      backend({ get_scan_state: () => ({ state: 'scanning', since: since(7), ticket: null }) })
      render(Preview)

      expect(await screen.findByText('Scanning your folders…')).toBeInTheDocument()
      expect(screen.getByText(/^[78] s$/)).toBeInTheDocument()
    })

    it('says when a scan waits for a clean to end', async () => {
      backend()
      render(Preview)
      await settle()

      await emit('scan-changed', { state: 'waiting_for_clean', since: since(0), ticket: null })
      expect(await screen.findByText('Waiting for Picard to finish cleaning…')).toBeInTheDocument()
      await emit('scan-changed', { state: 'scanning', since: since(0), ticket: null })
      expect(await screen.findByText('Scanning your folders…')).toBeInTheDocument()
    })

    it('keeps the list when a background scan is cancelled', async () => {
      let stored: VolumePreview[] = []
      const { sent } = backend({ get_last_preview: () => stored })
      await renderScanned()
      stored = volumes()
      await fireEvent.click(box('shop/web'))

      await emit('scan-changed', { state: 'scanning', since: since(0), ticket: null })
      await fireEvent.click(await screen.findByRole('button', { name: 'Cancel scan' }))
      expect(sent('cancel_scan')).toHaveLength(1)
      await emit('scan-changed', { state: 'idle' })

      expect(await screen.findByText('Macintosh HD')).toBeInTheDocument()
      expect(box('shop/web')).not.toBeChecked()
    })
  })

  it('drops a phase read that an event overtook, so a finished scan does not stick', async () => {
    const read = deferred<ScanPhase>()
    backend({ get_scan_state: () => read.promise })
    render(Preview)
    await settle()

    // The scan ends while the page's first read of the phase is in flight.
    await emit('scan-changed', { state: 'idle' })
    read.resolve({ state: 'scanning', since: new Date().toISOString(), ticket: null })
    await settle()

    expect(screen.queryByText('Scanning your folders…')).not.toBeInTheDocument()
    expect(
      screen.getByText('Press Scan to find build folders, caches and stale worktrees.'),
    ).toBeInTheDocument()
  })

  it('shows a background scan queued right behind its own scan like its own', async () => {
    const own = deferred<VolumePreview[]>()
    let stored: VolumePreview[] = []
    const { sent } = backend({ preview: () => own.promise, get_last_preview: () => stored })
    render(Preview)
    await fireEvent.click(screen.getByRole('button', { name: 'Scan' }))
    const ticket = sent('preview')[0].ticket as number
    const now = new Date().toISOString()
    await emit('scan-changed', { state: 'scanning', since: now, ticket })

    // The estimate starts before the page has handled its own reply.
    await emit('scan-changed', { state: 'scanning', since: now, ticket: null })
    stored = volumes()
    own.resolve(volumes())
    await settle()
    // The queued scan is running, so the panel stays up, as for any scan.
    expect(screen.getByText('Scanning your folders…')).toBeInTheDocument()

    const later = volumes()
    later[0].scanned_at = '2026-10-22T10:00:00Z'
    later[0].tasks[0].items.push(item('/Users/a/w/docs/node_modules', 3e9))
    stored = later
    await emit('preview-changed')
    await emit('scan-changed', { state: 'idle' })

    expect(await screen.findByRole('checkbox', { name: /docs/ })).toBeChecked()
    expect(screen.queryByRole('button', { name: 'Show it' })).not.toBeInTheDocument()
  })

  it('cancels its own scan while it waits for a clean', async () => {
    const own = deferred<VolumePreview[] | null>()
    const { sent } = backend({ preview: () => own.promise })
    render(Preview)
    await fireEvent.click(screen.getByRole('button', { name: 'Scan' }))
    const ticket = sent('preview')[0].ticket as number
    await emit('scan-changed', {
      state: 'waiting_for_clean',
      since: new Date().toISOString(),
      ticket,
    })
    expect(await screen.findByText('Waiting for Picard to finish cleaning…')).toBeInTheDocument()

    await fireEvent.click(screen.getByRole('button', { name: 'Cancel scan' }))
    expect(sent('cancel_scan')).toHaveLength(1)
    own.resolve(null)

    expect(
      await screen.findByText('Press Scan to find build folders, caches and stale worktrees.'),
    ).toBeInTheDocument()
    expect(screen.queryByText('Waiting for Picard to finish cleaning…')).not.toBeInTheDocument()
    expect(document.querySelector('.error')).toBeNull()
  })

  it('reads the phase again after Cancel, so a missed idle event cannot leave the panel up', async () => {
    let phase: ScanPhase = { state: 'idle' }
    const { sent } = backend({ get_scan_state: () => phase })
    render(Preview)
    await settle()
    await emit('scan-changed', { state: 'scanning', since: new Date().toISOString(), ticket: null })
    const cancel = await screen.findByRole('button', { name: 'Cancel scan' })

    // The backend is already idle, and its idle event never arrived.
    phase = { state: 'idle' }
    await fireEvent.click(cancel)

    await waitFor(() =>
      expect(screen.queryByText('Scanning your folders…')).not.toBeInTheDocument(),
    )
    expect(sent('get_scan_state').length).toBeGreaterThanOrEqual(2)
  })

  it('does not call its own clean a background one once it ends', async () => {
    const run = deferred<RunResult[]>()
    backend({ run_selected: () => run.promise })
    await renderScanned()
    const note = 'Picard is cleaning in the background. You can clean again when it finishes.'

    await fireEvent.click(screen.getByRole('button', { name: 'Clean selected' }))
    await emit('cleaning-changed', true)
    // The run ends before its `cleaning-changed` false arrives.
    run.resolve([])
    await waitFor(() => expect(screen.getByRole('button', { name: 'Clean selected' })).toBeEnabled())
    expect(screen.queryByText(note)).not.toBeInTheDocument()
  })

  it('keeps the list and shows no error when its own scan is cancelled', async () => {
    const rescan = deferred<VolumePreview[] | null>()
    let calls = 0
    const { sent } = backend({ preview: () => (calls++ === 0 ? volumes() : rescan.promise) })
    await renderScanned()
    await fireEvent.click(box('shop/web'))

    await fireEvent.click(screen.getByRole('button', { name: 'Rescan' }))
    await fireEvent.click(await screen.findByRole('button', { name: 'Cancel scan' }))
    expect(sent('cancel_scan')).toHaveLength(1)
    rescan.resolve(null)

    expect(await screen.findByText('Macintosh HD')).toBeInTheDocument()
    expect(box('shop/web')).not.toBeChecked()
    expect(screen.queryByRole('button', { name: 'Cancel scan' })).not.toBeInTheDocument()
    expect(document.querySelector('.error')).toBeNull()
  })

  it('holds Clean while Picard cleans in the background, and says so', async () => {
    backend()
    await renderScanned()
    const clean = screen.getByRole('button', { name: 'Clean selected' })
    const note = 'Picard is cleaning in the background. You can clean again when it finishes.'

    await emit('cleaning-changed', true)
    expect(await screen.findByText(note)).toBeInTheDocument()
    expect(clean).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Rescan' })).toBeEnabled()

    await emit('cleaning-changed', false)
    await waitFor(() => expect(clean).toBeEnabled())
    expect(screen.queryByText(note)).not.toBeInTheDocument()
  })

  it('says a background clean is running when the window opens during one', async () => {
    backend({ get_status: () => ({ ...status(), cleaning: true }) })
    render(Preview)
    expect(
      await screen.findByText(
        'Picard is cleaning in the background. You can clean again when it finishes.',
      ),
    ).toBeInTheDocument()
  })

  it('keeps Clean disabled until a stored scan is checked against the ignore list', async () => {
    const protection = deferred<Protection[]>()
    backend({ get_last_preview: () => volumes(), get_protection: () => protection.promise })
    render(Preview)
    await screen.findByText('Macintosh HD')

    const clean = screen.getByRole('button', { name: 'Clean selected' })
    expect(clean).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Rescan' })).toBeDisabled()

    // SHOP was ignored after the stored scan ran.
    protection.resolve(['ignored', null, 'ignored', null])
    await waitFor(() => expect(clean).toBeEnabled())
    expect(box('shop/web')).not.toBeChecked()
  })

  it('shows only the time for a scan today and adds the date for an older one', async () => {
    const today = volumes()
    today[0].scanned_at = new Date().toISOString()
    const older = volumes()
    older[0].mount = '/Volumes/Work'
    older[0].name = 'Work'
    older[0].scanned_at = '2026-01-05T09:00:00Z'
    backend({ get_last_preview: () => [...today, ...older] })
    render(Preview)
    await screen.findByText('Work')

    const clock = (d: Date) => d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' })
    const now = new Date(today[0].scanned_at)
    const then = new Date(older[0].scanned_at)
    const day = then.toLocaleDateString(undefined, { day: 'numeric', month: 'short' })
    expect(screen.getByText(`· scanned ${clock(now)}`)).toBeInTheDocument()
    expect(screen.getByText(`· scanned ${day}, ${clock(then)}`)).toBeInTheDocument()
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

    await findFreed('2.0 GB', '1 item')
    expect(screen.getByText('Recently removed')).toBeInTheDocument()
    expect(sent('get_history')).toHaveLength(2)
    expect(sent('get_status')).toHaveLength(2)
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
