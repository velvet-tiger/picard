<script lang="ts">
  import {
    preview,
    runSelected,
    getHistory,
    getTotals,
    setIgnored,
    getProtection,
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
  } from './api'

  let volumes = $state<VolumePreview[]>([])
  // Keyed by "mount\u0000path" so the same path on two volumes cannot collide.
  let checked = $state<Record<string, boolean>>({})
  let history = $state<HistoryEntry[]>([])
  let totals = $state<Totals | null>(null)
  let scanning = $state(true)
  let elapsed = $state(0)

  // Counts seconds while a scan runs. The scan reports no progress of its own,
  // so elapsed time is the honest thing to show.
  $effect(() => {
    if (!scanning) return
    elapsed = 0
    const started = Date.now()
    const timer = setInterval(() => (elapsed = Math.floor((Date.now() - started) / 1000)), 250)
    return () => clearInterval(timer)
  })
  let cleaning = $state(false)
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

  async function scan() {
    scanning = true
    error = null
    try {
      const found = await preview()
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
    } catch (e) {
      error = errorText(e)
    } finally {
      scanning = false
    }
    reloadRecords()
  }

  // A clean appends once per task, and FSEvents reports each append as several
  // events, each a `history-changed`. Reloads in flight absorb those events,
  // and one more reload follows them,
  // so the page ends on the newest file without a request per event.
  let reloading = false
  let reloadAgain = false

  /** Reloads the total and "Recently removed" list. Never throws; both loaders log their own errors. */
  async function reloadRecords() {
    if (reloading) {
      reloadAgain = true
      return
    }
    reloading = true
    try {
      do {
        reloadAgain = false
        await Promise.all([loadHistory(), loadTotals()])
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

  const sinceText = (time: string) =>
    new Date(time).toLocaleDateString(undefined, {
      day: 'numeric',
      month: 'short',
      year: 'numeric',
    })

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

  $effect(() =>
    onIgnoreChanged((change) => {
      if (change.source === 'clean') return
      refreshProtection().catch((e) => (error = errorText(e)))
    }),
  )

  scan()
</script>

<main>
  <div class="row">
    <h1>Clean up</h1>
    <span class="spacer"></span>
    <button onclick={scan} disabled={scanning || cleaning || busyToggling}>Rescan</button>
  </div>
  <p class="muted">
    Nothing is removed until you press Clean. Picard re-checks every item just before removing it.
  </p>

  {#if totals && totals.items > 0 && totals.since}
    <p class="totals">
      Picard has freed <strong>{formatBytes(totals.bytes)}</strong> across {totals.items}
      {totals.items === 1 ? 'item' : 'items'} since {sinceText(totals.since)}.
    </p>
  {/if}

  {#if error}
    <p class="error">{error}</p>
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
        <strong>Scanning your folders…</strong>
        <span class="muted elapsed">{elapsed} s</span>
      </div>
      <div class="bar" aria-hidden="true"><span></span></div>
      <p class="muted hint">
        Measuring build folders and checking worktrees. Large folders can take half a minute.
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
  {:else if volumes.length === 0}
    <p class="muted">No mounted volumes hold a configured root.</p>
  {:else}
    {#each volumes as v, vi}
      <div class="panel">
        <div class="row">
          <h2>{v.name}</h2>
          <span class="muted">{v.free_text}</span>
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
        disabled={cleaning || busyToggling || selectedCount === 0}
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
  .totals {
    font-variant-numeric: tabular-nums;
  }
</style>
