import { fireEvent, render, screen } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import About from './About.svelte'
import type { UpdateCheck } from './api'
import { deferred, fakeBackend, type Handler } from './fake-backend'

const upToDate: UpdateCheck = { current: '0.1.0', latest: '0.1.0', newer: false }

function backend(handlers: Record<string, Handler> = {}) {
  return fakeBackend({
    'plugin:app|version': () => '0.1.0',
    check_for_update: () => upToDate,
    open_link: () => null,
    ...handlers,
  })
}

describe('About', () => {
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

  it('clears an earlier result while checking again', async () => {
    const second = deferred<UpdateCheck>()
    let calls = 0
    backend({
      check_for_update: () => {
        calls += 1
        return calls === 1 ? { current: '0.1.0', latest: '0.2.0', newer: true } : second.promise
      },
    })
    render(About)
    expect(await screen.findByText('Version 0.2.0 is available.')).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Check again' }))
    expect(await screen.findByText(/Checking GitHub/)).toBeInTheDocument()
    expect(screen.queryByText('Version 0.2.0 is available.')).not.toBeInTheDocument()
    second.resolve(upToDate)
    expect(await screen.findByText("You're on the latest version.")).toBeInTheDocument()
  })

  it('says so while the check runs, then when the version is the latest', async () => {
    const reply = deferred<UpdateCheck>()
    backend({ check_for_update: () => reply.promise })
    render(About)
    expect(await screen.findByText(/Checking GitHub/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeDisabled()
    reply.resolve(upToDate)
    expect(await screen.findByText("You're on the latest version.")).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Check again' })).toBeEnabled()
  })

  it('offers the release page when a newer version is out', async () => {
    const api = backend({
      check_for_update: () => ({ current: '0.1.0', latest: '0.2.0', newer: true }),
    })
    render(About)
    expect(await screen.findByText('Version 0.2.0 is available.')).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Download it' }))
    expect(api.sent('open_link')).toEqual([{ link: 'latest_release' }])
  })

  it('shows why the check failed, and checks again when asked', async () => {
    let calls = 0
    const api = backend({
      check_for_update: () => {
        calls += 1
        if (calls === 1) throw "GitHub's rate limit was reached. Try again later."
        return upToDate
      },
    })
    render(About)
    expect(
      await screen.findByText("Could not check: GitHub's rate limit was reached. Try again later."),
    ).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Check again' }))
    expect(await screen.findByText("You're on the latest version.")).toBeInTheDocument()
    expect(api.sent('check_for_update')).toHaveLength(2)
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
