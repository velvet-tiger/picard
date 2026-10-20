import { fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { mockWindows } from '@tauri-apps/api/mocks'
import { describe, expect, it } from 'vitest'
import SettingsView from './Settings.svelte'
import { BUILT_IN_TASKS, TASK_LABELS, type Settings } from './api'
import { fakeBackend, type Handler } from './fake-backend'

function config(): Settings {
  return {
    poll_minutes: 5,
    threshold: { unit: 'gb', value: 30 },
    recover_margin_gb: 8,
    cooldown_minutes: 60,
    mode: 'ask',
    daily_scan: '05:30',
    roots: ['~/working'],
    exclude: [],
    tasks: [{ type: 'node_modules', enabled: true, stale_days: 30, prune_package_caches: false }],
  }
}

/** Replies for loading and saving `config()`. `handlers` override them per command. */
function backend(handlers: Record<string, Handler> = {}) {
  let ignored = ['~/keep/this', '~/and/that']
  mockWindows('main')
  return fakeBackend({
    get_settings: () => config(),
    get_launch_at_login: () => false,
    get_ignored: () => ignored,
    save_settings: () => null,
    set_ignored: (args) => {
      ignored = ignored.filter((p) => p !== args.path)
      return null
    },
    'plugin:window|close': () => null,
    ...handlers,
  })
}

async function renderLoaded() {
  const view = render(SettingsView)
  await screen.findByText('When space runs low')
  return view.component
}

const saveButton = () => screen.getByRole('button', { name: 'Save' })
const dailyBox = () => screen.getByRole('checkbox', { name: /Also scan once a day/ })
const timeInput = () => document.querySelector<HTMLInputElement>('input[type="time"]')

describe('Settings', () => {
  it('shows the load error', async () => {
    backend({
      get_settings: () => {
        throw 'config.toml: invalid threshold'
      },
    })
    render(SettingsView)
    expect(await screen.findByText('config.toml: invalid threshold')).toBeInTheDocument()
  })

  it('lists every built-in task without counting that as an unsaved edit', async () => {
    backend()
    const view = await renderLoaded()

    for (const type of BUILT_IN_TASKS) {
      expect(screen.getByText(TASK_LABELS[type])).toBeInTheDocument()
    }
    expect(view.hasUnsavedChanges()).toBe(false)
  })

  it('drops blank rows and trims folders when saving', async () => {
    const { sent } = backend()
    const view = await renderLoaded()

    await fireEvent.input(screen.getByDisplayValue('~/working'), {
      target: { value: '  ~/projects  ' },
    })
    await fireEvent.click(screen.getByRole('button', { name: 'Add pattern' }))
    expect(view.hasUnsavedChanges()).toBe(true)

    await fireEvent.click(saveButton())

    expect(
      await screen.findByText('Saved. Picard has reloaded the settings.'),
    ).toBeInTheDocument()
    expect(sent('save_settings')).toEqual([
      { settings: expect.objectContaining({ roots: ['~/projects'], exclude: [] }) },
    ])
    expect(view.hasUnsavedChanges()).toBe(false)
  })

  it('shows the save error and keeps the edits', async () => {
    backend({
      save_settings: () => {
        throw 'roots: ~/nope does not exist'
      },
    })
    const view = await renderLoaded()

    await fireEvent.click(screen.getByRole('button', { name: 'Add pattern' }))
    await fireEvent.click(saveButton())

    expect(await screen.findByText('roots: ~/nope does not exist')).toBeInTheDocument()
    expect(view.hasUnsavedChanges()).toBe(true)
  })

  it('restores the last daily scan time when the box is ticked again', async () => {
    backend()
    await renderLoaded()

    await fireEvent.click(dailyBox())
    expect(timeInput()).toBeNull()

    await fireEvent.click(dailyBox())
    expect(timeInput()?.value).toBe('05:30')
  })

  it('blocks Save while the daily scan time is blank', async () => {
    backend()
    await renderLoaded()

    const input = timeInput()
    if (!input) throw new Error('no time input')
    await fireEvent.input(input, { target: { value: '' } })

    expect(saveButton()).toBeDisabled()
    expect(screen.getByText('Pick a time, or untick the daily scan.')).toBeInTheDocument()
  })

  it('resets the threshold value when the unit changes', async () => {
    backend()
    await renderLoaded()

    await fireEvent.change(screen.getByDisplayValue('GB'), { target: { value: 'pct' } })

    expect(screen.getByDisplayValue('10')).toBeInTheDocument()
  })

  it('closes at once when nothing has changed', async () => {
    const { sent } = backend()
    const view = await renderLoaded()

    await view.close()

    expect(sent('plugin:window|close')).toHaveLength(1)
  })

  it('asks before closing with unsaved edits, then closes on Discard', async () => {
    const { sent } = backend()
    await renderLoaded()

    await fireEvent.click(screen.getByRole('button', { name: 'Add pattern' }))
    await fireEvent.click(screen.getByRole('button', { name: 'Close' }))

    expect(await screen.findByText('You have unsaved changes.')).toBeInTheDocument()
    expect(sent('plugin:window|close')).toHaveLength(0)

    await fireEvent.click(screen.getByRole('button', { name: 'Discard and close' }))
    await waitFor(() => expect(sent('plugin:window|close')).toHaveLength(1))
  })

  it('reports whether it hid the unsaved-changes bar', async () => {
    backend()
    const view = await renderLoaded()

    expect(view.dismissCloseConfirm()).toBe(false)
    await fireEvent.click(screen.getByRole('button', { name: 'Add pattern' }))
    await view.close()
    expect(view.dismissCloseConfirm()).toBe(true)
    await waitFor(() => expect(screen.queryByText('You have unsaved changes.')).toBeNull())
  })

  it('removes an ignored folder at once and reloads the list', async () => {
    const { sent } = backend()
    await renderLoaded()
    await screen.findByText('~/keep/this')

    const row = screen.getByText('~/keep/this').closest('.row')
    const remove = row?.querySelector('button')
    if (!remove) throw new Error('no Remove button on the ignored folder row')
    await fireEvent.click(remove)

    await waitFor(() => expect(screen.queryByText('~/keep/this')).toBeNull())
    expect(screen.getByText('~/and/that')).toBeInTheDocument()
    expect(sent('set_ignored')).toEqual([{ path: '~/keep/this', ignored: false }])
  })

  it('turns on Open at login', async () => {
    const { sent } = backend({ set_launch_at_login: () => null })
    await renderLoaded()
    const box = screen.getByRole('checkbox', { name: 'Open Picard when you log in' })

    await fireEvent.click(box)

    await waitFor(() => expect(sent('set_launch_at_login')).toEqual([{ enabled: true }]))
  })

  it('unticks Open at login again when the change fails', async () => {
    backend({
      set_launch_at_login: () => {
        throw 'Login items are managed by your organisation'
      },
    })
    await renderLoaded()
    const box = screen.getByRole('checkbox', { name: 'Open Picard when you log in' })

    await fireEvent.click(box)

    expect(
      await screen.findByText('Login items are managed by your organisation'),
    ).toBeInTheDocument()
    expect(box).not.toBeChecked()
  })
})
