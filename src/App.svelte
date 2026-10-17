<script lang="ts">
  // One window holds both views as tabs. The URL hash (#/clean or #/settings),
  // set by src-tauri/src/windows.rs, picks the first tab. When the window is
  // already open, the menu bar sends a `show-tab` event instead.
  import Preview from './lib/Preview.svelte'
  import SettingsView from './lib/Settings.svelte'
  import { closeWindow, onShowTab } from './lib/api'

  const TABS = [
    { id: 'clean', label: 'Clean' },
    { id: 'settings', label: 'Settings' },
  ] as const
  type TabId = (typeof TABS)[number]['id']

  const isTab = (id: string): id is TabId => TABS.some((t) => t.id === id)

  const initial = window.location.hash.replace('#/', '')
  const firstTab: TabId | null = isTab(initial) ? initial : null
  let active = $state<TabId | null>(firstTab)
  // A tab mounts the first time it is shown and then stays mounted, so the
  // Clean scan only starts when asked for and switching tabs keeps its state.
  let opened = $state<Record<TabId, boolean>>({
    clean: firstTab === 'clean',
    settings: firstTab === 'settings',
  })
  let settingsView = $state<SettingsView | undefined>()

  function show(id: TabId) {
    active = id
    opened[id] = true
  }

  $effect(() => {
    const unlisten = onShowTab((id) => {
      if (isTab(id)) show(id)
      else console.error(`show-tab: unknown tab "${id}"`)
    })
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  // A menu bar app has no Window menu, so ⌘W does nothing unless handled here.
  // Unsaved settings must not be lost from the Clean tab either, so the Settings
  // view decides whether the window may close.
  function onKey(e: KeyboardEvent) {
    if (e.key !== 'Escape' && !(e.metaKey && e.key.toLowerCase() === 'w')) return
    e.preventDefault()
    if (!settingsView) {
      closeWindow().catch((err) => console.error('close window', err))
      return
    }
    if (settingsView.dismissCloseConfirm()) return
    if (settingsView.hasUnsavedChanges()) show('settings')
    settingsView.close()
  }
</script>

<svelte:window onkeydown={onKey} />

<!-- The macOS title bar is hidden (src-tauri/src/windows.rs), so this strip is
     what the window is dragged by. Its height leaves room for the traffic lights. -->
<header class="titlebar" data-tauri-drag-region>
  {#if active !== null}
    <div class="tabs" role="tablist" aria-label="Picard">
      {#each TABS as t}
        <button
          role="tab"
          id="tab-{t.id}"
          aria-selected={active === t.id}
          aria-controls="panel-{t.id}"
          class:selected={active === t.id}
          onclick={() => show(t.id)}>{t.label}</button
        >
      {/each}
    </div>
  {/if}
</header>

{#if active === null}
  <main><p>Unknown view “{initial}”.</p></main>
{:else}
  {#if opened.clean}
    <div role="tabpanel" id="panel-clean" aria-labelledby="tab-clean" hidden={active !== 'clean'}>
      <Preview />
    </div>
  {/if}
  {#if opened.settings}
    <div
      role="tabpanel"
      id="panel-settings"
      aria-labelledby="tab-settings"
      hidden={active !== 'settings'}
    >
      <SettingsView bind:this={settingsView} />
    </div>
  {/if}
{/if}

<style>
  .titlebar {
    position: sticky;
    top: 0;
    z-index: 10;
    height: 40px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg);
  }
  /* Two segments that join into one pill. */
  .tabs {
    display: flex;
    gap: 3px;
  }
  .tabs button {
    height: 24px;
    padding: 0 20px;
    border-radius: 0;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    background: var(--lc-lilac);
  }
  .tabs button:first-child {
    border-radius: 12px 0 0 12px;
  }
  .tabs button:last-child {
    border-radius: 0 12px 12px 0;
  }
  .tabs button.selected {
    background: var(--lc-sky);
  }
</style>
