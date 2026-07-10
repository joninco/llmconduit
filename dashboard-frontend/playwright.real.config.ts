import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e-real',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 90_000,
  reporter: [['list']],
  use: {
    baseURL: 'http://127.0.0.1:5274',
    viewport: { width: 1280, height: 800 },
    trace: 'on-first-retry',
  },
  projects: [{ name: 'real-host-chromium', use: { browserName: 'chromium' } }],
  webServer: {
    command: 'node e2e/real-host.mjs',
    url: 'http://127.0.0.1:5274/health',
    reuseExistingServer: false,
    timeout: 300_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
});
