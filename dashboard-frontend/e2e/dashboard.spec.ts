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

    // Gap 01 finding 4: every chip exposes its data-quality provenance. The mock window
    // is fully measured, so directly-counted metrics read `measured`, sample-derived ones
    // `derived`, and the priced cost `estimated` (labelled as such, per the plan).
    const quality = (key: string) => page.getByTestId(`chip-${key}`).getAttribute('data-quality');
    expect(await quality('reqs_per_sec')).toBe('measured');
    expect(await quality('active_streams')).toBe('measured');
    expect(await quality('p50')).toBe('derived');
    expect(await quality('tokens_per_sec')).toBe('derived');
    expect(await quality('cost_per_min')).toBe('estimated');

    expect(consoleErrors, 'console errors on the stats strip').toEqual([]);
  });

  // Gap 08: the tokens cell reveals a token-economics popover (cached/reasoning split + cache-hit
  // + "$ saved"), and the aggregate cache-economics panel rolls the hit rate up by model. The
  // popover must render the split AND an honest `—` for an UNREPORTED class (never a fabricated 0).
  test('tokens popover shows the cached/reasoning split + — on unreported (gap 08)', async ({ page, consoleErrors }) => {
    await login(page);
    await openView(page, VIEWS[0]!); // Flows
    await page.waitForTimeout(400);

    // api_002's seed flow reports prompt/completion but UNREPORTED cached/reasoning → the popover
    // must show `—` for those classes, not `0`. Hover its tokens cell to reveal the breakdown.
    const row = page.getByTestId('flow-row').filter({ hasText: '/v1/chat/completions' }).first();
    await row.getByTestId('tokens-cell').hover();
    const popover = page.getByTestId('tokens-popover');
    await expect(popover).toBeVisible();
    // The split lines are present…
    await expect(popover.getByTestId('econ-line-cached')).toBeVisible();
    await expect(popover.getByTestId('econ-line-reasoning')).toBeVisible();
    // …and the unreported cached class reads the unavailable marker, NEVER `0`.
    const cachedLine = popover.getByTestId('econ-line-cached');
    await expect(cachedLine).toHaveAttribute('data-quality', 'unavailable');
    await expect(cachedLine).toContainText('—');

    // The aggregate cache-economics panel expands to a per-model roll-up.
    await page.getByTestId('cache-economics-toggle').click();
    await expect(page.getByTestId('cache-economics-table')).toBeVisible();
    await expect(page.getByTestId('cache-economics-row').first()).toBeVisible();

    expect(consoleErrors, 'console errors on the tokens popover').toEqual([]);
  });

  // Gap 09: the FlowDetail inspector shows a context-window utilization gauge, and the flows
  // screen shows an aggregate context-pressure stat. The gauge must render a DERIVED % for a flow
  // on a model WITH a known context window, and `—` (unavailable) for one WITHOUT — never a
  // fabricated 0%/100%. Covers the acceptance criterion: gauge WITH and WITHOUT `context_limit`.
  test('context gauge: derived with a known limit, — without (gap 09)', async ({ page, consoleErrors }) => {
    await login(page);
    await openView(page, VIEWS[0]!); // Flows
    await page.waitForTimeout(400);

    // The aggregate context-pressure stat is present, with a measured-coverage readout.
    await expect(page.getByTestId('context-pressure-panel')).toBeVisible();
    await expect(page.getByTestId('context-pressure-coverage')).toContainText('measured');

    // api_001 is served by llama-3.1-70b (catalog context_limit 131072) + reports usage → the
    // inspector gauge reads a DERIVED utilization, not `—`.
    const known = page.getByTestId('flow-row').filter({ hasText: '/v1/responses' }).first();
    await known.click();
    await expect(page.getByTestId('flow-detail')).toBeVisible();
    const gauge = page.getByTestId('context-gauge');
    await expect(gauge).toBeVisible();
    await expect(gauge).toHaveAttribute('data-quality', 'derived');
    await expect(page.getByTestId('context-util-pct')).not.toHaveText('—');
    await expect(page.getByTestId('context-gauge-fill')).toBeVisible();

    // api_004 is served by `mystery-model` (catalog context_limit NULL) but DOES report usage →
    // the gauge must read `—` (unknown capacity), NEVER 0% / 100%. Select by the model id (unique
    // to that row) so the known-window llama row on the same endpoint is not picked instead.
    const unknown = page.getByTestId('flow-row').filter({ hasText: 'mystery-model' }).first();
    await unknown.click();
    await expect(page.getByTestId('context-gauge')).toHaveAttribute('data-quality', 'unavailable');
    const pct = page.getByTestId('context-util-pct');
    await expect(pct).toHaveText('—');
    await expect(page.getByTestId('context-gauge-fill')).toHaveCount(0);

    expect(consoleErrors, 'console errors on the context gauge').toEqual([]);
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
