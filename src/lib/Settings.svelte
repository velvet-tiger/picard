<script lang="ts">
  import {
    getSettings,
    saveSettings,
    getLaunchAtLogin,
    setLaunchAtLogin,
    pickFolder,
    closeWindow,
    errorText,
    newTask,
    withEveryBuiltIn,
    TASK_LABELS,
    type Settings,
  } from './api'

  let settings = $state<Settings | null>(null)
  let launchAtLogin = $state(false)
  let loadError = $state<string | null>(null)
  let saveError = $state<string | null>(null)
  let saved = $state(false)
  let saving = $state(false)
  // The settings as last loaded or saved, to tell whether closing loses edits.
  let savedJson = $state('')
  let confirmingClose = $state(false)
  const dirty = $derived(settings !== null && JSON.stringify(settings) !== savedJson)

  const MODES = [
    { value: 'ask', label: 'Ask', help: 'Notify and wait for you to choose Clean.' },
    { value: 'auto', label: 'Auto', help: 'Clean straight away, then tell you what was freed.' },
    { value: 'off', label: 'Off', help: 'Only show free space.' },
  ] as const

  async function load() {
    try {
      const loaded = await getSettings()
      // Filled in before `savedJson` is taken, so tasks that are only listed,
      // not changed, do not count as unsaved edits.
      loaded.tasks = withEveryBuiltIn(loaded.tasks)
      settings = loaded
      savedJson = JSON.stringify(settings)
      launchAtLogin = await getLaunchAtLogin()
    } catch (e) {
      loadError = errorText(e)
    }
  }

  async function addFolder() {
    if (!settings) return
    try {
      const folder = await pickFolder()
      if (folder && !settings.roots.includes(folder)) settings.roots.push(folder)
    } catch (e) {
      saveError = errorText(e)
    }
  }

  async function save() {
    if (!settings) return
    saving = true
    saveError = null
    saved = false
    // A row left blank is not a folder or pattern; drop it rather than fail validation.
    const clean = $state.snapshot(settings)
    clean.roots = clean.roots.map((r) => r.trim()).filter(Boolean)
    clean.exclude = clean.exclude.map((p) => p.trim()).filter(Boolean)
    try {
      await saveSettings(clean)
      settings.roots = clean.roots
      settings.exclude = clean.exclude
      savedJson = JSON.stringify(settings)
      saved = true
    } catch (e) {
      saveError = errorText(e)
    } finally {
      saving = false
    }
  }

  async function toggleLogin(enabled: boolean) {
    try {
      await setLaunchAtLogin(enabled)
      launchAtLogin = enabled
    } catch (e) {
      saveError = errorText(e)
      launchAtLogin = !enabled
    }
  }

  /**
   * Closes the window, or shows the unsaved-changes bar first when there are edits.
   * App.svelte calls this for ⌘W and Escape from either tab.
   */
  export async function close(discard = false) {
    if (dirty && !discard) {
      confirmingClose = true
      return
    }
    try {
      await closeWindow()
    } catch (e) {
      saveError = errorText(e)
    }
  }

  /** True when there are edits that closing the window would lose. */
  export function hasUnsavedChanges(): boolean {
    return dirty
  }

  /** Hides the unsaved-changes bar. Returns false when it was not showing. */
  export function dismissCloseConfirm(): boolean {
    if (!confirmingClose) return false
    confirmingClose = false
    return true
  }

  function move(i: number, by: number) {
    if (!settings) return
    const j = i + by
    if (j < 0 || j >= settings.tasks.length) return
    const tasks = settings.tasks
    ;[tasks[i], tasks[j]] = [tasks[j], tasks[i]]
  }

  function setUnit(unit: 'gb' | 'pct') {
    if (!settings || settings.threshold.unit === unit) return
    settings.threshold = unit === 'gb' ? { unit, value: 30 } : { unit, value: 10 }
  }

  load()
</script>

