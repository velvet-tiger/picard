<script lang="ts">
  // The window cannot open web pages itself (see capabilities/default.json),
  // so each link is a button that asks Rust to open a page by name.
  import {
    appVersion,
    checkForUpdate,
    errorText,
    openLink,
    type Link,
    type UpdateCheck,
  } from './api'

  let version = $state<string | null>(null)
  let versionError = $state<string | null>(null)
  let checking = $state(true)
  let update = $state<UpdateCheck | null>(null)
  let updateError = $state<string | null>(null)
  let linkError = $state<string | null>(null)

  async function loadVersion() {
    try {
      version = await appVersion()
    } catch (e) {
      versionError = errorText(e)
    }
  }

  async function check() {
    checking = true
    update = null
    updateError = null
    try {
      update = await checkForUpdate()
    } catch (e) {
      updateError = errorText(e)
    } finally {
      checking = false
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

  // The tab mounts the first time it is shown, so GitHub is only asked then.
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
      {#if checking}
        <span class="muted">Checking GitHub for a newer version…</span>
      {:else if update?.newer}
        <span>Version {update.latest} is available.</span>
        <button class="link" onclick={() => open('latest_release')}>Download it</button>
      {:else if update}
        <span class="ok">You're on the latest version.</span>
      {:else if updateError}
        <span class="error">Could not check: {updateError}</span>
      {/if}
      <span class="spacer"></span>
      <button onclick={check} disabled={checking}>Check again</button>
    </div>
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
