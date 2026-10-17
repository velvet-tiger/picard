<script lang="ts">
  // Both windows load this page; the URL hash (#/preview or #/settings),
  // set by src-tauri/src/windows.rs, picks the view.
  import Preview from './lib/Preview.svelte'
  import SettingsView from './lib/Settings.svelte'

  const view = window.location.hash.replace('#/', '')
</script>

{#if view === 'preview'}
  <Preview />
{:else if view === 'settings'}
  <SettingsView />
{:else}
  <main><p>Unknown view “{view}”.</p></main>
{/if}
