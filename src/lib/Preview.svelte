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
    getProtection,
    getScanState,
    cancelScan,
    onScanChanged,
    onCleaningChanged,
    notifyIgnoreChanged,
    onIgnoreChanged,
    onHistoryChanged,
    errorText,
    formatBytes,
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

  // Paths whose Ignore or Unignore request is still in flight, keyed like `checked`.
  let toggling = $state<Record<string, boolean>>({})
  // Cleaning or rescanning mid-toggle could act on, or show, the list as it was.
  const busyToggling = $derived(Object.values(toggling).some(Boolean))

  /** Never removed: ignored, or holding an ignored folder (see IgnoreList in tasks/mod.rs). */
  const isProtected = (i: PreviewItem) => i.ignored || i.holds_ignored

  /** Items that can be ticked: everything not protected. */
  const selectable = (task: PreviewTask) => task.items.filter((i) => !isProtected(i))

  /** What a volume could free now. Recomputed here so it follows Ignore clicks. */
  const volumeTotal = (v: VolumePreview) =>
    v.tasks.flatMap(selectable).reduce((sum, i) => sum + i.bytes, 0)

  // Whether each category's list is shown, keyed by key(mount, task).
  let open = $state<Record<string, boolean>>({})

  /**
   * A category starts closed when it holds nothing Clean could remove. One that
   * failed to scan stays open so its error shows.
   */
  const startsOpen = (task: PreviewTask) => task.error !== null || selectable(task).length > 0

  /** What a closed category's heading says about the hidden list. */
  function closedSummary(task: PreviewTask): string {
    if (task.error) return 'could not scan'
    if (task.items.length === 0) return 'nothing to remove'
    const free = selectable(task).length
    if (free === 0) return 'all ignored'
    const items = `${free} ${free === 1 ? 'item' : 'items'}`
    const kept = task.items.length - free
    return kept === 0 ? items : `${items}, ${kept} ignored`
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
    // Everything not ignored starts ticked; one click on "Clean selected" removes it all.
    checked = Object.fromEntries(
      found.flatMap((v) =>
        v.tasks.flatMap((t) => t.items.map((i) => [key(v.mount, i.path), !isProtected(i)])),
      ),
    )
    // Set on every scan only. An Ignore click that empties a category leaves
    // it open, so the list does not vanish from under the pointer.
    open = Object.fromEntries(
      found.flatMap((v) => v.tasks.map((t) => [key(v.mount, t.task), startsOpen(t)])),
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

  /**
   * Cancels the running scan, whoever started it. Then reads the phase again,
   * so a missed `scan-changed` cannot leave the panel up with nothing running.
   */
  async function cancel() {
    try {
      await cancelScan()
    } catch (e) {
      error = errorText(e)
      return
    }
    await readScanState()
  }

  function phaseChanged(next: ScanPhase) {
    scanEvents++
    applyPhase(next)
  }

  /** Reads the phase, for a window opened mid-scan and as a backstop for a missed event. */
  async function readScanState() {
    const seen = scanEvents
    let now: ScanPhase
    try {
      now = await getScanState()
    } catch (e) {
      console.error('scan state', e)
      return
    }
    if (seen === scanEvents) applyPhase(now)
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

  async function loadStatus() {
    try {
      const seen = cleaningEvents
      status = await getStatus()
      if (seen === cleaningEvents) cleaningAnywhere = status.cleaning
    } catch (e) {
      // The disk tiles are a convenience; hide them rather than show stale numbers.
      status = null
      console.error('status', e)
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

  async function clean() {
    cleaning = true
    error = null
    results = null
    const selections = volumes
      .map((v) => ({
        mount: v.mount,
        paths: v.tasks.flatMap((t) =>
          selectable(t)
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
    return selectable(task).filter((i) => checked[key(mount, i.path)]).length
  }

  /**
   * Ticks or unticks every item of one task on one volume, leaving other tasks
   * alone. Ignored items stay unticked.
   */
  function setTask(mount: string, task: PreviewTask, value: boolean) {
    for (const i of selectable(task)) checked[key(mount, i.path)] = value
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
    unlisten.then(readScanState)
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

<main>
  <div class="row">
    <h1>Clean up</h1>
    <span class="spacer"></span>
    <button onclick={scan} disabled={busy()}>{scanned ? 'Rescan' : 'Scan'}</button
    >
  </div>
  <p class="muted">
    Nothing is removed until you press Clean. Picard re-checks every item just before removing it.
  </p>

  {#if diskRows.length > 0 || totals}
    <!-- One row of disk tiles per volume; the freed tile spans them, since it covers every volume. -->
    <section aria-label="Disk summary">
      <dl class="stats">
        {#each diskRows as v, row}
          <div class="stat disk" style="grid-row: {row + 1}">
            <dt>Disk size</dt>
            <dd class="value">{formatBytes(v.total_bytes)}</dd>
            <dd class="sub" title={v.mount}>{v.name}</dd>
          </div>
          <div class="stat available" class:low={v.low} style="grid-row: {row + 1}">
            <dt>Available</dt>
            <dd class="value">{formatBytes(v.free_bytes)}</dd>
            <dd class="sub">{availableCaption(v)}</dd>
          </div>
        {/each}
        {#if totals}
          <div
            class="stat freed"
            class:solo={diskRows.length === 0}
            style="grid-row: 1 / span {Math.max(diskRows.length, 1)}"
          >
            <dt>Freed by Picard</dt>
            {#if totals.items > 0}
              <dd class="value">{formatBytes(totals.bytes)}</dd>
              <dd class="sub">{freedCaption(totals)}</dd>
            {:else}
              <dd class="value">0 MB</dd>
              <dd class="sub">Nothing removed yet</dd>
            {/if}
          </div>
        {/if}
      </dl>
    </section>
  {/if}

  {#if error}
    <p class="error">{error}</p>
  {/if}

  {#if cleaningAnywhere && !cleaning}
    <p class="muted" role="status">
      Picard is cleaning in the background. You can clean again when it finishes.
    </p>
  {/if}

  {#if stale?.kind === 'newer'}
    {@const found = stale.found}
    <p class="panel stale" role="status">
      <span>A newer scan from {scannedText(new Date(newest(found)).toISOString())} is ready.</span>
      <button onclick={showLatest} disabled={busy()}>Show it</button>
    </p>
  {:else if stale?.kind === 'cleaned'}
    <p class="panel stale" role="status">
      <span>Picard cleaned in the background, so this list may be out of date.</span>
      <button onclick={scan} disabled={busy()}>Scan again</button>
    </p>
  {/if}

  {#if results}
    {#each results as r}
      <div class="panel">
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

  {#if scanning}
    <div class="panel scanning" role="status" aria-live="polite">
      <div class="row">
        <span class="spinner" aria-hidden="true"></span>
        <strong>
          {waitingForClean ? 'Waiting for Picard to finish cleaning…' : 'Scanning your folders…'}
        </strong>
        <span class="muted elapsed">{elapsed} s</span>
        <span class="spacer"></span>
        <button onclick={cancel}>Cancel scan</button>
      </div>
      <div class="bar" aria-hidden="true"><span></span></div>
      <p class="muted hint">
        {#if waitingForClean}
          The scan starts when the clean ends, so it never lists folders being removed.
        {:else}
          Measuring build folders and checking worktrees. Large folders can take half a minute.
        {/if}
      </p>
      {#each [72, 58, 66] as width}
        <div class="skeleton" aria-hidden="true">
          <span class="box"></span>
          <span class="lines"
            ><span style="width: {width}%"></span><span style="width: {width - 20}%"></span></span
          >
          <span class="box"></span>
        </div>
      {/each}
    </div>
  {:else if !scanned}
    <!-- After a failed first scan the error says what happened; the prompt would hide it. -->
    {#if !error}
      <p class="muted">Press Scan to find build folders, caches and stale worktrees.</p>
    {/if}
  {:else if volumes.length === 0}
    <p class="muted">No mounted volumes hold a configured root.</p>
  {:else}
    {#each volumes as v, vi}
      <div class="panel">
        <div class="row">
          <h2>{v.name}</h2>
          <span class="muted">{v.free_text}</span>
          <span class="muted">· scanned {scannedText(v.scanned_at)}</span>
          <span class="spacer"></span>
          <span class="muted">up to {formatBytes(volumeTotal(v))}</span>
        </div>
        {#each v.tasks as t, ti}
          {@const tk = key(v.mount, t.task)}
          {@const label = TASK_LABELS[t.task] ?? t.task}
          {@const listId = `task-${vi}-${ti}`}
          <!-- The toggle and summary sit outside the h3 so the heading's name is the category alone. -->
          <div class="task">
            <button
              class="toggle"
              aria-expanded={open[tk]}
              aria-controls={listId}
              aria-label="{label} list"
              onclick={() => (open[tk] = !open[tk])}
              ><span class="chevron" class:open={open[tk]} aria-hidden="true">▸</span></button
            >
            <h3>
              {#if !t.error && selectable(t).length > 0}
                {@const ticked = taskTickedCount(v.mount, t)}
                {@const total = selectable(t).length}
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
            {#if !open[tk]}
              <span class="muted summary">{closedSummary(t)}</span>
            {/if}
          </div>
          <div id={listId} hidden={!open[tk]}>
            {#if t.error}
              <p class="error">Could not scan: {t.error}</p>
            {:else if t.items.length === 0}
              <p class="muted">Nothing to remove.</p>
            {:else}
              <ul>
                {#each t.items as i}
                  {@const k = key(v.mount, i.path)}
                  <li class="candidate" class:ignored={isProtected(i)}>
                    <!-- The row already shows the path; a tooltip only helps when shortPath has elided part of it. -->
                    <label
                      class="item"
                      title={shortPath(i.path) !== tildePath(i.path) ? tildePath(i.path) : undefined}
                    >
                      <input
                        type="checkbox"
                        bind:checked={checked[k]}
                        disabled={isProtected(i) || cleaning}
                      />
                      <span class="text">
                        <span class="title"
                          >{itemTitle(t.task, i.path)}
                          <span class="muted reason"
                            >{i.ignored
                              ? 'ignored, never cleaned'
                              : i.holds_ignored
                                ? 'holds an ignored folder, never cleaned'
                                : i.reason}</span
                          ></span
                        >
                        <span class="path muted">{shortPath(i.path)}</span>
                      </span>
                      <span class="size">{i.size_text}</span>
                    </label>
                    {#if IGNORABLE_TASKS.includes(t.task)}
                      <button
                        class="ignore"
                        onclick={() => toggleIgnore(v.mount, i)}
                        disabled={toggling[k] || cleaning}
                        aria-label="{i.ignored ? 'Stop ignoring' : 'Ignore'} {tildePath(i.path)}"
                        >{i.ignored ? 'Unignore' : 'Ignore'}</button
                      >
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}
          </div>
        {/each}
      </div>
    {/each}

    <div class="row footer">
      <button onclick={() => setAll(true)} disabled={cleaning}>Select all</button>
      <button onclick={() => setAll(false)} disabled={cleaning}>Select none</button>
      <span class="spacer"></span>
      <span class="muted">{selectedCount} selected, {formatBytes(selectedBytes)}</span>
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

  {#if history.length > 0}
    <div class="panel">
      <h2>Recently removed</h2>
      <ul class="history">
        {#each history as h}
          <li class="history-row" title={tildePath(h.path)}>
            <span class="muted">{new Date(h.time).toLocaleString()}</span>
            <span class="text">{itemTitle(h.task, h.path)}</span>
            <span class="size">{formatBytes(h.bytes)}</span>
          </li>
        {/each}
      </ul>
    </div>
  {/if}
</main>

<style>
  h3 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
  }
  /* chevron | checkbox and name | summary when closed */
  .task {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 12px 0 4px;
  }
  .task h3 {
    margin: 0;
  }
  /* 24px is the smallest target WCAG 2.2 (2.5.8) allows. */
  .toggle {
    min-width: 24px;
    min-height: 24px;
    border: none;
    background: none;
    padding: 0;
    color: var(--muted);
    font-size: 11px;
    cursor: pointer;
  }
  .chevron {
    display: inline-block;
    transition: transform 0.15s;
  }
  .chevron.open {
    transform: rotate(90deg);
  }
  .summary {
    font-size: 12px;
  }
  .group {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    cursor: pointer;
  }
  .group input {
    margin: 0;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  li {
    padding: 4px 0;
    border-bottom: 1px solid var(--line);
  }
  li:last-child {
    border-bottom: none;
  }
  /* item | Ignore button */
  .candidate {
    display: flex;
    align-items: start;
    gap: 10px;
  }
  .candidate .item {
    flex: 1;
    min-width: 0;
  }
  .candidate.ignored .item {
    opacity: 0.55;
    cursor: default;
  }
  .ignore {
    font-size: 11px;
    padding: 1px 8px;
    white-space: nowrap;
  }
  /* checkbox | name, reason and path | size */
  .item {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    column-gap: 10px;
    align-items: start;
    cursor: pointer;
  }
  .item input {
    margin-top: 2px;
  }
  .text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .title {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .reason {
    font-weight: normal;
    font-size: 12px;
    margin-left: 6px;
  }
  .item .path {
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .size {
    text-align: right;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .scanning .row {
    gap: 10px;
  }
  .elapsed {
    font-variant-numeric: tabular-nums;
  }
  .hint {
    margin: 8px 0 12px;
  }
  .spinner {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    border: 2px solid var(--line);
    border-top-color: var(--accent);
    animation: spin 0.8s linear infinite;
  }
  /* Indeterminate bar: a short stripe that sweeps across. */
  .bar {
    position: relative;
    height: 4px;
    margin-top: 12px;
    border-radius: 2px;
    background: var(--line);
    overflow: hidden;
  }
  .bar span {
    position: absolute;
    inset: 0 auto 0 0;
    width: 35%;
    border-radius: 2px;
    background: var(--accent);
    animation: sweep 1.4s ease-in-out infinite;
  }
  /* Placeholder rows in the shape of results: checkbox | two lines | size. */
  .skeleton {
    display: grid;
    grid-template-columns: 14px minmax(0, 1fr) 48px;
    column-gap: 10px;
    align-items: start;
    padding: 8px 0;
    border-top: 1px solid var(--line);
  }
  .skeleton .box,
  .skeleton .lines span {
    display: block;
    height: 12px;
    border-radius: 4px;
    background: linear-gradient(90deg, var(--line) 25%, var(--bg) 50%, var(--line) 75%);
    background-size: 200% 100%;
    animation: shimmer 1.6s linear infinite;
  }
  .skeleton .box {
    height: 14px;
  }
  .skeleton .lines {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @keyframes sweep {
    0% {
      transform: translateX(-100%);
    }
    100% {
      transform: translateX(290%);
    }
  }
  @keyframes shimmer {
    to {
      background-position: -200% 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
    .spinner,
    .bar span,
    .skeleton .box,
    .skeleton .lines span {
      animation: none;
    }
  }
  .history-row {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    column-gap: 10px;
  }
  .history-row .text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    display: block;
  }
  .footer {
    margin-top: 14px;
    position: sticky;
    bottom: 0;
    background: var(--bg);
    padding: 10px 0;
  }
  .history li {
    font-size: 12px;
  }
  /* size | available | freed, with freed spanning every volume's row. */
  .stats {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 10px;
    margin: 12px 0 0;
  }
  .stat {
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 8px;
    padding: 10px 14px;
    min-width: 0;
  }
  .stat.disk {
    grid-column: 1;
  }
  .stat.available {
    grid-column: 2;
  }
  .stat.freed {
    grid-column: 3;
    display: flex;
    flex-direction: column;
    justify-content: center;
  }
  .stat dt {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
  }
  .stat dd {
    margin: 0;
  }
  .stat .value {
    font-size: 22px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .stat .sub {
    font-size: 12px;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* The threshold warning must stay readable at the 480px minimum window width. */
  .stat.available .sub {
    white-space: normal;
  }
  /* With no volume rows, the freed tile takes the whole strip instead of the third column. */
  .stat.freed.solo {
    grid-column: 1 / -1;
  }
  .stat.low .value,
  .stat.low .sub {
    color: var(--danger);
  }

  .stale {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
  }

  .stale span {
    flex: 1;
  }
</style>
