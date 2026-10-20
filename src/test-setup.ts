// Loaded before every test file (see `test.setupFiles` in vite.config.ts).
import '@testing-library/jest-dom/vitest'
import { clearMocks } from '@tauri-apps/api/mocks'
import { afterEach } from 'vitest'

// jsdom keeps one `window` per test file, so a fake backend from one test
// would otherwise answer the next test's calls.
afterEach(() => clearMocks())
