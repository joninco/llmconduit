import AxeBuilder from '@axe-core/playwright';
import type { Page } from '@playwright/test';
import { test, expect, VIEWS, login, openView } from './harness';

const VIEWPORTS = [320, 375, 768, 1024, 1600] as const;

function importantViolations(violations: Awaited<ReturnType<AxeBuilder['analyze']>>['violations']) {
  return violations
    .filter((violation) => violation.impact === 'critical' || violation.impact === 'serious')
    .map((violation) => ({
      id: violation.id,
      impact: violation.impact,
      targets: violation.nodes.map((node) => node.target.join(' ')),
    }));
}

async function assertAxe(page: Page, { forcedColors = false } = {}): Promise<void> {
  let audit = new AxeBuilder({ page });
  // Chromium's forced-colors emulation reports author colors after the UA has replaced them with
  // system colors, producing false contrast failures for virtually every text node. The normal
  // route matrix still runs the contrast rule at every viewport; this pass verifies the remaining
  // semantic rules plus the forced-colors-specific operational alternatives.
  if (forcedColors) audit = audit.disableRules(['color-contrast']);
  const results = await audit.analyze();
  expect(importantViolations(results.violations)).toEqual([]);
}

for (const width of VIEWPORTS) {
  test(`all routes reflow and pass serious/critical axe checks at ${width}px`, async ({ page, consoleErrors }) => {
    test.setTimeout(75_000);
    await page.setViewportSize({ width, height: 900 });
    await login(page);
    await expect(page.getByRole('button', { name: 'Logout' })).toBeVisible();

    for (const view of VIEWS) {
      await openView(page, view);
      await assertAxe(page);
      if (view.name === 'flows') {
        await page.getByTestId('flow-row').first().locator('button').first().click();
        const detail = page.getByTestId('flow-detail');
        await expect(detail).toBeVisible();
        await assertAxe(page);
        await page.keyboard.press('Escape');
        await expect(detail).toHaveCount(0);
      }
    }

    if (width < 1024) await expect(page.getByTestId('mobile-shell')).toBeVisible();
    else await expect(page.getByTestId('mobile-shell')).toHaveCount(0);
    expect(consoleErrors, `console errors at ${width}px`).toEqual([]);
  });
}

test('200% zoom retains navigation and operational controls', async ({ page, consoleErrors }) => {
  await page.setViewportSize({ width: 640, height: 900 });
  await login(page);
  await page.evaluate(() => { document.documentElement.style.zoom = '2'; });
  await expect(page.getByRole('button', { name: 'Logout' })).toBeVisible();
  await expect(page.getByRole('tab', { name: 'Overview' })).toBeVisible();
  await page.getByRole('tab', { name: 'Flows' }).click();
  await expect(page.getByTestId('flow-row').first()).toBeVisible();
  await assertAxe(page);
  expect(consoleErrors, 'console errors at 200% zoom').toEqual([]);
});

test('keyboard journey covers tabs, flow detail focus restoration, and modal fullscreen', async ({ page, consoleErrors }) => {
  await page.setViewportSize({ width: 375, height: 900 });
  await login(page);

  const overview = page.getByRole('tab', { name: 'Overview' });
  await overview.focus();
  await page.keyboard.press('ArrowRight');
  const flowsTab = page.getByRole('tab', { name: 'Flows' });
  await expect(flowsTab).toHaveAttribute('aria-selected', 'true');
  await expect(flowsTab).toBeFocused();

  const origin = page.getByTestId('flow-row').first().getByRole('button');
  await origin.focus();
  await page.keyboard.press('Enter');
  const detail = page.getByTestId('flow-detail');
  await expect(detail).toBeVisible();
  expect(await detail.evaluate((element) => element.contains(document.activeElement))).toBe(true);
  await page.keyboard.press('Escape');
  await expect(detail).toHaveCount(0);
  await expect(origin).toBeFocused();

  await page.getByRole('tab', { name: 'Theater' }).click();
  const fullscreen = page.getByTestId('theater-fullscreen-toggle');
  await fullscreen.click();
  const dialog = page.locator('dialog[data-fullscreen="true"]');
  await expect(dialog).toHaveAttribute('open', '');
  expect(await dialog.evaluate((element) => element.contains(document.activeElement))).toBe(true);
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(page.getByTestId('theater-fullscreen-toggle')).toBeFocused();
  expect(consoleErrors, 'console errors on keyboard journey').toEqual([]);
});

test('reduced motion and forced colors retain accessible topology alternatives', async ({ page, consoleErrors }) => {
  await page.emulateMedia({ reducedMotion: 'reduce', forcedColors: 'active' });
  await login(page);
  await page.getByRole('tab', { name: 'Topology' }).click();
  await expect(page.getByTestId('topology-companion-table')).toBeVisible();
  await expect(page.getByTestId('topo-particle')).toHaveCount(0);
  const node = page.getByTestId('topo-node').first();
  await node.focus();
  await expect(page.getByRole('tooltip')).toBeVisible();
  await assertAxe(page, { forcedColors: true });
  expect(consoleErrors, 'console errors in forced-colors/reduced-motion').toEqual([]);
});
