import { test, expect, type Page } from '@playwright/test';

const TOKEN = 'real-host-token';

async function login(page: Page): Promise<void> {
  await page.goto('/dashboard', { waitUntil: 'networkidle' });
  await page.getByLabel(/access token/i).fill(TOKEN);
  await page.getByRole('button', { name: /sign in/i }).click();
  await expect(page.getByRole('navigation')).toBeVisible();
}

test('real embedded host: auth, CSP/assets, API flow, WS, seek, logout and relogin', async ({ page }) => {
  const shell = await page.request.get('/dashboard');
  expect(shell.status()).toBe(200);
  const csp = shell.headers()['content-security-policy'] ?? '';
  expect(csp).toContain("default-src 'self'");
  expect(csp).toContain("script-src 'self' 'nonce-");
  const scriptPolicy = csp.split(';').find((directive) => directive.trim().startsWith('script-src')) ?? '';
  expect(scriptPolicy).not.toContain("'unsafe-inline'");

  await login(page);
  const bootstrap = await page.evaluate(() => window.__LLMCONDUIT_DASHBOARD__);
  expect(bootstrap).toMatchObject({ authenticated: true, mutations_enabled: true, schema_version: 2 });
  expect((await page.context().cookies()).some((cookie) => cookie.name === 'llmconduit_session')).toBe(true);

  const assetPath = await page.locator('script[type="module"][src]').getAttribute('src');
  expect(assetPath).toMatch(/^\/dashboard\/assets\/.*\.js$/);
  const compressed = await page.request.get(assetPath!, { headers: { 'accept-encoding': 'br' } });
  expect(compressed.status()).toBe(200);
  expect(compressed.headers()['content-encoding']).toBe('br');
  expect(compressed.headers().vary).toContain('Accept-Encoding');
  expect(compressed.headers()['cache-control']).toBe('public,max-age=31536000,immutable');
  const etag = compressed.headers().etag;
  expect(etag).toMatch(/^"[0-9a-f]{64}"$/);
  const cached = await page.request.get(assetPath!, {
    headers: { 'accept-encoding': 'br', 'if-none-match': etag },
  });
  expect(cached.status()).toBe(304);

  const completion = await page.request.post('/v1/chat/completions', {
    data: {
      model: 'mock-model',
      messages: [{ role: 'user', content: 'say hello' }],
      stream: false,
    },
  });
  expect(completion.status()).toBe(200);
  expect((await completion.json()).choices[0].message.content).toContain('hello from upstream');

  const flowsResponse = await page.request.get('/dashboard/api/flows');
  expect(flowsResponse.status()).toBe(200);
  expect(flowsResponse.headers()['x-llmconduit-dashboard-schema']).toBe('2');
  const flows = await flowsResponse.json();
  expect(flows.flow_seq).toBeGreaterThan(0);
  expect(flows.flows.some((flow: { model_served?: string }) => flow.model_served === 'mock-model')).toBe(true);

  await page.getByRole('tab', { name: 'Flows' }).click();
  await expect(page.getByTestId('flow-row').filter({ hasText: 'mock-model' }).first()).toBeVisible();
  await expect(page.getByTestId('live-indicator')).toContainText(/live/i);

  // Five publisher ticks establish a retained historical cut. The local scrubber also receives
  // those real WS metric frames, so selecting an older position exercises REST seek + LIVE resume.
  await page.waitForTimeout(5_500);
  const snapshotResponse = await page.request.get(`/dashboard/api/snapshot?at=${Date.now()}`);
  expect(snapshotResponse.status()).toBe(200);
  const snapshot = await snapshotResponse.json();
  expect(snapshot.history.quota_bytes).toBeGreaterThan(0);
  expect(snapshot.history.retained_cuts).toBeGreaterThan(0);
  expect(snapshot.flow_summaries_truncated).toBe(false);

  const track = page.getByTestId('scrubber-track');
  const box = await track.boundingBox();
  expect(box).not.toBeNull();
  // Select near the newest sample so the first retained five-second cut is at-or-before the
  // requested instant (an early position can correctly predate the oldest retained cut).
  await track.click({ position: { x: Math.max(1, box!.width * 0.9), y: box!.height / 2 } });
  await expect(page.getByTestId('live-toggle')).toBeVisible();
  await expect(page.getByTestId('flow-row').filter({ hasText: 'mock-model' }).first()).toBeVisible();
  await page.getByTestId('live-toggle').click();
  await expect(page.getByTestId('live-indicator')).toBeVisible();

  await page.getByRole('button', { name: 'Logout' }).click();
  await expect(page.getByText(/access token required/i)).toBeVisible();
  await page.getByLabel(/dashboard token/i).fill(TOKEN);
  await page.getByRole('button', { name: /sign in/i }).click();
  await expect(page.getByRole('navigation')).toBeVisible();
  await expect(page.getByRole('tab', { name: 'Overview' })).toBeVisible();
});
