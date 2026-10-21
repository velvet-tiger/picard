import { fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { mockWindows } from '@tauri-apps/api/mocks'
import { afterEach, describe, expect, it } from 'vitest'
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
    appearance: 'system',
    auto_update: true,
    roots: ['~/working'],
    exclude: [],
    // The backend fills in the defaults when the file has no list.
    activity_ignore: ['.claude', 'AGENTS.md'],
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
    get_not_activity: () => ['.claude', 'AGENTS.md'],
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
  afterEach(() => {
    delete document.documentElement.dataset.theme
  })

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

  it('lists the default names as rows that can be removed', async () => {
    backend()
    await renderLoaded()

    expect(screen.getByDisplayValue('.claude')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Remove AGENTS.md' })).toBeInTheDocument()
  })

  it('treats the defaults in another order as the defaults', async () => {
    backend()
    await renderLoaded()
    const restore = () => screen.getByRole('button', { name: 'Restore defaults' })

    await fireEvent.click(screen.getByRole('button', { name: 'Remove .claude' }))
    // Enabled only once the defaults have loaded and the list differs.
    await waitFor(() => expect(restore()).toBeEnabled())
    await fireEvent.click(screen.getByRole('button', { name: 'Add name' }))
    const rows = screen.getAllByPlaceholderText('.vscode')
    await fireEvent.input(rows[rows.length - 1], { target: { value: '.claude' } })
    expect(restore()).toBeDisabled()
    // A blank row is dropped on save, so the list still holds the defaults.
    await fireEvent.click(screen.getByRole('button', { name: 'Add name' }))
    expect(restore()).toBeDisabled()
  })

  it('saves added names, trimmed, and drops blank ones', async () => {
    const { sent } = backend()
    const view = await renderLoaded()

    await fireEvent.click(screen.getByRole('button', { name: 'Add name' }))
    await fireEvent.click(screen.getByRole('button', { name: 'Add name' }))
    const rows = screen.getAllByPlaceholderText('.vscode')
    await fireEvent.input(rows[rows.length - 2], { target: { value: '  .vscode  ' } })
    expect(view.hasUnsavedChanges()).toBe(true)
    await fireEvent.click(saveButton())

    expect(
      await screen.findByText('Saved. Picard has reloaded the settings.'),
    ).toBeInTheDocument()
    expect(sent('save_settings')).toEqual([
      {
        settings: expect.objectContaining({ activity_ignore: ['.claude', 'AGENTS.md', '.vscode'] }),
      },
    ])
    expect(view.hasUnsavedChanges()).toBe(false)
  })

  it('removes a default and restores the defaults', async () => {
    backend()
    const view = await renderLoaded()
    const restore = () => screen.getByRole('button', { name: 'Restore defaults' })

    await fireEvent.click(screen.getByRole('button', { name: 'Remove AGENTS.md' }))
    expect(screen.queryByDisplayValue('AGENTS.md')).not.toBeInTheDocument()
    expect(view.hasUnsavedChanges()).toBe(true)
    await waitFor(() => expect(restore()).toBeEnabled())

    await fireEvent.click(restore())
    expect(screen.getByDisplayValue('AGENTS.md')).toBeInTheDocument()
    expect(view.hasUnsavedChanges()).toBe(false)
    expect(restore()).toBeDisabled()
  })

  it('still loads when the default names cannot be read', async () => {
    backend({
      get_not_activity: () => {
        throw 'not allowed'
      },
    })
    await renderLoaded()

    expect(
      await screen.findByText('Could not read the default names: not allowed'),
    ).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Add name' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Restore defaults' })).toBeDisabled()
  })

  it('sends an emptied list after Remove', async () => {
    const { sent } = backend({
      get_settings: () => ({ ...config(), activity_ignore: ['.vscode'] }),
    })
    await renderLoaded()

    await fireEvent.click(screen.getByRole('button', { name: 'Remove .vscode' }))
    expect(screen.getByText('None. Every change counts as work.')).toBeInTheDocument()
    await fireEvent.click(saveButton())

    await screen.findByText('Saved. Picard has reloaded the settings.')
    expect(sent('save_settings')).toEqual([
      { settings: expect.objectContaining({ activity_ignore: [] }) },
    ])
  })

  it('shows the error for an invalid name and keeps it to fix', async () => {
    const message =
      'activity_ignore entry "src/lib" must be a plain file or folder name, not a path or pattern'
    backend({
      save_settings: () => {
        throw message
      },
    })
    const view = await renderLoaded()

    await fireEvent.click(screen.getByRole('button', { name: 'Add name' }))
    const rows = screen.getAllByPlaceholderText('.vscode')
    await fireEvent.input(rows[rows.length - 1], { target: { value: 'src/lib' } })
    await fireEvent.click(saveButton())

    expect(await screen.findByText(message)).toBeInTheDocument()
    expect(screen.getByDisplayValue('src/lib')).toBeInTheDocument()
    expect(view.hasUnsavedChanges()).toBe(true)
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

  it('cleans every Cargo project once the untouched box is unticked', async () => {
    const { sent } = backend({
      get_settings: () => ({
        ...config(),
        tasks: [{ type: 'cargo_clean', enabled: true, mode: 'stale', stale_days: 5 }],
      }),
    })
    await renderLoaded()
    const box = screen.getByRole('checkbox', { name: 'Only projects untouched for' })
    const days = screen.getByRole('spinbutton', { name: 'Days untouched' })
    expect(box).toBeChecked()
    expect(days).toBeEnabled()

    await fireEvent.click(box)

    expect(days).toBeDisabled()
    expect(
      screen.getByText('Cleans every project, including ones you are working on.'),
    ).toBeInTheDocument()
    await fireEvent.click(saveButton())
    await waitFor(() =>
      expect(sent('save_settings')).toEqual([
        {
          settings: expect.objectContaining({
            tasks: expect.arrayContaining([
              expect.objectContaining({ type: 'cargo_clean', mode: 'all', stale_days: 5 }),
            ]),
          }),
        },
      ]),
    )
  })

  it('saves the chosen appearance and applies it only once saved', async () => {
    const { sent } = backend()
    await renderLoaded()
    const root = document.documentElement

    await fireEvent.click(screen.getByRole('radio', { name: /Dark/ }))
    expect(root.hasAttribute('data-theme')).toBe(false)
    await fireEvent.click(saveButton())

    await screen.findByText('Saved. Picard has reloaded the settings.')
    expect(sent('save_settings')).toEqual([
      { settings: expect.objectContaining({ appearance: 'dark' }) },
    ])
    expect(root.dataset.theme).toBe('dark')
  })

  it('saves the automatic updates choice', async () => {
    const { sent } = backend()
    await renderLoaded()
    const box = screen.getByRole('checkbox', { name: 'Check for updates daily and install them' })
    expect(box).toBeChecked()

    await fireEvent.click(box)
    await fireEvent.click(saveButton())

    await screen.findByText('Saved. Picard has reloaded the settings.')
    expect(sent('save_settings')).toEqual([
      { settings: expect.objectContaining({ auto_update: false }) },
    ])
  })

  it('keeps the old appearance when the save fails', async () => {
    backend({
      save_settings: () => {
        throw 'disk full'
      },
    })
    await renderLoaded()
    document.documentElement.dataset.theme = 'dark'

    await fireEvent.click(screen.getByRole('radio', { name: /Light/ }))
    await fireEvent.click(saveButton())

    expect(await screen.findByText('disk full')).toBeInTheDocument()
    expect(document.documentElement.dataset.theme).toBe('dark')
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
