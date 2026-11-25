// Owner, 2026-11-25: "I currently can't open the settings". The held-email
// strip above the header had grown to 17,536px, so the header and its gear sat
// far below the first screen, and the Settings menu hung under the gear: in an
// 832px Mac app window it started 55px from the bottom. Now the strip scrolls
// inside 45% of the window, and Settings opens fully inside the window
// wherever the gear is (app.js _settingsPlace).
import { test, expect, Page } from './fixtures';

function held(i: number) {
  return {
    id: 'apr_inview' + String(i).padStart(4, '0'), session: 'gtm-engine', age_s: 60, expires: false, expires_in_s: null, requests: 1,
    preview: { endpoint: 'send', from: 'info@example.com', to: `buyer${i}@example.org`, subject: `Draft ${i}`,
      reply_all: false, body: 'Hello,\n\n' + 'A long held draft line.\n'.repeat(30), external_recipients: [`buyer${i}@example.org`] },
  };
}

async function open(page: Page, n: number) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amuxApprCollapsed'); });
  await page.route(/\/api\/email\/approvals(?:\?.*)?$/, r => r.fulfill({ json: { pending: Array.from({ length: n }, (_, i) => held(i)) } }));
  await page.route(/\/api\/grants(?:\?.*)?$/, r => r.fulfill({ json: { pending: [] } }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._approvalsRefresh === 'function' && typeof (window as any)._homeShowEverything === 'function');
  await page.evaluate(() => (window as any)._homeShowEverything());
  await page.evaluate(() => (window as any)._approvalsRefresh());
  await expect(page.locator('#email-approvals-banner')).toBeVisible();
}

async function menuBox(page: Page) {
  return page.evaluate(() => {
    const m = document.getElementById('settings-menu')!, r = m.getBoundingClientRect();
    const t = document.elementFromPoint(r.left + r.width / 2, r.top + Math.min(r.height / 2, 200));
    return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, vh: innerHeight, vw: innerWidth, hit: !!(t && m.contains(t)) };
  });
}

test('many held emails leave the header and its gear on the first screen', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 832 });
  await open(page, 25);
  const strip = await page.locator('#email-approvals-banner').boundingBox();
  expect(strip!.height).toBeLessThanOrEqual(832 * 0.45 + 1);
  // The drafts are all there, scrolled inside the strip.
  await expect(page.locator('#email-approvals-banner details')).toHaveCount(25);
  const gear = await page.locator('#settings-btn').boundingBox();
  expect(gear!.y + gear!.height).toBeLessThan(832);
  await page.locator('#settings-btn').click();
  await expect(page.locator('#settings-menu')).toHaveClass(/open/);
  const b = await menuBox(page);
  expect(b.top).toBeGreaterThanOrEqual(0);
  expect(b.bottom).toBeLessThanOrEqual(b.vh + 1);
  expect(b.hit).toBe(true);
  expect(b.right - b.left).toBeLessThanOrEqual(460);
  // Its tab row is not under the page's own tab bar.
  const bar = await page.evaluate(() => (document.getElementById('chrome-tabs-bar')?.getBoundingClientRect().bottom) || 0);
  expect(b.top).toBeGreaterThanOrEqual(bar);
  await page.screenshot({ path: test.info().outputPath('settings-with-held-mail.png') });
});

test('a gear low in the window opens Settings beside it, inside the window, and the gear still closes it', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 832 });
  await open(page, 1);
  // Push the header to the bottom edge, as any tall strip above it would.
  await page.evaluate(() => {
    const d = document.createElement('div'); d.style.height = (innerHeight - 70) + 'px';
    const h = document.querySelector('.header-row')!; h.parentNode!.insertBefore(d, h); scrollTo(0, 0);
  });
  await page.locator('#settings-btn').click();
  const b = await menuBox(page);
  expect(b.top).toBeGreaterThanOrEqual(0);
  expect(b.bottom).toBeLessThanOrEqual(b.vh + 1);
  expect(b.left).toBeGreaterThanOrEqual(0);
  expect(b.hit).toBe(true);
  await page.locator('#settings-btn').click();
  await expect(page.locator('#settings-menu')).not.toHaveClass(/open/);
});

test('on a phone the menu keeps its own placement', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await open(page, 1);
  await page.evaluate(() => (window as any).toggleSettings());
  const pos = await page.evaluate(() => document.getElementById('settings-menu')!.style.position);
  expect(pos).toBe('');
});
