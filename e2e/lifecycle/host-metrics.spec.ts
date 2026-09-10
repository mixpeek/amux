import { test, expect } from '../fixtures';
import { boot, checkpoint } from './evidence';

test('LC-HOST: measured host analysis, refresh and related navigation work at each viewport', async ({ page }, info) => {
  test.setTimeout(90_000);
  await boot(page);
  const tab = page.locator('#tab-metrics');
  if (!await tab.isVisible()) {
    await page.locator('.tab-customize-wrap > .tab-customize-btn').click();
    await page.locator('#tab-customizer-menu [data-tab-id="metrics"] input[type="checkbox"]').check();
    await page.locator('.tab-customize-wrap > .tab-customize-btn').click();
  }
  await tab.click();
  // The phone worker list is a full-width sidebar. Follow its visible
  // collapse/expand controls before reaching the host-wide mode selector.
  const sidebar = page.locator('#metrics-sidebar');
  await sidebar.getByRole('button', { name: 'Collapse sidebar' }).click();
  await expect(sidebar).toHaveClass(/collapsed/);
  await page.getByRole('button', { name: 'Show workers list' }).click();
  await expect(sidebar).not.toHaveClass(/collapsed/);
  await sidebar.getByRole('button', { name: 'Collapse sidebar' }).click();
  await expect(sidebar).toHaveClass(/collapsed/);
  const response = page.waitForResponse(r => r.url().endsWith('/api/metrics/host'), { timeout: 60_000 });
  await page.locator('#metricsmode-host').click();
  const measured = await response;
  expect(measured.ok()).toBe(true);
  const data = await measured.json();
  expect(data.measured, 'the shipped host probe must actually run').toBe(true);
  expect(data.n_considered).toBeGreaterThan(0);
  expect(data.cpu.count).toBeGreaterThan(0);
  await expect(page.locator('#host-content')).toContainText('Host Analysis');
  await expect(page.locator('#host-content')).toContainText('Top by CPU');
  await checkpoint(page, info, 'host-analysis');
  const refreshed = page.waitForResponse(r => r.url().endsWith('/api/metrics/host'));
  await page.locator('#host-content').getByRole('button', { name: /Refresh/ }).click();
  expect((await refreshed).ok()).toBe(true);
  await page.locator('#host-content [title="Open Disk Cleanup"]').click();
  await expect(page.locator('#reclaim-content')).toBeVisible();
  await expect(page.locator('#host-content')).toBeHidden();
  await page.locator('#metricsmode-system').click();
  await expect(page.locator('#metrics-content')).toBeVisible();
  await expect(page.locator('#reclaim-content')).toBeHidden();
});
