<script lang="ts">
  import {
    getSettings,
    saveSettings,
    getLaunchAtLogin,
    setLaunchAtLogin,
    errorText,
    newTask,
    TASK_LABELS,
    type Settings,
    type TaskType,
  } from './api'

  let settings = $state<Settings | null>(null)
  let launchAtLogin = $state(false)
  let loadError = $state<string | null>(null)
  let saveError = $state<string | null>(null)
  let saved = $state(false)
  let saving = $state(false)
  let addType = $state<TaskType>('git_worktrees')

  const MODES = [
    { value: 'ask', label: 'Ask', help: 'Notify and wait for you to choose Clean.' },
    { value: 'auto', label: 'Auto', help: 'Clean straight away, then tell you what was freed.' },
    { value: 'off', label: 'Off', help: 'Only show free space.' },
  ] as const

  async function load() {
    try {
      settings = await getSettings()
      launchAtLogin = await getLaunchAtLogin()
    } catch (e) {
      loadError = errorText(e)
    }
  }

  async function save() {
    if (!settings) return
    saving = true
    saveError = null
    saved = false
    try {
      await saveSettings($state.snapshot(settings))
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
      <h2>Threshold</h2>
      <div class="row">
        <span>Low when free space drops below</span>
        <input type="number" min="0" step="any" bind:value={settings.threshold.value} />
        <select
          value={settings.threshold.unit}
          onchange={(e) => setUnit(e.currentTarget.value as 'gb' | 'pct')}
        >
          <option value="gb">GB</option>
          <option value="pct">%</option>
        </select>
      </div>
      <div class="row">
        <span>Stop cleaning once this much more is free (GB)</span>
        <input type="number" min="0" step="any" bind:value={settings.recover_margin_gb} />
      </div>
      <div class="row">
        <span>Wait before alerting again (minutes)</span>
        <input type="number" min="0" bind:value={settings.cooldown_minutes} />
      </div>
      <div class="row">
        <span>Check every (minutes)</span>
        <input type="number" min="1" bind:value={settings.poll_minutes} />
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
      <button onclick={() => settings!.roots.push('')}>Add folder</button>

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
            <button onclick={() => settings!.tasks.splice(i, 1)}>Remove</button>
          </div>
          {#if task.type === 'git_worktrees'}
            <div class="row opts">
              <span>Remove merged worktrees idle for</span>
              <input type="number" min="0" bind:value={task.min_idle_minutes} /> <span>minutes;
                others idle for</span>
              <input type="number" min="0" bind:value={task.stale_days} /> <span>days</span>
            </div>
          {:else if task.type === 'node_modules'}
            <div class="row opts">
              <span>Projects untouched for</span>
              <input type="number" min="0" bind:value={task.stale_days} /> <span>days</span>
              <label class="row"
                ><input type="checkbox" bind:checked={task.prune_package_caches} /> Also prune npm
                and pnpm caches</label
              >
            </div>
          {:else if task.type === 'cargo_clean'}
            <div class="row opts">
              <select bind:value={task.mode}>
                <option value="stale">Only projects untouched for</option>
                <option value="all">Every project</option>
              </select>
              {#if task.mode === 'stale'}
                <input type="number" min="0" bind:value={task.stale_days} /> <span>days</span>
              {/if}
            </div>
          {:else if task.type === 'command'}
            <div class="opts">
              <div class="row">
                <span>Name</span><input class="wide" bind:value={task.name} />
              </div>
              <div class="row">
                <span>Command</span><input
                  class="wide path"
                  bind:value={task.run}
                  placeholder="docker system prune -f"
                />
              </div>
              <div class="row">
                <span>Time limit (seconds)</span><input
                  type="number"
                  min="1"
                  bind:value={task.timeout_secs}
                />
              </div>
            </div>
          {/if}
        </div>
      {/each}
      <div class="row">
        <select bind:value={addType}>
          {#each Object.entries(TASK_LABELS) as [value, label]}
            <option {value}>{label}</option>
          {/each}
        </select>
        <button onclick={() => settings!.tasks.push(newTask(addType))}>Add task</button>
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
      {#if saveError}<span class="error">{saveError}</span>{/if}
      {#if saved}<span class="ok">Saved. Picard has reloaded the settings.</span>{/if}
      <span class="spacer"></span>
      <button class="primary" onclick={save} disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
    </div>
  {/if}
</main>

<style>
  section .row {
    margin: 6px 0;
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
