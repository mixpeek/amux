// The Mac app (desktop/, user agent "Amux-Desktop/…") has real windows and
// tabs, so the dashboard's in-page tab strip is hidden there and the page
// always shows its own first tab. A browser keeps the strip.
import { test, expect } from './fixtures';

test.describe('in the Mac app', () => {
  test.use({ userAgent: 'Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15 Amux-Desktop/2.0' });
  test('the in-page tab strip is hidden and takes no space, even with a second tab saved as active', async ({ page }, info) => {
    test.skip(info.project.name !== 'desktop', 'desktop layout only');
    await page.addInitScript(() => {
      localStorage.setItem('amux_chrome_tabs2', JSON.stringify([{ id: 1, name: 'Tab 1' }, { id: 2, name: 'Tab 2' }]));
      localStorage.setItem('amux_chrome_active2', '2');
    });
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).switchView === 'function');
    await expect(page.locator('body')).toHaveClass(/native-tabs/);
    await expect(page.locator('#chrome-tabs-bar')).toBeHidden();
    await expect(page.locator('#chrome-tab-frames')).toBeHidden();
    await expect.poll(() => page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--chrome-tab-h').trim())).toBe('0px');
    expect(await page.evaluate(() => (0, eval)('_chromeActiveId'))).toBe(1);
  });
});

test('in a browser the in-page tab strip is still shown', async ({ page }, info) => {
  test.skip(info.project.name !== 'desktop', 'desktop layout only');
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await expect(page.locator('body')).not.toHaveClass(/native-tabs/);
  await expect(page.locator('#chrome-tabs-bar')).toBeVisible();
});
