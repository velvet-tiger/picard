<script lang="ts">
  // One window holds every view as a tab. The URL hash (#/clean or #/settings),
  // set by src-tauri/src/windows.rs, picks the first tab and then tracks the
  // active one. When the window is
  // already open, the menu bar sends a `show-tab` event instead.
  import Preview from './lib/Preview.svelte'
  import SettingsView from './lib/Settings.svelte'
  import About from './lib/About.svelte'
  import { closeWindow, onShowTab } from './lib/api'

  const TABS = [
    { id: 'clean', label: 'Clean' },
    { id: 'settings', label: 'Settings' },
    { id: 'about', label: 'About' },
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
    about: firstTab === 'about',
  })
  let settingsView = $state<SettingsView | undefined>()

  // The hash follows the active tab so a page reload (for example the dev
  // server's) reopens the tab the user was on, not the one the window opened on.
  function show(id: TabId) {
    active = id
    opened[id] = true
    history.replaceState(null, '', `#/${id}`)
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

{#if active === null}
  <main><p>Unknown view “{initial}”.</p></main>
{:else}
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
  {#if opened.about}
    <div role="tabpanel" id="panel-about" aria-labelledby="tab-about" hidden={active !== 'about'}>
      <About />
    </div>
  {/if}
{/if}

<style>
  .tabs {
    display: flex;
    justify-content: center;
    gap: 2px;
    width: fit-content;
    margin: 12px auto 0;
    padding: 2px;
    border: 1px solid var(--line);
    border-radius: 7px;
    background: var(--panel);
  }
  .tabs button {
    border: none;
    border-radius: 5px;
    padding: 3px 18px;
    background: transparent;
    color: var(--text);
  }
  .tabs button.selected {
    background: var(--accent);
    color: #fff;
  }
</style>
