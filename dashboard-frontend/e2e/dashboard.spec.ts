import { test, expect, VIEWS, installDeterminism, login, openView } from './harness';

test.describe('Argus dashboard', () => {
  test('login shell renders before auth', async ({ page, consoleErrors }) => {
    await installDeterminism(page);
    await page.goto('/dashboard/?mock=1', { waitUntil: 'networkidle' });
    await expect(page.getByText(/access token required/i)).toBeVisible();
    await page.evaluate(() => document.fonts.ready.then(() => undefined));
    await expect(page).toHaveScreenshot('login.png');
    expect(consoleErrors, 'console errors on login shell').toEqual([]);
  });

  // Gap 01: the stats strip must be HONEST under live (mock-streamed) traffic — real
  // numeric values, not the all-`0.0` the live WS tile used to ship. The mock streams a
  // snapshot + a metric_tick (active_streams/tokens_per_sec/cost_per_min all > 0), so the
  // chips must read real numbers — and the don't-lie-with-zeros markers must NOT appear
  // while real samples are present.
  test('stats strip reads real metrics under live mock traffic (gap 01)', async ({ page, consoleErrors }) => {
    await login(page);
    // Let the mock deliver its snapshot + live frames (incl. the metric_tick).
    await page.waitForTimeout(800);

    // tok/s + $/min + active are the fields the OLD WS tile hard-coded to 0 — they must
    // now carry real values (the mock seeds them > 0), proving live flows reach the strip.
    for (const key of ['active_streams', 'tokens_per_sec', 'cost_per_min', 'reqs_per_sec']) {
      const value = page.getByTestId(`chip-${key}`).getByTestId('chip-value');
      await expect(value).toBeVisible();
      const text = (await value.textContent())?.trim() ?? '';
      expect(text, `${key} must be measured, not unavailable`).not.toBe('—');
      // A real, non-zero reading (the mock's seeded window is all > 0).
      expect(text, `${key} reads a real number`).toMatch(/[1-9]/);
    }
    expect(consoleErrors, 'console errors on the stats strip').toEqual([]);
  });

  for (const view of VIEWS) {
    test(`${view.name}: renders + no console errors + matches baseline`, async ({ page, consoleErrors }) => {
      await login(page);
      await openView(page, view);
      // Let d3-force / uPlot / sankey reach their settled frame before the pixel baseline.
      // (mock streams a finite snapshot + 5 frames, then is quiescent.)
      await page.waitForTimeout(800);
      await expect(page).toHaveScreenshot(`${view.name}.png`);
      expect(consoleErrors, `console errors on ${view.name}`).toEqual([]);
    });
  }
});
