import { cleanup, render, waitFor } from '@testing-library/svelte'
import { mockWindows } from '@tauri-apps/api/mocks'
import { afterEach, describe, expect, it, vi } from 'vitest'
import App from './App.svelte'
import css from './app.css?raw'
import { fakeBackend } from './lib/fake-backend'

/** Answers the calls App makes on load. No tab is opened, so no tab loads data. */
function backend(appearance: () => unknown) {
  mockWindows('main')
  return fakeBackend({
    get_appearance: appearance,
  })
}

describe('App', () => {
  // App stops listening for `show-tab` when it unmounts. That needs the Tauri
  // mocks, which test-setup.ts clears in an afterEach that runs after this one.
  afterEach(async () => {
    cleanup()
    await new Promise((settle) => setTimeout(settle))
    delete document.documentElement.dataset.theme
  })

  it('applies the configured appearance when the window loads', async () => {
    backend(() => 'dark')
    render(App)
    await waitFor(() => expect(document.documentElement.dataset.theme).toBe('dark'))
  })

  it('follows macOS when the appearance cannot be read', async () => {
    const logged = vi.spyOn(console, 'error').mockImplementation(() => {})
    backend(() => {
      throw 'config.toml is not valid'
    })
    render(App)
    await waitFor(() => expect(logged).toHaveBeenCalled())
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false)
    logged.mockRestore()
  })
})

describe('app.css', () => {
  // Forced Dark and macOS dark each list the dark palette, so an edit to one
  // block alone would make them differ.
  it('uses the same dark palette for forced Dark and macOS dark', () => {
    const vars = (selector: string) => {
      const start = css.indexOf(selector)
      expect(start, selector).toBeGreaterThan(-1)
      const body = css.slice(start, css.indexOf('}', start))
      return [...body.matchAll(/(--[\w-]+):\s*([^;]+);/g)].map((m) => `${m[1]}: ${m[2]}`)
    }
    const forced = vars(":root[data-theme='dark'] {")
    expect(forced.length).toBeGreaterThan(0)
    expect(vars(':root:not([data-theme]) {')).toEqual(forced)
  })

  // jsdom has no layout, so these check the rules themselves.
  const rule = (selector: RegExp) => {
    const start = css.search(selector)
    expect(start, String(selector)).toBeGreaterThan(-1)
    return css.slice(start, css.indexOf('}', start))
  }

  // A scrollbar that appears only on tall tabs would make them narrower than
  // the others.
  it('always reserves the scrollbar track so every tab has the same width', () => {
    expect(rule(/^\.scroll-area\s*\{/m)).toMatch(/overflow-y:\s*scroll\s*;/)
  })

  // The window has no title strip, so a page scrollbar would run up beside
  // the tab bar. Only the area under the bar may scroll.
  it('keeps the page still so the scrollbar starts under the tab bar', () => {
    expect(rule(/^html,\s*body\s*\{/m)).toMatch(/overflow:\s*hidden\s*;/)
  })
})
