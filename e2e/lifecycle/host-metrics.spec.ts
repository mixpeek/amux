import { test, expect } from '../fixtures';
import { boot, checkpoint } from './evidence';

test('LC-HOST: measured host analysis, refresh and related navigation work at each viewport', async ({ page }, info) => {
  test.setTimeout(90_000);
  const diagnostics: any[] = [];
  page.on('request', request => {
    if (request.url().endsWith('/api/client-debug') && request.method() === 'POST') {
      const data = request.postDataJSON();
      if (data?.kind === 'host-analysis-contrast') diagnostics.push(data);
    }
  });
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
  for (const theme of ['light', 'dark']) {
    if (await page.locator('body').evaluate(el => el.classList.contains('light')) !== (theme === 'light')) {
      await page.locator('#settings-btn').click();
      await page.locator('#settings-menu .settings-tab-btn[data-stab="device"]').click();
      await page.locator('#theme-checkbox + .theme-track').click();
      await page.locator('#settings-btn').click();
      await expect(page.locator('#settings-menu')).not.toHaveClass(/open/);
    }
    const refreshed = page.waitForResponse(r => r.url().endsWith('/api/metrics/host'));
    await page.locator('#host-content').getByRole('button', { name: /Refresh/ }).click();
    expect((await refreshed).ok()).toBe(true);
    const chips = page.locator('#host-content > div:nth-child(2) > span');
    await expect(chips).toHaveCount(3);
    const ratios = await chips.evaluateAll(elements => {
      const luminance = (color: string) => {
        const [r, g, b] = color.match(/[\d.]+/g)!.slice(0, 3).map(Number).map(v => {
          const c = v / 255;
          return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
        });
        return .2126 * r + .7152 * g + .0722 * b;
      };
      return elements.flatMap(chip => [chip, chip.querySelector('b')!].map(el => {
        const a = luminance(getComputedStyle(el).color), b = luminance(getComputedStyle(chip).backgroundColor);
        return { text: el.textContent, ratio: (Math.max(a, b) + .05) / (Math.min(a, b) + .05) };
      }));
    });
    await info.attach(`host-contrast-${theme}`, { body: JSON.stringify(ratios), contentType: 'application/json' });
    for (const sample of ratios) expect(sample.ratio, `${theme}: ${sample.text} must be readable`).toBeGreaterThanOrEqual(4.5);
    await expect.poll(() => diagnostics.some(d => d.light === (theme === 'light') && d.verdict === 'readable' && d.n_considered === 6)).toBe(true);
    await checkpoint(page, info, `host-analysis-${theme}`);
  }
  await page.locator('#host-content [title="Open Disk Cleanup"]').click();
  await expect(page.locator('#reclaim-content')).toBeVisible();
  await expect(page.locator('#host-content')).toBeHidden();
  await page.locator('#metricsmode-system').click();
  await expect(page.locator('#metrics-content')).toBeVisible();
  await expect(page.locator('#reclaim-content')).toBeHidden();
});
