<script lang="ts">
  // The window cannot open web pages itself (see capabilities/default.json),
  // so each link is a button that asks Rust to open a page by name.
  import {
    appVersion,
    checkForUpdate,
    errorText,
    installUpdate,
    onUpdateStatus,
    openLink,
    restartToUpdate,
    type Link,
    type UpdateStatus,
  } from './api'

  let version = $state<string | null>(null)
  let versionError = $state<string | null>(null)
  let status = $state<UpdateStatus | null>(null)
  let asking = $state(true)
  let actionError = $state<string | null>(null)
  let linkError = $state<string | null>(null)
  // Counts the status events received. A reply that is still in progress
  // (checking or downloading) is only used if no event arrived while it was on
  // its way, because the event is newer. A finished reply is always used, so a
  // lost event cannot leave the tab on "Checking".
  let eventCount = 0

  const working = $derived(
    asking || status?.state === 'checking' || status?.state === 'downloading',
  )

  async function loadVersion() {
    try {
      version = await appVersion()
    } catch (e) {
      versionError = errorText(e)
    }
  }

  async function ask(request: () => Promise<UpdateStatus>) {
    asking = true
    actionError = null
    const before = eventCount
    try {
      const reply = await request()
      const finished = reply.state !== 'checking' && reply.state !== 'downloading'
      if (finished || eventCount === before) status = reply
    } catch (e) {
      actionError = errorText(e)
    } finally {
      asking = false
    }
  }

  function check() {
    status = null
    return ask(checkForUpdate)
  }

  async function restart() {
    actionError = null
    try {
      await restartToUpdate()
    } catch (e) {
      actionError = errorText(e)
    }
  }

  async function open(link: Link) {
    linkError = null
    try {
      await openLink(link)
    } catch (e) {
      linkError = errorText(e)
    }
  }

  // The background check can finish while this tab is open.
  $effect(() => {
    const unlisten = onUpdateStatus((next) => {
      eventCount += 1
      status = next
    })
    return () => {
      unlisten.then((stop) => stop())
    }
  })

  // The tab mounts the first time it is shown, so the server is only asked then.
  loadVersion()
  check()
</script>

<main>
  <h1>About Picard</h1>
  <p class="muted">
    {#if version}Version {version}{:else if versionError}<span class="error"
        >Could not read the version: {versionError}</span
      >{/if}
  </p>

  <section class="panel">
    <h2>Updates</h2>
    <div class="row status">
      {#if status?.state === 'downloading'}
        <span class="muted">Downloading version {status.version}…</span>
      {:else if status?.state === 'ready'}
        <span>Version {status.version} is installed.</span>
        <button class="link" onclick={restart}>Restart to finish updating</button>
      {:else if status?.state === 'available'}
        <span>Version {status.version} is available.</span>
        <button class="link" onclick={() => ask(installUpdate)} disabled={working}>Install it</button
        >
      {:else if status?.state === 'failed'}
        <span class="error"
          >{status.stage === 'check' ? 'Could not check for updates' : 'Could not install the update'}:
          {status.message}</span
        >
        {#if status.stage === 'install'}
          <button class="link" onclick={() => open('latest_release')}>Download it instead</button>
        {/if}
      {:else if working}
        <span class="muted">Checking for a newer version…</span>
      {:else if status?.state === 'up_to_date'}
        <span class="ok">You're on the latest version.</span>
      {/if}
      <span class="spacer"></span>
      <button onclick={check} disabled={working}>Check again</button>
    </div>
    {#if actionError}<p class="error">{actionError}</p>{/if}
  </section>

  <section class="panel">
    <h2>Links</h2>
    <ul>
      <li>
        Made by <button class="link" onclick={() => open('website')}>Velvet Tiger</button>
      </li>
      <li>
        Found a bug or want something? <button class="link" onclick={() => open('issues')}
          >Report an issue on GitHub</button
        >
      </li>
    </ul>
    {#if linkError}<p class="error">{linkError}</p>{/if}
  </section>
</main>

<style>
  .status {
    margin-top: 8px;
  }
  ul {
    margin: 8px 0 0;
    padding-left: 18px;
  }
  li + li {
    margin-top: 4px;
  }
  button.link {
    border: none;
    padding: 0;
    background: none;
    color: var(--accent);
    text-decoration: underline;
  }
</style>
