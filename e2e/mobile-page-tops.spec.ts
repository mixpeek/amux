// Phone tops for Email, Connectors, Scheduler and Board (Ethan, 2026-10-14:
// "do the same for the other mobile pages"): at phone width the explanations
// fold away, long button labels shorten, nothing overflows sideways; on a
// desktop width all of it is still there.
import { test, expect } from './fixtures';

const shown = (page: any, sel: string) => page.evaluate((s: string) => {
  const e = document.querySelector(s) as HTMLElement | null;
  return !!e && getComputedStyle(e).display !== 'none';
}, sel);

test('phone width folds explanations and shortens labels; desktop keeps them', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  const go = (v: string) => page.evaluate((v) => (window as any).switchView(v), v);

  await page.setViewportSize({ width: 390, height: 844 });
  await go('email');
  expect(await shown(page, '#email-view .email-about')).toBe(false);
  expect(await shown(page, '#email-view .email-about-btn')).toBe(true);
  await page.locator('#email-view .email-about-btn').click();
  expect(await shown(page, '#email-view .email-about'), 'the info button reveals the explanation').toBe(true);
  await go('connectors');
  expect(await shown(page, '.cx-intro')).toBe(false);
  expect(await shown(page, '.cx-add-noun')).toBe(false);
  await go('scheduler');
  expect(await shown(page, '.sx-new-text')).toBe(false);
  await expect(page.locator('.sx-new')).toHaveAttribute('aria-label', 'New schedule');
  await go('board');
  expect(await shown(page, '#board-view .board-page-heading p')).toBe(false);
  for (const v of ['email', 'connectors', 'scheduler', 'board']) {
    await go(v);
    expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth), v + ' overflows sideways').toBeLessThanOrEqual(0);
  }

  await page.setViewportSize({ width: 1400, height: 900 });
  await go('email');
  expect(await shown(page, '#email-view .email-about-btn')).toBe(false);
  await go('connectors');
  expect(await shown(page, '.cx-intro')).toBe(true);
  await go('scheduler');
  expect(await shown(page, '.sx-new-text')).toBe(true);
});
