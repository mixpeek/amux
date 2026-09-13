import { test, expect } from './fixtures';

// AMUX-4475 / AMUX-4476. On 2026-09-12 Ethan sent five screenshots of the top
// chrome: the interaction-feedback "Actions/Confirmed" hub orphaned at the header's
// right edge, a cramped mobile header, and "weird blue highlighting" on the nav
// tabs; plus "messages ... its too slow". This spec pins the fixes, and because
// e2e/playwright.config.ts runs the WHOLE suite under each of the desktop (1280),
// mobile (375) and ios-safari (iPhone 15) projects, these assertions run on both
// mobile and desktop in CI (rust.yml `e2e:` job) automatically. That is the
// "any UI change is tested on both mobile and desktop, in CI" guarantee.

function stubFleet(page: import('@playwright/test').Page) {
  return page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({
    json: Array.from({ length: 52 }, (_, i) => ({
      name: 'chrome-fixture-' + i, running: true, status: 'working', dir: '/workspace',
      provider: 'codex', rate_limited_until: i < 12 ? Date.now() / 1000 + 3600 : null,
    })),
  }));
}

test('the header toolbar is decluttered and fits the viewport', async ({ page }, info) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await stubFleet(page);
  await page.goto('/');
  await expect(page.locator('#active-count')).toHaveText('52');
  const width = page.viewportSize()!.width;

  // The interaction-feedback hub must not be visible in the header (AMUX-4475).
  // It may still exist in the DOM (state/feedback.mjs tracks action outcomes),
  // but it is hidden so it stops orphaning the toolbar.
  const hub = page.locator('#interaction-feedback');
  if (await hub.count()) await expect(hub).toBeHidden();

  // The chrome fits: no horizontal overflow at this project's viewport.
  expect(await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  )).toBeLessThanOrEqual(1);

  // Every header action button is on-screen (x within the viewport).
  for (const id of ['brand-header', 'notif-btn', 'rate-limit-pill', 'add-btn', 'settings-btn', 'active-btn']) {
    const box = await page.locator('#' + id).boundingBox();
    expect(box, id).not.toBeNull();
    expect(box!.x, id).toBeGreaterThanOrEqual(-0.5);
    expect(box!.x + box!.width, id).toBeLessThanOrEqual(width + 0.5);
  }

  // On desktop widths the toolbar USES the real estate: the actions cluster is
  // anchored toward the right, not crammed beside the brand (AMUX-4475,
  // "make the toolbar use the real estate we have").
  if (width >= 1000) {
    const actions = await page.locator('.header-actions').boundingBox();
    expect(actions!.x + actions!.width).toBeGreaterThan(width * 0.7);
  }

  await page.screenshot({ path: info.outputPath('header-chrome-' + width + '.png'), clip: { x: 0, y: 0, width, height: 140 } });
});

test('a focused nav tab never draws a ring that overflow can clip into blue bars', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await stubFleet(page);
  await page.goto('/');
  await expect(page.locator('#tab-board')).toBeVisible();
  // .tab-bar is overflow-x:auto, which per the overflow spec forces overflow-y:auto,
  // so a focus ring at offset >= 0 has its top/bottom clipped into stray blue
  // vertical bars (AMUX-4475). After focusing a tab, there must be either NO ring
  // or an INSET one (negative outline-offset) — never a visible ring at offset >= 0.
  const ring = await page.locator('#tab-board').evaluate((e) => {
    (e as HTMLElement).focus();
    const cs = getComputedStyle(e);
    return { width: parseFloat(cs.outlineWidth) || 0, offset: parseFloat(cs.outlineOffset) || 0 };
  });
  expect(ring.width === 0 || ring.offset < 0).toBe(true);
});
