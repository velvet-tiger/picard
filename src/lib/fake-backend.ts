// Test-only stand-in for the Rust commands. The app never imports this file.
import type { InvokeArgs } from '@tauri-apps/api/core'
import { mockIPC } from '@tauri-apps/api/mocks'

export type Args = Record<string, unknown>
export type Handler = (args: Args) => unknown

/**
 * Answers `invoke` calls with `handlers`, keyed by command name, and records
 * every call. A command with no handler throws, so a test notices a call it
 * did not expect. `sent(cmd)` lists the arguments of each call to `cmd`.
 * Event listening (`listen`, `emit`) is answered by the Tauri mock itself.
 */
export function fakeBackend(handlers: Record<string, Handler>) {
  const calls: { cmd: string; args: Args }[] = []
  mockIPC((cmd, payload) => {
    const args = namedArgs(cmd, payload)
    calls.push({ cmd, args })
    const handler = handlers[cmd]
    if (!handler) throw new Error(`fake backend has no handler for ${cmd}`)
    return handler(args)
  }, { shouldMockEvents: true })
  return { sent: (cmd: string) => calls.filter((c) => c.cmd === cmd).map((c) => c.args) }
}

/** Every command in api.ts takes named arguments; raw bytes would be a bug. */
function namedArgs(cmd: string, payload: InvokeArgs | undefined): Args {
  if (payload === undefined) return {}
  if (Array.isArray(payload) || payload instanceof ArrayBuffer || payload instanceof Uint8Array) {
    throw new Error(`${cmd}: expected named arguments, got raw bytes`)
  }
  return payload
}

/** A promise the test settles by hand, to control the order replies arrive in. */
export function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((r) => (resolve = r))
  return { promise, resolve }
}
