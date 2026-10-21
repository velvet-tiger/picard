import { cleanup, fireEvent, render, screen } from '@testing-library/svelte'
import { mockWindows } from '@tauri-apps/api/mocks'
import { emit } from '@tauri-apps/api/event'
import { afterEach, describe, expect, it } from 'vitest'
import About from './About.svelte'
import type { UpdateStatus } from './api'
import { deferred, fakeBackend, type Handler } from './fake-backend'

const upToDate: UpdateStatus = { state: 'up_to_date' }
const available: UpdateStatus = { state: 'available', version: '0.2.0' }
const ready: UpdateStatus = { state: 'ready', version: '0.2.0' }

function backend(handlers: Record<string, Handler> = {}) {
  mockWindows('main')
  return fakeBackend({
    'plugin:app|version': () => '0.1.0',
    check_for_update: () => upToDate,
    install_update: () => ready,
    restart_to_update: () => null,
    open_link: () => null,
    ...handlers,
  })
}

describe('About', () => {
  // The tab stops listening for `update-status` when it unmounts. That needs
  // the Tauri mocks, which test-setup.ts clears in an afterEach that runs after this one.
  afterEach(async () => {
    cleanup()
    await new Promise((settle) => setTimeout(settle))
  })

  it('shows the running version', async () => {
    backend()
    render(About)
    expect(await screen.findByText('Version 0.1.0')).toBeInTheDocument()
  })

  it('shows why the version could not be read', async () => {
    backend({
      'plugin:app|version': () => {
        throw 'no package info'
      },
    })
    render(About)
    expect(
      await screen.findByText('Could not read the version: no package info'),
    ).toBeInTheDocument()
  })

  it('says so while the check runs, then when the version is the latest', async () => {
    const reply = deferred<UpdateStatus>()
    backend({ check_for_update: () => reply.promise })
    render(About)
    expect(await screen.findByText(/Checking for a newer version/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeDisabled()
    reply.resolve(upToDate)
    expect(await screen.findByText("You're on the latest version.")).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeEnabled()
  })

  it('clears an earlier result while checking again', async () => {
    const second = deferred<UpdateStatus>()
    let calls = 0
    backend({
      check_for_update: () => {
        calls += 1
        return calls === 1 ? available : second.promise
      },
    })
    render(About)
    expect(await screen.findByText('Version 0.2.0 is available.')).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Check again' }))
    expect(await screen.findByText(/Checking for a newer version/)).toBeInTheDocument()
    expect(screen.queryByText('Version 0.2.0 is available.')).not.toBeInTheDocument()
    second.resolve(upToDate)
    expect(await screen.findByText("You're on the latest version.")).toBeInTheDocument()
  })

  it('installs an offered version when asked', async () => {
    const api = backend({ check_for_update: () => available })
    render(About)
    await fireEvent.click(await screen.findByRole('button', { name: 'Install it' }))
    expect(await screen.findByText('Version 0.2.0 is installed.')).toBeInTheDocument()
    expect(api.sent('install_update')).toHaveLength(1)
  })

  it('offers a restart once the update is installed, and restarts when asked', async () => {
    const api = backend({ check_for_update: () => ready })
    render(About)
    expect(await screen.findByText('Version 0.2.0 is installed.')).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Restart to finish updating' }))
    expect(api.sent('restart_to_update')).toHaveLength(1)
  })

  it('shows why a restart was refused', async () => {
    backend({
      check_for_update: () => ready,
      restart_to_update: () => {
        throw 'Picard is cleaning. Restart it when the cleanup has finished.'
      },
    })
    render(About)
    await fireEvent.click(await screen.findByRole('button', { name: 'Restart to finish updating' }))
    expect(
      await screen.findByText('Picard is cleaning. Restart it when the cleanup has finished.'),
    ).toBeInTheDocument()
  })

  it('follows a download that the background check started', async () => {
    backend()
    render(About)
    await screen.findByText("You're on the latest version.")

    await emit('update-status', { state: 'downloading', version: '0.2.0' })
    expect(await screen.findByText('Downloading version 0.2.0…')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeDisabled()

    await emit('update-status', ready)
    expect(await screen.findByText('Version 0.2.0 is installed.')).toBeInTheDocument()
  })

  it('keeps a newer event over a reply that is still in progress', async () => {
    const reply = deferred<UpdateStatus>()
    backend({ check_for_update: () => reply.promise })
    render(About)
    await screen.findByText(/Checking for a newer version/)

    await emit('update-status', ready)
    await screen.findByText('Version 0.2.0 is installed.')
    reply.resolve({ state: 'downloading', version: '0.2.0' })

    await new Promise((settle) => setTimeout(settle))
    expect(screen.getByText('Version 0.2.0 is installed.')).toBeInTheDocument()
  })

  it('uses a finished reply even when an event arrived first, so it cannot stay on Checking', async () => {
    const reply = deferred<UpdateStatus>()
    backend({ check_for_update: () => reply.promise })
    render(About)
    await screen.findByText(/Checking for a newer version/)

    await emit('update-status', { state: 'checking' })
    reply.resolve(upToDate)

    expect(await screen.findByText("You're on the latest version.")).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeEnabled()
  })

  it('words a failed check apart from a failed install, and links the download only for the install', async () => {
    let calls = 0
    const api = backend({
      check_for_update: () => {
        calls += 1
        return calls === 1
          ? { state: 'failed', message: 'error sending request', stage: 'check' }
          : available
      },
      install_update: () => ({
        state: 'failed',
        message: 'The signature verification failed (version 0.2.0)',
        stage: 'install',
      }),
    })
    render(About)
    expect(
      await screen.findByText('Could not check for updates: error sending request'),
    ).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Download it instead' })).not.toBeInTheDocument()

    await fireEvent.click(screen.getByRole('button', { name: 'Check again' }))
    await fireEvent.click(await screen.findByRole('button', { name: 'Install it' }))
    expect(
      await screen.findByText(
        'Could not install the update: The signature verification failed (version 0.2.0)',
      ),
    ).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Download it instead' }))
    expect(api.sent('open_link')).toEqual([{ link: 'latest_release' }])
    expect(api.sent('check_for_update')).toHaveLength(2)
  })

  it('shows an error when asking to check or install is rejected', async () => {
    backend({
      check_for_update: () => {
        throw 'background task failed'
      },
    })
    render(About)
    expect(await screen.findByText('background task failed')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeEnabled()
  })

  it('opens the website and the issue tracker by name', async () => {
    const api = backend()
    render(About)
    await fireEvent.click(await screen.findByRole('button', { name: 'Velvet Tiger' }))
    await fireEvent.click(screen.getByRole('button', { name: 'Report an issue on GitHub' }))
    expect(api.sent('open_link')).toEqual([{ link: 'website' }, { link: 'issues' }])
  })

  it('shows an error when a link cannot be opened', async () => {
    backend({
      open_link: () => {
        throw 'could not open https://velvettiger.com.au: no such file'
      },
    })
    render(About)
    await fireEvent.click(await screen.findByRole('button', { name: 'Velvet Tiger' }))
    expect(
      await screen.findByText('could not open https://velvettiger.com.au: no such file'),
    ).toBeInTheDocument()
  })
})
