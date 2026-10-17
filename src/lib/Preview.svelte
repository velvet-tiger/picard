<script lang="ts">
  import {
    preview,
    runSelected,
    getHistory,
    errorText,
    formatBytes,
    itemTitle,
    shortPath,
    tildePath,
    TASK_LABELS,
    type VolumePreview,
    type RunResult,
    type HistoryEntry,
  } from './api'

  let volumes = $state<VolumePreview[]>([])
  // Keyed by "mount\u0000path" so the same path on two volumes cannot collide.
  let checked = $state<Record<string, boolean>>({})
  let history = $state<HistoryEntry[]>([])
  let scanning = $state(true)
  let cleaning = $state(false)
  let error = $state<string | null>(null)
  let results = $state<RunResult[] | null>(null)

  const key = (mount: string, path: string) => `${mount}\u0000${path}`

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
      // Everything starts ticked; one click on "Clean selected" removes it all.
      checked = Object.fromEntries(
        found.flatMap((v) =>
          v.tasks.flatMap((t) => t.items.map((i) => [key(v.mount, i.path), true])),
        ),
      )
    } catch (e) {
      error = errorText(e)
    } finally {
      scanning = false
    }
    loadHistory()
  }

  async function loadHistory() {
    try {
      history = await getHistory(10)
    } catch (e) {
      // History is a convenience; a failure here should not hide the scan.
      console.error('history', e)
    }
  }

  async function clean() {
    cleaning = true
    error = null
    results = null
    const selections = volumes
      .map((v) => ({
        mount: v.mount,
        paths: v.tasks.flatMap((t) =>
          t.items.filter((i) => checked[key(v.mount, i.path)]).map((i) => i.path),
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
    checked = Object.fromEntries(Object.keys(checked).map((k) => [k, value]))
  }

  scan()
</script>

<main>
  <div class="row">
    <h1>Clean up</h1>
    <span class="spacer"></span>
    <button onclick={scan} disabled={scanning || cleaning}>Rescan</button>
  </div>
  <p class="muted">
    Nothing is removed until you press Clean. Picard re-checks every item just before removing it.
  </p>

  {#if error}
    <p class="error">{error}</p>
  {/if}

  {#if results}
    {#each results as r}
      <div class="panel">
        <h2>{r.name}: freed {r.freed_text}</h2>
        {#if r.error}<p class="error">{r.error}</p>{/if}
        {#if r.stopped}<p class="muted">Stopped: {r.stopped}</p>{/if}
        {#each r.failed as f}
          <p class="error" title={tildePath(f.path)}>
            {itemTitle(f.task, f.path)}: {f.reason}
          </p>
        {/each}
      </div>
    {/each}
  {/if}

  {#if scanning}
    <p class="muted">Scanning… this can take half a minute on large folders.</p>
  {:else if volumes.length === 0}
    <p class="muted">No mounted volumes hold a configured root.</p>
  {:else}
    {#each volumes as v}
      <div class="panel">
        <div class="row">
          <h2>{v.name}</h2>
          <span class="muted">{v.free_text}</span>
          <span class="spacer"></span>
          <span class="muted">up to {v.total_text}</span>
        </div>
        {#each v.tasks as t}
          <h3>{TASK_LABELS[t.task] ?? t.task}</h3>
          {#if t.error}
            <p class="error">Could not scan: {t.error}</p>
          {:else if t.items.length === 0}
            <p class="muted">Nothing to remove.</p>
          {:else}
            <ul>
              {#each t.items as i}
                <li>
                  <label class="item" title={tildePath(i.path)}>
                    <input type="checkbox" bind:checked={checked[key(v.mount, i.path)]} />
                    <span class="text">
                      <span class="title"
                        >{itemTitle(t.task, i.path)}
                        <span class="muted reason">{i.reason}</span></span
                      >
                      <span class="path muted">{shortPath(i.path)}</span>
                    </span>
                    <span class="size">{i.size_text}</span>
                  </label>
                </li>
              {/each}
            </ul>
          {/if}
        {/each}
      </div>
    {/each}

    <div class="row footer">
      <button onclick={() => setAll(true)} disabled={cleaning}>Select all</button>
      <button onclick={() => setAll(false)} disabled={cleaning}>Select none</button>
      <span class="spacer"></span>
      <span class="muted">{selectedCount} selected, {formatBytes(selectedBytes)}</span>
      <button class="primary" onclick={clean} disabled={cleaning || selectedCount === 0}>
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
    margin: 12px 0 4px;
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
</style>