<main>
  <h1>Settings</h1>
  <p class="muted">Saved to config.toml. Comments and sections not shown here are kept.</p>

  {#if loadError}
    <p class="error">{loadError}</p>
  {:else if !settings}
    <p class="muted">Loading…</p>
  {:else}
    <section class="panel">
      <h2>When space runs low</h2>
      {#each MODES as m}
        <label class="choice">
          <input type="radio" name="mode" value={m.value} bind:group={settings.mode} />
          <strong>{m.label}</strong> <span class="muted">{m.help}</span>
        </label>
      {/each}
    </section>

    <section class="panel">
      <h2>Daily scan</h2>
      <div class="field">
        <label class="row check">
          <input
            type="checkbox"
            checked={settings.daily_scan !== null}
            onchange={(e) => (settings!.daily_scan = e.currentTarget.checked ? '03:00' : null)}
          />
          Also scan once a day at
        </label>
        <span class="control">
          {#if settings.daily_scan !== null}
            <input type="time" required bind:value={settings.daily_scan} />
          {/if}
        </span>
      </div>
      <p class="muted">
        Runs even when space is fine, and follows the mode above. Auto cleans everything stale, Ask
        notifies you, Off does nothing.
      </p>
    </section>

    <section class="panel">
      <h2>Threshold</h2>
      <div class="field">
        <span>Low when free space is below</span>
        <span class="control">
          <input type="number" min="0" step="any" bind:value={settings.threshold.value} />
          <select
            value={settings.threshold.unit}
            onchange={(e) => setUnit(e.currentTarget.value as 'gb' | 'pct')}
          >
            <option value="gb">GB</option>
            <option value="pct">%</option>
          </select>
        </span>
      </div>
      <div class="field">
        <span>Stop cleaning once this much more is free</span>
        <span class="control">
          <input type="number" min="0" step="any" bind:value={settings.recover_margin_gb} />
          <span class="unit">GB</span>
        </span>
      </div>
      <div class="field">
        <span>Wait before alerting again</span>
        <span class="control">
          <input type="number" min="0" bind:value={settings.cooldown_minutes} />
          <span class="unit">minutes</span>
        </span>
      </div>
      <div class="field">
        <span>Check free space every</span>
        <span class="control">
          <input type="number" min="1" bind:value={settings.poll_minutes} />
          <span class="unit">minutes</span>
        </span>
      </div>
    </section>

    <section class="panel">
      <h2>Folders to clean</h2>
      <p class="muted">Picard only ever removes things inside these folders.</p>
      {#each settings.roots as _, i}
        <div class="row">
          <input class="wide path" bind:value={settings.roots[i]} placeholder="~/working" />
          <button onclick={() => settings!.roots.splice(i, 1)}>Remove</button>
        </div>
      {/each}
      <button onclick={addFolder}>Add folder…</button>

      <h2 class="sub">Never touch (glob patterns)</h2>
      {#each settings.exclude as _, i}
        <div class="row">
          <input class="wide path" bind:value={settings.exclude[i]} placeholder="**/keep-*/**" />
          <button onclick={() => settings!.exclude.splice(i, 1)}>Remove</button>
        </div>
      {/each}
      <button onclick={() => settings!.exclude.push('')}>Add pattern</button>
    </section>

    <section class="panel">
      <h2>Cleanup tasks</h2>
      <p class="muted">Run top to bottom. Picard stops once enough space is free.</p>
      {#each settings.tasks as task, i}
        <div class="task">
          <div class="row">
            <label class="row"
              ><input type="checkbox" bind:checked={task.enabled} />
              <strong>{TASK_LABELS[task.type]}</strong></label
            >
            <span class="spacer"></span>
            <button onclick={() => move(i, -1)} disabled={i === 0} aria-label="Move up">↑</button>
            <button
              onclick={() => move(i, 1)}
              disabled={i === settings!.tasks.length - 1}
              aria-label="Move down">↓</button
            >
            {#if task.type === 'command'}
              <button onclick={() => settings!.tasks.splice(i, 1)}>Remove</button>
            {/if}
          </div>
          <div class="opts">
            {#if task.type === 'git_worktrees'}
              <div class="field">
                <span>Merged worktrees, once idle for</span>
                <span class="control">
                  <input type="number" min="0" bind:value={task.min_idle_minutes} />
                  <span class="unit">minutes</span>
                </span>
              </div>
              <div class="field">
                <span>Other worktrees, once idle for</span>
                <span class="control">
                  <input type="number" min="0" bind:value={task.stale_days} />
                  <span class="unit">days</span>
                </span>
              </div>
            {:else if task.type === 'node_modules'}
              <div class="field">
                <span>Projects untouched for</span>
                <span class="control">
                  <input type="number" min="0" bind:value={task.stale_days} />
                  <span class="unit">days</span>
                </span>
              </div>
              <label class="field check">
                <span>Also prune npm and pnpm caches</span>
                <span class="control">
                  <input type="checkbox" bind:checked={task.prune_package_caches} />
                </span>
              </label>
            {:else if task.type === 'cargo_clean'}
              <div class="field">
                <span>Clean</span>
                <span class="control">
                  <select bind:value={task.mode}>
                    <option value="stale">Idle projects</option>
                    <option value="all">Every project</option>
                  </select>
                </span>
              </div>
              {#if task.mode === 'stale'}
                <div class="field">
                  <span>Projects untouched for</span>
                  <span class="control">
                    <input type="number" min="0" bind:value={task.stale_days} />
                    <span class="unit">days</span>
                  </span>
                </div>
              {/if}
            {:else if task.type === 'python_venv'}
              <div class="field">
                <span>Projects untouched for</span>
                <span class="control">
                  <input type="number" min="0" bind:value={task.stale_days} />
                  <span class="unit">days</span>
                </span>
              </div>
            {:else if task.type === 'build_output'}
              <div class="field">
                <span>Projects untouched for</span>
                <span class="control">
                  <input type="number" min="0" bind:value={task.stale_days} />
                  <span class="unit">days</span>
                </span>
              </div>
              <div class="field">
                <span>Folder names</span>
                <input
                  class="wide path"
                  bind:value={
                    () => task.dirs.join(', '),
                    (v: string) =>
                      (task.dirs = v
                        .split(',')
                        .map((d) => d.trim())
                        .filter(Boolean))
                  }
                  placeholder="dist, build, .next, vendor"
                />
              </div>
              <p class="muted">Only folders git ignores and tracks nothing in.</p>
            {:else if task.type === 'tool_caches'}
              <p class="muted">Runs each tool's own prune command. Missing tools are skipped.</p>
              <label class="field check">
                <span>uv (uv cache prune)</span>
                <span class="control"><input type="checkbox" bind:checked={task.uv} /></span>
              </label>
              <label class="field check">
                <span>Homebrew (brew cleanup --prune=all)</span>
                <span class="control"><input type="checkbox" bind:checked={task.homebrew} /></span>
              </label>
              <label class="field check">
                <span>Unavailable iOS Simulators</span>
                <span class="control"><input type="checkbox" bind:checked={task.simulators} /></span>
              </label>
              <label class="field check">
                <span>bun (bun pm cache rm)</span>
                <span class="control"><input type="checkbox" bind:checked={task.bun} /></span>
              </label>
            {:else if task.type === 'command'}
              <div class="field">
                <span>Name</span>
                <input class="wide" bind:value={task.name} />
              </div>
              <div class="field">
                <span>Command</span>
                <input class="wide path" bind:value={task.run} placeholder="docker system prune -f" />
              </div>
              <div class="field">
                <span>Time limit</span>
                <span class="control">
                  <input type="number" min="1" bind:value={task.timeout_secs} />
                  <span class="unit">seconds</span>
                </span>
              </div>
            {/if}
          </div>
        </div>
      {/each}
      <div class="row">
        <button onclick={() => settings!.tasks.push(newTask('command'))}>Add custom command</button>
      </div>
    </section>

    <section class="panel">
      <label class="row">
        <input
          type="checkbox"
          checked={launchAtLogin}
          onchange={(e) => toggleLogin(e.currentTarget.checked)}
        />
        Open Picard when you log in
      </label>
    </section>

    <div class="row footer">
      {#if confirmingClose}
        <span>You have unsaved changes.</span>
        <span class="spacer"></span>
        <button onclick={() => (confirmingClose = false)}>Keep editing</button>
        <button onclick={() => close(true)}>Discard and close</button>
      {:else}
        {#if saveError}<span class="error">{saveError}</span>{/if}
        {#if saved && !dirty}<span class="ok">Saved. Picard has reloaded the settings.</span>{/if}
        <span class="spacer"></span>
        <button onclick={() => close()}>Close</button>
        <button class="primary" onclick={save} disabled={saving}
          >{saving ? 'Saving…' : 'Save'}</button
        >
      {/if}
    </div>
  {/if}
</main>

<style>
  section .row {
    margin: 6px 0;
  }
  /* One option per row: label on the left, input and unit kept together on the right. */
  .field {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    column-gap: 12px;
    margin: 6px 0;
  }
  .field .wide {
    width: 100%;
    min-width: 0;
  }
  .field:has(.wide) {
    grid-template-columns: 120px minmax(0, 1fr);
  }
  .control {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    justify-self: end;
  }
  .unit {
    color: var(--muted);
    min-width: 52px;
  }
  .check {
    cursor: pointer;
  }
  .choice {
    display: block;
    margin: 6px 0;
  }
  .wide {
    flex: 1;
    min-width: 240px;
  }
  .sub {
    margin-top: 14px;
  }
  .task {
    border-top: 1px solid var(--line);
    padding: 8px 0;
  }
  .task:first-of-type {
    border-top: none;
  }
  .opts {
    margin-left: 24px;
  }
  .footer {
    margin-top: 14px;
    position: sticky;
    bottom: 0;
    background: var(--bg);
    padding: 10px 0;
  }
</style>
