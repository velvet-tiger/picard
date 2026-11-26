<script lang="ts">
  import {
    preview,
    getLastPreview,
    onPreviewChanged,
    runSelected,
    getHistory,
    getTotals,
    getStatus,
    setIgnored,
    revealNotice,
    deleteNotice,
    getProtection,
    getScanState,
    cancelScan,
    onScanChanged,
    onCleaningChanged,
    notifyIgnoreChanged,
    onIgnoreChanged,
    onHistoryChanged,
    onStatusChanged,
    errorText,
    formatBytes,
    headroom,
    afterCleanText,
    itemTitle,
    shortPath,
    tildePath,
    TASK_LABELS,
    IGNORABLE_TASKS,
    type VolumePreview,
    type PreviewTask,
    type PreviewItem,
    type RunResult,
    type HistoryEntry,
    type Totals,
    type Status,
    type VolumeStatus,
    type ScanPhase,
  } from './api'

  let volumes = $state<VolumePreview[]>([])
  // Keyed by "mount\u0000path" so the same path on two volumes cannot collide.
  let checked = $state<Record<string, boolean>>({})
  let history = $state<HistoryEntry[]>([])
  let totals = $state<Totals | null>(null)
  let status = $state<Status | null>(null)
  // The ticket sent with the page's own Scan, from the click until its reply,
  // and when it was clicked. The backend puts the ticket in that scan's phase,
  // so the page can tell its own scan from one it should follow.
  let myTicket = $state<number | null>(null)
  let lastTicket = Math.floor(Math.random() * 1e9)
  const asked = $derived(myTicket !== null)
  let askedAt = $state(0)
  // The shared scan as the backend reports it, whoever started it: a click
  // here, a notification's estimate, or the rescan after a background clean.
  // The page shows them all the same way.
  let phase = $state<ScanPhase>({ state: 'idle' })
  const scanning = $derived(asked || phase.state !== 'idle')
  const waitingForClean = $derived(phase.state === 'waiting_for_clean')
  /** When the shown phase began, in ms since the epoch. */
  const scanSince = $derived(
    phase.state === 'idle' ? askedAt : Date.parse(phase.since) || askedAt,
  )
  // Set while the page follows a scan it did not start, so the result is
  // shown when it ends, as the page's own would be.
  let following = false
  // Counts `scan-changed` events, so a phase read that was in flight when one
  // arrived does not overwrite it with an older answer.
  let scanEvents = 0
  // A scan can take half a minute, so the page does not start one itself. It
  // shows the latest stored scan, such as the one behind a notification, or
  // waits for a click.
  let scanned = $state(false)
  let elapsed = $state(0)

  // Counts seconds while a scan runs or waits. The scan reports no progress of
  // its own, so elapsed time is the honest thing to show. It counts from when
  // the backend started, so a window opened mid-scan shows the real time.
  $effect(() => {
    if (!scanning) return
    const since = scanSince
    const tick = () => (elapsed = Math.max(0, Math.floor((Date.now() - since) / 1000)))
    tick()
    const timer = setInterval(tick, 250)
    return () => clearInterval(timer)
  })
  let cleaning = $state(false)
  // A cleanup running anywhere, this window's included, as last reported.
  let cleaningAnywhere = $state(false)
  // Counts `cleaning-changed` events, so a status read that was in flight
  // when one arrived does not overwrite it with an older answer.
  let cleaningEvents = 0

  function cleaningChanged(now: boolean) {
    cleaningEvents++
    cleaningAnywhere = now
  }

  /** Reads whether a cleanup runs anywhere, unless an event says so first. */
  async function readCleaning() {
    const seen = cleaningEvents
    try {
      const now = await getStatus()
      if (seen === cleaningEvents) cleaningAnywhere = now.cleaning
    } catch (e) {
      console.error('status', e)
    }
  }
  let error = $state<string | null>(null)
  let results = $state<RunResult[] | null>(null)

  const key = (mount: string, path: string) => `${mount}\u0000${path}`

  // Paths with an Ignore, Unignore, Finder or Delete request still in flight,
  // keyed like `checked`.
  let toggling = $state<Record<string, boolean>>({})
  // Cleaning or rescanning mid-toggle could act on, or show, the list as it was.
  const busyToggling = $derived(Object.values(toggling).some(Boolean))

  /** Never removed: ignored, or holding an ignored folder (see IgnoreList in tasks/mod.rs). */
  const isProtected = (i: PreviewItem) => i.ignored || i.holds_ignored

  // Rows left out of this list's clean only, keyed like `checked`. Nothing is
  // written to the config, and a new list clears them.
  let skipped = $state<Record<string, boolean>>({})

  const isSkipped = (mount: string, i: PreviewItem) => skipped[key(mount, i.path)] === true

  /** Left out of Clean, Select all and the totals: protected, or skipped for now. */
  const isExcluded = (mount: string, i: PreviewItem) => isProtected(i) || isSkipped(mount, i)

  /** Items that can be ticked: everything not excluded. */
  const selectable = (mount: string, task: PreviewTask) =>
    task.items.filter((i) => !isExcluded(mount, i))

  /** What a volume could free now. Recomputed here so it follows Ignore and Skip clicks. */
  const volumeTotal = (v: VolumePreview) =>
    v.tasks.flatMap((t) => selectable(v.mount, t)).reduce((sum, i) => sum + i.bytes, 0)

  // Whether each category's list is shown, keyed by key(mount, task).
  let open = $state<Record<string, boolean>>({})

  /**
   * A category starts closed when it holds nothing Clean could remove. One that
   * failed to scan stays open so its error shows.
   */
  const startsOpen = (mount: string, task: PreviewTask) =>
    task.error !== null || selectable(mount, task).length > 0

  /** What a closed category's heading says about the hidden list. */
  function closedSummary(mount: string, task: PreviewTask): string {
    if (task.error) return 'could not scan'
    if (task.items.length === 0) return 'nothing to remove'
    const free = selectable(mount, task).length
    // A row both ignored and skipped counts as ignored, the stronger of the two.
    const ignored = task.items.filter(isProtected).length
    const skips = task.items.length - free - ignored
    if (free === 0) {
      if (skips === 0) return 'all ignored'
      return ignored === 0 ? 'all skipped' : `${ignored} ignored, ${skips} skipped`
    }
    const parts = [`${free} ${free === 1 ? 'item' : 'items'}`]
    if (ignored > 0) parts.push(`${ignored} ignored`)
    if (skips > 0) parts.push(`${skips} skipped`)
    return parts.join(', ')
  }

  const selectedBytes = $derived(
    volumes
      .flatMap((v) =>
        v.tasks.flatMap((t) => t.items.filter((i) => checked[key(v.mount, i.path)])),
      )
      .reduce((sum, i) => sum + i.bytes, 0),
  )
  const selectedCount = $derived(Object.values(checked).filter(Boolean).length)

  /**
   * A store change the page has not shown: a newer scan, or a background clean
   * of a volume on the page. The page keeps its list, and the ticks you
   * changed, until you choose to update it.
   */
  type Stale = { kind: 'newer'; found: VolumePreview[] } | { kind: 'cleaned' }
  let stale = $state<Stale | null>(null)
  // True while a stored scan is re-checked against the ignore list. Its rows
  // were ticked from the ignore list at scan time, so Clean waits.
  let checkingStored = $state(false)
  // The store changed while the page was busy; it is read again once idle.
  let storedPending = false

  /** Puts a scan's results on the page, with its ticks and open categories reset. */
  function show(found: VolumePreview[]) {
    stale = null
    volumes = found
    confirmingDelete = null
    // A skip lasts for one list; a fresh scan offers every row again.
    skipped = {}
    // Everything not ignored starts ticked; one click on "Clean selected" removes it all.
    checked = Object.fromEntries(
      found.flatMap((v) =>
        v.tasks.flatMap((t) => t.items.map((i) => [key(v.mount, i.path), !isProtected(i)])),
      ),
    )
    // Set on every scan only. An Ignore click that empties a category leaves
    // it open, so the list does not vanish from under the pointer.
    open = Object.fromEntries(
      found.flatMap((v) => v.tasks.map((t) => [key(v.mount, t.task), startsOpen(v.mount, t)])),
    )
    scanned = true
  }

  async function scan() {
    myTicket = ++lastTicket
    askedAt = Date.now()
    error = null
    try {
      // Null means the scan was cancelled; the list stays as it was.
      const found = await preview(myTicket)
      if (found) show(found)
    } catch (e) {
      error = errorText(e)
    } finally {
      myTicket = null
    }
    reloadRecords()
    afterBusy()
  }

  // A cancelled scan takes up to about a second to stop, so after Cancel the
  // phase is read this many times, this far apart, until it says idle.
  const CANCEL_CHECKS = 8
  const CANCEL_CHECK_MS = 250

  /**
   * Cancels the running scan, whoever started it. Then reads the phase until
   * it says idle or an event arrives, so a missed `scan-changed` cannot leave
   * the panel up with nothing running.
   */
  async function cancel() {
    const seen = scanEvents
    try {
      await cancelScan()
    } catch (e) {
      error = errorText(e)
      return
    }
    for (let check = 0; check < CANCEL_CHECKS && seen === scanEvents; check++) {
      if (check > 0) await new Promise((r) => setTimeout(r, CANCEL_CHECK_MS))
      if ((await readScanState())?.state === 'idle') return
    }
  }

  function phaseChanged(next: ScanPhase) {
    scanEvents++
    applyPhase(next)
  }

  /**
   * Reads the phase, for a window opened mid-scan and as a backstop for a
   * missed event. Returns what it applied, or null if the read failed or an
   * event overtook it.
   */
  async function readScanState(): Promise<ScanPhase | null> {
    const seen = scanEvents
    let now: ScanPhase
    try {
      now = await getScanState()
    } catch (e) {
      console.error('scan state', e)
      return null
    }
    if (seen !== scanEvents) return null
    applyPhase(now)
    return now
  }

  /**
   * Follows the shared scan. Any scan but the page's own, even one queued
   * right behind it, is shown when it ends, as if the page had started it.
   */
  function applyPhase(next: ScanPhase) {
    phase = next
    if (next.state !== 'idle') {
      following = next.ticket === null || next.ticket !== myTicket
      return
    }
    const followed = following
    following = false
    if (followed && !asked) {
      showFollowed()
    } else {
      afterBusy()
    }
  }

  /**
   * Shows what a followed scan stored. A cancelled or failed scan stores
   * nothing newer, so the page keeps its list and only checks the store as usual.
   */
  async function showFollowed() {
    storedPending = false
    let found: VolumePreview[]
    try {
      found = await getLastPreview()
    } catch (e) {
      console.error('last preview', e)
      return
    }
    if (busy()) {
      storedPending = true
      return
    }
    if (found.length > 0 && (!scanned || newest(found) > newest(volumes))) {
      await showStored(found)
    } else {
      loadStored()
    }
  }

  /** The newest scan time in `list`, in ms since the epoch; 0 for an empty list or a bad time. */
  const newest = (list: VolumePreview[]) =>
    Math.max(0, ...list.map((v) => Date.parse(v.scanned_at) || 0))

  // A scan, a clean, an Ignore click or a stored scan's ignore check owns the list while it runs.
  const busy = () => scanning || cleaning || busyToggling || checkingStored

  /**
   * Compares the stored scans with the page. With nothing on the page yet,
   * the stored scan is shown, so a notification opens on the list it counted.
   * Otherwise a newer scan or a background clean is offered, not applied.
   */
  async function loadStored() {
    if (busy()) {
      storedPending = true
      return
    }
    storedPending = false
    let found: VolumePreview[]
    try {
      found = await getLastPreview()
    } catch (e) {
      // Like history, the stored scan is a convenience; Scan still works.
      console.error('last preview', e)
      return
    }
    if (busy()) {
      storedPending = true
      return
    }
    if (!scanned) {
      if (found.length > 0) await showStored(found)
    } else if (newest(found) > newest(volumes)) {
      stale = { kind: 'newer', found }
    } else if (volumes.some((v) => !found.some((f) => f.mount === v.mount))) {
      // Every list the page shows was stored, so a missing volume was cleaned.
      stale = { kind: 'cleaned' }
    }
  }

  /** Shows a stored scan, then re-checks it against the ignore list, which may have changed since. */
  async function showStored(found: VolumePreview[]) {
    show(found)
    checkingStored = true
    try {
      await refreshProtection()
    } catch (e) {
      error = errorText(e)
    } finally {
      checkingStored = false
    }
    afterBusy()
  }

  /**
   * Shows what the store holds now. The banner's copy can be out of date if a
   * background clean dropped a volume after the banner appeared.
   */
  async function showLatest() {
    let found: VolumePreview[]
    try {
      found = await getLastPreview()
    } catch (e) {
      error = errorText(e)
      return
    }
    await showStored(found)
  }

  /** Reads the store again if it changed while the page was busy. */
  function afterBusy() {
    if (storedPending && !busy()) loadStored()
  }

  /** "14:02" for a scan today, "21 Oct, 14:02" for an older one. */
  function scannedText(time: string): string {
    const when = new Date(time)
    const clock = when.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' })
    if (when.toDateString() === new Date().toDateString()) return clock
    const day = when.toLocaleDateString(undefined, { day: 'numeric', month: 'short' })
    return `${day}, ${clock}`
  }

  // A clean appends once per task, and FSEvents reports each append as several
  // events, each a `history-changed`. Reloads in flight absorb those events,
  // and one more reload follows them,
  // so the page ends on the newest file without a request per event.
  let reloading = false
  let reloadAgain = false

  /**
   * Reloads the stats strip and "Recently removed" list. Never throws; each
   * loader logs its own errors. Free space is read here too, so it follows a clean.
   */
  async function reloadRecords() {
    if (reloading) {
      reloadAgain = true
      return
    }
    reloading = true
    try {
      do {
        reloadAgain = false
        await Promise.all([loadHistory(), loadTotals(), loadStatus()])
      } while (reloadAgain)
    } finally {
      reloading = false
    }
  }

  async function loadHistory() {
    try {
      history = await getHistory(10)
    } catch (e) {
      // History is a convenience; a failure here should not hide the scan.
      console.error('history', e)
    }
  }

  async function loadTotals() {
    try {
      totals = await getTotals()
    } catch (e) {
      // Like history, the total is a convenience; hide it rather than show a stale one.
      totals = null
      console.error('totals', e)
    }
  }

  // Counts status reads. Reads now overlap, since the poll loop asks for one
  // every 30 seconds, so a slow reply must not replace a newer one.
  let statusCalls = 0

  async function loadStatus() {
    const call = ++statusCalls
    try {
      const seen = cleaningEvents
      const now = await getStatus()
      if (call !== statusCalls) return
      status = now
      if (seen === cleaningEvents) cleaningAnywhere = now.cleaning
    } catch (e) {
      console.error('status', e)
      // The disk tiles are a convenience; hide them rather than show stale numbers.
      if (call === statusCalls) status = null
    }
  }

  /** "11% free", or nothing when the volume reports no capacity. */
  const freePercent = (v: VolumeStatus) =>
    v.total_bytes > 0 ? `${Math.round((v.free_bytes / v.total_bytes) * 100)}% free` : ''

  /** "11% free", plus a warning when the volume is below its threshold. */
  const availableCaption = (v: VolumeStatus) =>
    [freePercent(v), v.low ? 'below your threshold' : ''].filter(Boolean).join(', ')

  const diskRows = $derived(status?.volumes ?? [])

  const sinceText = (time: string) =>
    new Date(time).toLocaleDateString(undefined, {
      day: 'numeric',
      month: 'short',
      year: 'numeric',
    })

  /** "235 items since 17 Oct 2026". */
  function freedCaption(t: Totals): string {
    const items = `${t.items} ${t.items === 1 ? 'item' : 'items'}`
    return t.since ? `${items} since ${sinceText(t.since)}` : items
  }

  /** "112 GB" as ["112", "GB"], so the unit can sit smaller beside the number. */
  function sizeParts(bytes: number): [string, string] {
    const text = formatBytes(bytes)
    const gap = text.lastIndexOf(' ')
    return [text.slice(0, gap), text.slice(gap + 1)]
  }

  /** Bytes ticked on one volume. */
  const volumeSelected = (v: VolumePreview) =>
    v.tasks
      .flatMap((t) => t.items)
      .filter((i) => checked[key(v.mount, i.path)])
      .reduce((sum, i) => sum + i.bytes, 0)

  /** Rows on one volume that Clean could remove now. */
  const volumeCount = (v: VolumePreview) =>
    v.tasks.reduce((n, t) => n + selectable(v.mount, t).length, 0)

  /** What one category could free now, following Ignore and Skip clicks. */
  const taskTotal = (mount: string, t: PreviewTask) =>
    selectable(mount, t).reduce((sum, i) => sum + i.bytes, 0)

  // The scan's list is hidden while a scan runs, so its figures go with it.
  const showList = $derived(scanned && !scanning)

  /**
   * One section per volume: its disk reading, which loads without a scan, and
   * its scan results, paired by mount point. Either can be missing, since a
   * reading can fail and a volume can go unscanned.
   */
  const sections = $derived.by(() => {
    const found = showList ? volumes : []
    const byMount = new Map(found.map((v) => [v.mount, v]))
    const fromDisks = diskRows.map((disk) => {
      const scan = byMount.get(disk.mount)
      return { mount: disk.mount, name: scan?.name ?? disk.name, disk, scan }
    })
    const scanOnly = found
      .filter((v) => !diskRows.some((d) => d.mount === v.mount))
      .map((scan) => ({ mount: scan.mount, name: scan.name, disk: undefined, scan }))
    return [...fromDisks, ...scanOnly]
  })

  async function clean() {
    cleaning = true
    error = null
    results = null
    const selections = volumes
      .map((v) => ({
        mount: v.mount,
        paths: v.tasks.flatMap((t) =>
          selectable(v.mount, t)
            .filter((i) => checked[key(v.mount, i.path)])
            .map((i) => i.path),
        ),
      }))
      .filter((s) => s.paths.length > 0)
    try {
      results = await runSelected(selections)
    } catch (e) {
      error = errorText(e)
    } finally {
      // The page's own clean has ended. A fresh read says whether another is
      // still running, so the background note does not flash up meanwhile.
      await readCleaning()
      cleaning = false
    }
    await scan()
  }

  function setAll(value: boolean) {
    for (const v of volumes) for (const t of v.tasks) setTask(v.mount, t, value)
  }

  /** How many of one task's items on one volume are ticked. */
  function taskTickedCount(mount: string, task: PreviewTask): number {
    return selectable(mount, task).filter((i) => checked[key(mount, i.path)]).length
  }

  /**
   * Ticks or unticks every item of one task on one volume, leaving other tasks
   * alone. Ignored and skipped items stay unticked.
   */
  function setTask(mount: string, task: PreviewTask, value: boolean) {
    for (const i of selectable(mount, task)) checked[key(mount, i.path)] = value
  }

  /**
   * Skips an item for this list, or offers it again. The page alone keeps
   * skips, so nothing reaches the backend. Either way the item ends unticked:
   * a skipped row cannot be cleaned, and an unskipped one waits for your tick.
   */
  function toggleSkip(mount: string, item: PreviewItem) {
    const k = key(mount, item.path)
    checked[k] = false
    skipped[k] = !skipped[k]
  }

  /**
   * Ignores an item, or stops ignoring it. The item is unticked before the
   * request goes out, so a quick Clean cannot send it, and stays unticked
   * until you tick it again.
   */
  async function toggleIgnore(mount: string, item: PreviewItem) {
    const k = key(mount, item.path)
    const ignore = !item.ignored
    checked[k] = false
    toggling[k] = true
    error = null
    try {
      await setIgnored(item.path, ignore)
      item.ignored = ignore
      // Ignore outranks a skip. Once unignored, the row should come back plain.
      if (ignore) delete skipped[k]
      notifyIgnoreChanged({ path: item.path, ignored: ignore, source: 'clean' })
      // One change can protect or free other rows too, such as the worktree
      // that holds this folder.
      await refreshProtection()
    } catch (e) {
      error = errorText(e)
    } finally {
      toggling[k] = false
    }
    afterBusy()
  }

  /** The notice whose Delete waits for a second click, keyed like `checked`. */
  let confirmingDelete = $state<string | null>(null)
  // The run lock lets one delete through at a time, so a second would only fail.
  let deletesInFlight = $state(0)
  const deleteBlocked = (k: string) =>
    deletesInFlight > 0 || toggling[k] || cleaning || cleaningAnywhere

  async function reveal(mount: string, path: string) {
    const k = key(mount, path)
    toggling[k] = true
    try {
      await revealNotice(path)
    } catch (e) {
      error = errorText(e)
    } finally {
      toggling[k] = false
    }
    afterBusy()
  }

  /**
   * Moves a confirmed notice to the Trash, then drops it and every row under
   * it. A row that went stale fails safely, because the backend judges it again.
   */
  async function deleteConfirmed(mount: string, path: string) {
    const k = key(mount, path)
    confirmingDelete = null
    toggling[k] = true
    deletesInFlight++
    error = null
    try {
      await deleteNotice(path)
      dropRows(path)
    } catch (e) {
      error = errorText(e)
    } finally {
      deletesInFlight--
      toggling[k] = false
    }
    afterBusy()
  }

  /** Takes `path` and everything under it off the page, with its ticks and skips. */
  function dropRows(path: string) {
    const gone = (p: string) => p === path || p.startsWith(`${path}/`)
    for (const v of volumes) {
      for (const t of v.tasks) {
        t.notices = t.notices?.filter((n) => !gone(n.path))
        for (const i of t.items.filter((i) => gone(i.path))) {
          delete checked[key(v.mount, i.path)]
          delete skipped[key(v.mount, i.path)]
        }
        t.items = t.items.filter((i) => !gone(i.path))
      }
    }
  }

  let refreshCalls = 0

  /**
   * Asks the backend how the ignore list protects every listed row, and
   * updates them. Cheap: it reads the config and checks paths, with no scan.
   * A row that becomes protected is unticked; one that is freed stays
   * unticked until you tick it.
   */
  async function refreshProtection() {
    const rows = volumes.flatMap((v) =>
      v.tasks.flatMap((t) => t.items.map((item) => ({ mount: v.mount, item }))),
    )
    if (rows.length === 0) return
    const call = ++refreshCalls
    const found = await getProtection(rows.map((r) => r.item.path))
    // Two quick toggles start two refreshes whose replies can arrive in either
    // order. Only the newest one read the config after every write before it.
    if (call !== refreshCalls) return
    if (found.length !== rows.length) {
      throw new Error(`get_protection returned ${found.length} answers for ${rows.length} rows`)
    }
    rows.forEach(({ mount, item }, n) => {
      const ignored = found[n] === 'ignored'
      const holds = found[n] === 'holds_ignored'
      if (ignored !== item.ignored || holds !== item.holds_ignored) {
        checked[key(mount, item.path)] = false
      }
      if (ignored || holds) delete skipped[key(mount, item.path)]
      item.ignored = ignored
      item.holds_ignored = holds
    })
  }

  // Background cleans (auto mode, the daily scan, the CLI) change the history
  // while this page is open. Only the records reload; a rescan would be slow
  // and would reset the ticks.
  $effect(() => {
    const unlisten = onHistoryChanged(reloadRecords)
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  // The poll loop has read free space: on its 30 second beat while this
  // window is open, or because the menu opened or a scan ended.
  $effect(() => {
    const unlisten = onStatusChanged(loadStatus)
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  // A notification's scan lands while the window is open.
  $effect(() => {
    const unlisten = onPreviewChanged(loadStored)
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  // The phase is read once the listener is live, so a scan that ends between
  // the two cannot leave the page showing it.
  $effect(() => {
    const unlisten = onScanChanged(phaseChanged)
    unlisten.then(() => readScanState())
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  $effect(() => {
    const unlisten = onCleaningChanged(cleaningChanged)
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  $effect(() =>
    onIgnoreChanged((change) => {
      if (change.source === 'clean') return
      refreshProtection().catch((e) => (error = errorText(e)))
    }),
  )

  reloadRecords()
  loadStored()
</script>

<main class="clean">
  <header class="head">
    <h1>Clean up</h1>
    <button class:primary={!scanned && !scanning} onclick={scan} disabled={busy()}
      >{scanned ? 'Rescan' : 'Scan'}</button
    >
    <p class="note">
      Nothing is removed until you press Clean. Picard re-checks every item just before removing it.
    </p>
  </header>

  {#if error}
    <p class="error alert">{error}</p>
  {/if}

  {#if cleaningAnywhere && !cleaning}
    <p class="note" role="status">
      Picard is cleaning in the background. You can clean again when it finishes.
    </p>
  {/if}

  {#if stale?.kind === 'newer'}
    {@const found = stale.found}
    <p class="stale" role="status">
      <span>A newer scan from {scannedText(new Date(newest(found)).toISOString())} is ready.</span>
      <button onclick={showLatest} disabled={busy()}>Show it</button>
    </p>
  {:else if stale?.kind === 'cleaned'}
    <p class="stale" role="status">
      <span>Picard cleaned in the background, so this list may be out of date.</span>
      <button onclick={scan} disabled={busy()}>Scan again</button>
    </p>
  {/if}

  {#if results}
    {#each results as r}
      <div class="result">
        <h2>{r.name}: freed {r.freed_text}</h2>
        {#if r.error}<p class="error">{r.error}</p>{/if}
        {#if r.stopped}<p class="muted">Stopped: {r.stopped}</p>{/if}
        {#each r.skipped as s}
          <p class="muted" title={tildePath(s.path)}>
            Skipped {itemTitle(s.task, s.path)}: {s.reason}
          </p>
        {/each}
        {#each r.failed as f}
          <p class="error" title={tildePath(f.path)}>
            {itemTitle(f.task, f.path)}: {f.reason}
          </p>
        {/each}
      </div>
    {/each}
  {/if}

  {#each sections as s, si (s.mount)}
    {@const notices = s.scan ? s.scan.tasks.flatMap((t) => t.notices ?? []) : []}
    <section class="volume" aria-labelledby="volume-{si}">
      <div class="volume-head">
        <h2 id="volume-{si}">{s.name}</h2>
        {#if s.disk && s.disk.total_bytes > 0}
          <span class="muted disk-size">{formatBytes(s.disk.total_bytes)} disk</span>
        {/if}
        <span class="spacer"></span>
        {#if s.scan}
          <span class="muted">Scanned {scannedText(s.scan.scanned_at)}</span>
        {/if}
      </div>

      <!-- Measurements as figures: what is free, what was found, what is ticked. -->
      <dl class="figures">
        {#if s.disk}
          {@const [n, u] = sizeParts(s.disk.free_bytes)}
          <div class="figure-cell available" class:low={s.disk.low}>
            <dt>Available</dt>
            <dd class="value"><span class="n">{n}</span> <span class="u">{u}</span></dd>
            <dd class="caption">{availableCaption(s.disk)}</dd>
          </div>
        {/if}
        {#if s.scan}
          {@const [fn, fu] = sizeParts(volumeTotal(s.scan))}
          {@const count = volumeCount(s.scan)}
          {@const [sn, su] = sizeParts(volumeSelected(s.scan))}
          <div class="figure-cell">
            <dt>Found, up to</dt>
            <dd class="value"><span class="n">{fn}</span> <span class="u">{fu}</span></dd>
            <dd class="caption">in {count} {count === 1 ? 'item' : 'items'}</dd>
          </div>
          <div class="figure-cell">
            <dt>Selected</dt>
            <dd class="value"><span class="n">{sn}</span> <span class="u">{su}</span></dd>
            {#if s.disk}
              <dd class="caption">
                {afterCleanText(s.disk.free_bytes, volumeSelected(s.scan), s.disk.threshold_bytes)}
              </dd>
            {/if}
          </div>
        {/if}
      </dl>

      <!-- The headroom meter: free now, ticked and the rest found, against the threshold. -->
      {#if s.disk && s.scan}
        {@const h = headroom(
          s.disk.free_bytes,
          volumeSelected(s.scan),
          volumeTotal(s.scan),
          s.disk.threshold_bytes,
        )}
        {#if h.scaleBytes > 0}
          <div class="meter" aria-hidden="true">
            <div class="track">
              <span class="seg free" style="flex-basis: {h.free}%"></span>
              <span class="seg chosen" style="flex-basis: {h.selected}%"></span>
              <span class="seg found" style="flex-basis: {h.found}%"></span>
              {#if h.pin !== null}
                <span class="pin" style="left: {h.pin}%"
                  ><span>{formatBytes(s.disk.threshold_bytes)} threshold</span></span
                >
              {/if}
            </div>
            <div class="axis"><span>0</span><span>{formatBytes(h.scaleBytes)}</span></div>
          </div>
          <ul class="legend" aria-hidden="true">
            <li><span class="key free"></span>Free now</li>
            <li><span class="key chosen"></span>Selected</li>
            <li><span class="key found"></span>Found, not selected</li>
          </ul>
        {/if}
      {/if}

      {#if s.scan}
        {@const v = s.scan}
        {#if notices.length > 0}
          <section class="notices" aria-label="Needs you">
            <h3>Needs you</h3>
            <ul>
              {#each notices as n}
                {@const k = key(v.mount, n.path)}
                <li class="notice">
                  <span class="text">
                    <span class="title" title={tildePath(n.path)}>{shortPath(n.path)}</span>
                    <span class="reason">{n.message}</span>
                  </span>
                  <span class="size">{n.size_text}</span>
                  <span class="actions">
                    <button
                      class="row-action"
                      onclick={() => reveal(v.mount, n.path)}
                      disabled={toggling[k] || cleaning}
                      aria-label="Open {tildePath(n.path)} in Finder">Open in Finder</button
                    >
                    {#if n.deletable && confirmingDelete !== k}
                      <button
                        class="row-action"
                        onclick={() => (confirmingDelete = k)}
                        disabled={deleteBlocked(k)}
                        aria-label="Delete {tildePath(n.path)}">Delete</button
                      >
                    {/if}
                  </span>
                  {#if n.deletable && confirmingDelete === k}
                    <div class="confirm">
                      <span>Move {tildePath(n.path)} ({n.size_text}) to the Trash?</span>
                      <button
                        class="row-action danger"
                        onclick={() => deleteConfirmed(v.mount, n.path)}
                        disabled={deleteBlocked(k)}
                        aria-label="Move {tildePath(n.path)} to the Trash">Move to Trash</button
                      >
                      <button
                        class="row-action"
                        onclick={() => (confirmingDelete = null)}
                        aria-label="Cancel deleting {tildePath(n.path)}">Cancel</button
                      >
                    </div>
                  {/if}
                </li>
              {/each}
            </ul>
          </section>
        {/if}

        <div class="ledger">
          {#each v.tasks as t, ti}
            {@const tk = key(v.mount, t.task)}
            {@const label = TASK_LABELS[t.task] ?? t.task}
            {@const listId = `task-${si}-${ti}`}
            <!-- The toggle and summary sit outside the h3 so the heading's name is the category alone. -->
            <div class="task">
              <button
                class="toggle"
                aria-expanded={open[tk]}
                aria-controls={listId}
                aria-label="{label} list"
                onclick={() => (open[tk] = !open[tk])}
                ><svg class="chevron" class:open={open[tk]} viewBox="0 0 16 16" aria-hidden="true"
                  ><path d="M6 3l5 5-5 5" /></svg
                ></button
              >
              <h3>
                {#if !t.error && selectable(v.mount, t).length > 0}
                  {@const ticked = taskTickedCount(v.mount, t)}
                  {@const total = selectable(v.mount, t).length}
                  <label class="group">
                    <input
                      type="checkbox"
                      checked={ticked === total}
                      indeterminate={ticked > 0 && ticked < total}
                      onchange={(e) => setTask(v.mount, t, e.currentTarget.checked)}
                    />
                    {label}
                  </label>
                {:else}
                  {label}
                {/if}
              </h3>
              <span class="summary">{#if !open[tk]}{closedSummary(v.mount, t)}{/if}</span>
              <span class="size">{#if !t.error && t.items.length > 0}{formatBytes(taskTotal(v.mount, t))}{/if}</span>
            </div>
            <div id={listId} class="items" hidden={!open[tk]}>
              {#if t.error}
                <p class="error">Could not scan: {t.error}</p>
              {:else if t.items.length === 0}
                <p class="muted">Nothing to remove.</p>
              {:else}
                <ul>
                  {#each t.items as i}
                    {@const k = key(v.mount, i.path)}
                    {@const skip = isSkipped(v.mount, i)}
                    <li class="candidate" class:excluded={isExcluded(v.mount, i)}>
                      <!-- One line per row. The path gives way first, and at narrow widths it is only in the tooltip. -->
                      <label class="item" title={tildePath(i.path)}>
                        <input
                          type="checkbox"
                          bind:checked={checked[k]}
                          disabled={isExcluded(v.mount, i) || cleaning}
                        />
                        <span class="text">
                          <span class="title">{itemTitle(t.task, i.path)}</span>
                          <span class="reason"
                            >{i.ignored
                              ? 'ignored, never cleaned'
                              : i.holds_ignored
                                ? 'holds an ignored folder, never cleaned'
                                : skip
                                  ? 'skipped this time'
                                  : i.reason}</span
                          >
                          <span class="path">{shortPath(i.path)}</span>
                        </span>
                        <span class="size">{i.size_text}</span>
                      </label>
                      <span class="actions">
                        <!-- A protected row is never cleaned, so skipping it would change nothing. -->
                        {#if !isProtected(i)}
                          <button
                            class="row-action"
                            onclick={() => toggleSkip(v.mount, i)}
                            disabled={toggling[k] || cleaning}
                            title={skip ? undefined : 'Leave out of this clean only'}
                            aria-pressed={skip}
                            aria-label="{skip ? 'Unskip' : 'Skip'} {tildePath(i.path)}"
                            >{skip ? 'Unskip' : 'Skip'}</button
                          >
                        {/if}
                        {#if IGNORABLE_TASKS.includes(t.task)}
                          <button
                            class="row-action"
                            onclick={() => toggleIgnore(v.mount, i)}
                            disabled={toggling[k] || cleaning}
                            aria-label="{i.ignored ? 'Unignore' : 'Ignore'} {tildePath(i.path)}"
                            >{i.ignored ? 'Unignore' : 'Ignore'}</button
                          >
                        {/if}
                      </span>
                    </li>
                  {/each}
                </ul>
              {/if}
            </div>
          {/each}
        </div>
      {/if}
    </section>
  {/each}

  {#if scanning}
    <div class="scanning" role="status" aria-live="polite">
      <div class="row">
        <strong>
          {waitingForClean ? 'Waiting for Picard to finish cleaning…' : 'Scanning your folders…'}
        </strong>
        <span class="spacer"></span>
        <span class="elapsed">{elapsed} s</span>
        <button onclick={cancel}>Cancel scan</button>
      </div>
      <div class="bar" aria-hidden="true"><span></span></div>
      <p class="note">
        {#if waitingForClean}
          The scan starts when the clean ends, so it never lists folders being removed.
        {:else}
          Measuring build folders and checking worktrees. Large folders can take half a minute.
        {/if}
      </p>
      {#each [72, 58, 66] as width}
        <div class="skeleton" aria-hidden="true">
          <span class="box"></span>
          <span class="line" style="width: {width}%"></span>
          <span class="box"></span>
        </div>
      {/each}
    </div>
  {:else if !scanned}
    <!-- After a failed first scan the error says what happened; the prompt would hide it. -->
    {#if !error}
      <p class="empty">Press Scan to find build folders, caches and stale worktrees.</p>
    {/if}
  {:else if volumes.length === 0}
    <p class="empty">No mounted volumes hold a configured root.</p>
  {:else}
    <div class="row footer">
      <button onclick={() => setAll(true)} disabled={cleaning}>Select all</button>
      <button onclick={() => setAll(false)} disabled={cleaning}>Select none</button>
      <span class="spacer"></span>
      <span class="tally">{selectedCount} selected, {formatBytes(selectedBytes)}</span>
      <button
        class="primary"
        onclick={clean}
        disabled={cleaning ||
          cleaningAnywhere ||
          busyToggling ||
          checkingStored ||
          selectedCount === 0}
      >
        {cleaning ? 'Cleaning…' : 'Clean selected'}
      </button>
    </div>
  {/if}

  {#if history.length > 0 || totals}
    <section class="history" aria-label="Recently removed">
      <div class="history-head">
        {#if history.length > 0}
          <h2>Recently removed</h2>
        {/if}
        <span class="spacer"></span>
        {#if totals}
          <dl class="freed">
            <div>
              <dt>Freed by Picard</dt>
              {#if totals.items > 0}
                <dd class="value">{formatBytes(totals.bytes)}</dd>
                <dd class="caption">{freedCaption(totals)}</dd>
              {:else}
                <dd class="value">0 MB</dd>
                <dd class="caption">Nothing removed yet</dd>
              {/if}
            </div>
          </dl>
        {/if}
      </div>
      {#if history.length > 0}
        <ul>
          {#each history as h}
            <li class="history-row" title={tildePath(h.path)}>
              <span class="when">{new Date(h.time).toLocaleString()}</span>
              <span class="text">{itemTitle(h.task, h.path)}</span>
              <span class="size">{formatBytes(h.bytes)}</span>
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}
</main>

<style>
  .clean {
    display: grid;
    gap: var(--space-lg);
    align-content: start;
    /* Shared by the category heading and its rows, so the rows line up under its checkbox. */
    --toggle-size: var(--space-lg);
    --task-gap: var(--space-xs);
  }

  .head {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: var(--space-xs) var(--space-md);
  }
  .note {
    grid-column: 1 / -1;
    color: var(--color-muted);
    font-size: var(--text-sm);
  }
  .alert {
    padding: var(--space-sm) var(--space-md);
    background: var(--color-paper-2);
    border-left: 2px solid var(--color-error);
    font-weight: 500;
  }
  .stale {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-md);
    background: var(--color-paper-2);
    border-left: 2px solid var(--color-accent-2);
  }
  .stale span {
    flex: 1;
  }
  .result {
    display: grid;
    gap: var(--space-2xs);
    padding-top: var(--space-sm);
    border-top: var(--rule-hair) solid var(--color-rule);
  }
  .result h2 {
    font-size: var(--text-md);
  }

  /* One volume: head, figures, meter, then what the scan found on it. */
  .volume {
    display: grid;
    gap: var(--space-md);
  }
  .volume-head {
    display: flex;
    align-items: baseline;
    gap: var(--space-xs);
    padding-top: var(--space-sm);
    border-top: var(--rule-hair) solid var(--color-rule);
    font-size: var(--text-sm);
  }
  .volume-head h2 {
    font-size: var(--text-lg);
  }

  /* Three across at every window width; the numbers are set to fit 480px. */
  .figures {
    display: flex;
    margin: 0;
  }
  .figure-cell {
    flex: 1 1 0;
    display: grid;
    gap: var(--space-3xs);
    align-content: start;
    min-width: 0;
    padding: 0 var(--space-sm);
    border-left: var(--rule-hair) solid var(--color-rule);
  }
  .figure-cell:first-child {
    padding-left: 0;
    border-left: 0;
  }
  .figure-cell:last-child {
    padding-right: 0;
  }
  .figure-cell dt {
    color: var(--color-muted);
    font-size: var(--text-sm);
    font-weight: 500;
  }
  .figure-cell dd {
    margin: 0;
  }
  .value {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .value .n {
    font-size: var(--text-display-s);
    font-variant-numeric: tabular-nums;
    letter-spacing: -0.03em;
    line-height: 1;
  }
  .value .u {
    color: var(--color-muted);
    font-size: var(--text-sm);
  }
  .caption {
    color: var(--color-muted);
    font-size: var(--text-xs);
    overflow-wrap: break-word;
  }
  .figure-cell.low .n,
  .figure-cell.low .caption {
    color: var(--color-error);
  }

  .meter {
    padding-top: var(--space-lg);
  }
  .track {
    position: relative;
    display: flex;
    height: 8px;
    outline: var(--rule-hair) solid var(--color-rule);
  }
  .seg {
    flex-grow: 0;
    flex-shrink: 0;
    min-width: 0;
  }
  .seg.free,
  .key.free {
    background: var(--color-muted-2);
  }
  .seg.chosen,
  .key.chosen {
    background: var(--color-accent);
  }
  .seg.found,
  .key.found {
    background: var(--color-accent-2);
  }
  .pin {
    position: absolute;
    top: -18px;
    bottom: -4px;
    width: 0;
    border-left: var(--rule-hair) solid var(--color-ink);
  }
  .pin span {
    position: absolute;
    bottom: 100%;
    right: 0;
    padding-right: var(--space-2xs);
    color: var(--color-ink-2);
    font-size: var(--text-xs);
    white-space: nowrap;
  }
  .axis {
    display: flex;
    justify-content: space-between;
    padding-top: var(--space-2xs);
    color: var(--color-muted);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    font-variant-numeric: tabular-nums;
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2xs) var(--space-md);
    margin: 0;
    padding: 0;
    list-style: none;
    color: var(--color-muted);
    font-size: var(--text-sm);
  }
  .legend li {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
  }
  .key {
    width: 10px;
    height: 10px;
    flex-shrink: 0;
  }

  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .items li + li,
  .notices li + li,
  .history li + li {
    border-top: var(--rule-hair) solid var(--color-rule-2);
  }

  /* Rows: one line each. Name and reason first, the path takes what is left. */
  .text {
    display: flex;
    align-items: baseline;
    gap: var(--space-xs);
    min-width: 0;
  }
  .title {
    flex: 0 0 auto;
    max-width: 65%;
    min-width: 0;
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .reason {
    flex: 0 1 auto;
    min-width: 0;
    color: var(--color-muted);
    font-size: var(--text-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .text .path {
    flex: 1 1 0;
    min-width: 0;
    color: var(--color-muted);
    font-size: var(--text-xs);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .size {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    text-align: right;
    white-space: nowrap;
  }
  .actions {
    display: flex;
    gap: var(--space-3xs);
    justify-content: end;
  }
  .row-action {
    min-height: 20px;
    padding: 0 var(--space-2xs);
    border: 0;
    color: var(--color-muted);
    font-size: var(--text-sm);
    font-weight: 400;
  }
  .row-action:hover {
    background: none;
    color: var(--color-ink);
    text-decoration: underline;
  }
  .row-action:active {
    transform: none;
    color: var(--color-ink-2);
  }
  .row-action:disabled {
    background: none;
  }
  .row-action[aria-pressed='true'] {
    color: var(--color-ink);
  }
  .row-action.danger {
    color: var(--color-error);
  }

  .notices {
    display: grid;
    gap: var(--space-xs);
    padding: var(--space-sm) var(--space-md);
    background: var(--color-paper-2);
    border-left: 2px solid var(--color-accent);
  }
  .notices h3 {
    font-size: var(--text-md);
  }
  .notice {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    align-items: baseline;
    column-gap: var(--space-sm);
    padding: var(--space-2xs) 0;
  }
  .notice .text {
    flex-wrap: wrap;
  }
  .notice .title {
    flex: 1 1 100%;
    max-width: none;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    font-weight: 400;
    white-space: normal;
    overflow-wrap: anywhere;
  }
  .confirm {
    grid-column: 1 / -1;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-xs);
    padding-top: var(--space-xs);
    font-size: var(--text-sm);
  }
  .confirm span {
    flex: 1 1 16rem;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  /* chevron | checkbox and name | summary when closed | what it could free */
  .task {
    display: grid;
    grid-template-columns: var(--toggle-size) minmax(0, 1fr) auto 64px;
    align-items: center;
    column-gap: var(--task-gap);
    padding: var(--space-2xs) 0;
    border-top: var(--rule-hair) solid var(--color-rule);
  }
  .task h3 {
    line-height: 1.45;
  }
  .summary {
    color: var(--color-muted);
    font-size: var(--text-sm);
  }
  /* Above the 24px minimum target WCAG 2.2 (2.5.8) allows. */
  .toggle {
    width: var(--toggle-size);
    height: var(--toggle-size);
    min-height: 0;
    padding: 0;
    border: none;
    color: var(--color-muted);
  }
  .toggle:hover {
    background: none;
    color: var(--color-ink);
  }
  .toggle:active {
    transform: none;
    color: var(--color-ink-2);
  }
  /* Drawn, not a text glyph: the small-triangle characters stay tiny at any font size. */
  .chevron {
    display: block;
    width: 14px;
    height: 14px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: square;
    transition: transform var(--dur-short) var(--ease-in-out);
  }
  .chevron.open {
    transform: rotate(90deg);
  }
  .group {
    display: inline-flex;
    align-items: center;
    gap: var(--space-sm);
    cursor: pointer;
  }
  .items {
    padding: 0 0 var(--space-xs) calc(var(--toggle-size) + var(--task-gap));
  }
  .items > p {
    padding: var(--space-2xs) 0;
    font-size: var(--text-sm);
  }

  /* item | Skip and Ignore */
  .candidate {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 92px;
    align-items: center;
    column-gap: var(--space-xs);
    min-height: 24px;
    padding: var(--space-3xs) 0;
  }
  .candidate:has(input:checked) {
    background: var(--color-paper-3);
  }
  /* checkbox | name, reason and path | size */
  .item {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) 64px;
    align-items: center;
    column-gap: var(--space-sm);
    min-width: 0;
    cursor: pointer;
  }
  .candidate.excluded .item {
    opacity: 0.55;
    cursor: default;
  }

  .scanning {
    display: grid;
    gap: var(--space-sm);
    padding-top: var(--space-sm);
    border-top: var(--rule-hair) solid var(--color-rule);
  }
  .scanning strong {
    font-weight: 500;
  }
  .elapsed {
    color: var(--color-muted);
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  /* Indeterminate: a short stripe that sweeps across. The scan reports no progress. */
  .bar {
    position: relative;
    height: 2px;
    overflow: hidden;
    background: var(--color-rule);
  }
  .bar span {
    position: absolute;
    inset: 0 auto 0 0;
    width: 35%;
    background: var(--color-accent);
    animation: sweep 1.4s linear infinite;
  }
  @keyframes sweep {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(290%);
    }
  }
  /* Placeholder rows in the shape of results: checkbox | name | size. */
  .skeleton {
    display: grid;
    grid-template-columns: 14px 1fr 48px;
    column-gap: var(--space-sm);
    align-items: center;
    height: 24px;
    border-top: var(--rule-hair) solid var(--color-rule-2);
  }
  .skeleton .box,
  .skeleton .line {
    display: block;
    height: 10px;
    background: var(--color-rule);
  }
  .empty {
    padding-top: var(--space-sm);
    border-top: var(--rule-hair) solid var(--color-rule);
    color: var(--color-ink-2);
  }

  /* The action bar stays in reach while the list scrolls, edge to edge. */
  .footer {
    position: sticky;
    bottom: 0;
    z-index: var(--z-sticky);
    margin-inline: calc(-1 * var(--gutter));
    padding: var(--space-sm) var(--gutter);
    background: var(--color-paper);
    border-top: var(--rule-hair) solid var(--color-rule);
  }
  /* One line at any width: the tally gives way before Clean selected wraps. */
  .footer {
    flex-wrap: nowrap;
  }
  .tally {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--color-muted);
    font-size: var(--text-sm);
    font-variant-numeric: tabular-nums;
    padding-right: var(--space-xs);
  }

  .history {
    display: grid;
    gap: var(--space-xs);
  }
  .history-head {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-xs) var(--space-md);
    padding-top: var(--space-sm);
    border-top: var(--rule-hair) solid var(--color-rule);
  }
  .history-head h2 {
    font-size: var(--text-lg);
  }
  .freed {
    margin: 0;
  }
  .freed div {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-xs);
  }
  .freed dt {
    color: var(--color-muted);
    font-size: var(--text-sm);
  }
  .freed dd {
    margin: 0;
  }
  .freed .value {
    font-size: var(--text-lg);
  }
  .freed .caption {
    font-size: var(--text-sm);
  }
  .history-row {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    column-gap: var(--space-sm);
    padding: var(--space-3xs) 0;
    font-size: var(--text-sm);
  }
  .history-row .when {
    color: var(--color-muted);
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  .history-row .text {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Narrow windows: a row's path moves to its tooltip, since a few characters of it say nothing. */
  @media (max-width: 40rem) {
    .text .path {
      display: none;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
    .bar span {
      animation: none;
      width: 100%;
      opacity: 0.4;
    }
  }
</style>
