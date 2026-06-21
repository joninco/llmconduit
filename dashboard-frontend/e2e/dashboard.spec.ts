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
