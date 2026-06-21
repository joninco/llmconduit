import { defineConfig } from '@playwright/test';

/**
 * E2E + visual-regression harness for the Argus dashboard.
 *
 * Runs against the Vite dev server (port 5273) in mock mode (`?mock=1`), so it needs
 * no Rust backend. `webServer` auto-starts `npm run dev` and reuses an already-running
 * dev server locally. Screenshot baselines live in `e2e/dashboard.spec.ts-snapshots/`
 * (committed); regenerate with `npm run e2e:update`.
 */
export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: process.env.CI ? 2 : undefined,
  reporter: [['list']],
  timeout: 30_000,
  expect: {
    timeout: 10_000,
    // Anti-alias / sub-pixel tolerance for the canvas/SVG views (d3-force, sankey, uPlot).
    toHaveScreenshot: { maxDiffPixelRatio: 0.02, animations: 'disabled', caret: 'hide' },
  },
  use: {
    baseURL: 'http://localhost:5273',
    viewport: { width: 1600, height: 1000 },
    trace: 'on-first-retry',
    screenshot: 'only-on-failure',
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
  webServer: {
    command: 'npm run dev',
    url: 'http://localhost:5273/dashboard/',
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
    stdout: 'ignore',
    stderr: 'pipe',
  },
});
