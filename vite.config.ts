import { svelte } from '@sveltejs/vite-plugin-svelte'
import { svelteTesting } from '@testing-library/svelte/vite'
import { defineConfig } from 'vitest/config'

// Tauri expects a fixed dev port and needs Rust errors to stay visible in the terminal.
export default defineConfig({
  plugins: [svelte(), svelteTesting()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  test: {
    // The components talk to Tauri through `window`, so tests need a DOM.
    environment: 'jsdom',
    include: ['src/**/*.test.ts'],
    setupFiles: ['src/test-setup.ts'],
    // Vitest empties CSS imports, even `?raw`. App.test.ts reads app.css.
    css: { include: [/\/src\/app\.css/] },
  },
})
