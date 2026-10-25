// On a phone the Messages log keeps ONE row of controls above the list and ONE
// below (Ethan, 2026-10-21: "there should only be 1 row at bottom and one on
// top and they can scroll right"). It had six rows on top (back, mode switch,
// search, filters, date and count, kind chips) and a pager that wrapped to two,
// leaving the list the bottom third of a 375px screen.
import { test, expect, Page } from './fixtures';

async function openLog(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function' && typeof _cvSelect === 'function'");
  await page.evaluate("switchView('messages')");
  await page.evaluate("_cvSelect('@all')");
  await expect(page.locator('#msgs-log-pane')).toBeVisible();
  await expect(page.locator('#msgs-search')).toBeVisible();
}

const box = (page: Page, sel: string) => page.locator(sel).first().boundingBox();

test('phone: one row of controls above the list, one below, both scroll sideways', async ({ page }) => {
  const vp = page.viewportSize()!;
  test.skip(vp.width > 600, 'phone layout');
  await openLog(page);
  // Enough pages that the pager shows all of its controls.
  await page.evaluate("_msgsTotal = 4293; _msgsPage = 1; _msgsRenderPager()");
  const bar = (await box(page, '#msgs-topbar'))!;
  expect(bar.height, 'the top bar is one row').toBeLessThanOrEqual(56);
  for (const sel of ['#msgmode-messages', '#msgmode-ask', '#msgs-search', '#msgs-session-filter', '#msgs-sender-filter', '#msgs-count']) {
    const b = (await box(page, sel))!;
    expect(b, sel + ' is laid out').toBeTruthy();
    const mid = b.y + b.height / 2;
    expect(mid, sel + ' sits in the one top row').toBeGreaterThanOrEqual(bar.y);
    expect(mid, sel + ' sits in the one top row').toBeLessThanOrEqual(bar.y + bar.height);
  }
  // It scrolls sideways rather than wrapping.
  expect(await page.locator('#msgs-topbar').evaluate(e => e.scrollWidth > e.clientWidth)).toBe(true);
  const pager = (await box(page, '#msgs-pager'))!;
  expect(pager.height, 'the pager is one row').toBeLessThanOrEqual(60);
  // Every control the old layout offered is still reachable: scroll to the end.
  await page.locator('#msgs-topbar').evaluate(e => { e.scrollLeft = e.scrollWidth; });
  await expect(page.locator('#msgs-kind-filter button').first()).toBeAttached();
  // The list gets most of the screen.
  const list = (await box(page, '#msgs-list'))!;
  expect(list.height, 'the list is not squeezed').toBeGreaterThan(vp.height * 0.5);
  // Touch targets stay 44px.
  for (const sel of ['#msgmode-messages', '#msgs-session-filter']) expect((await box(page, sel))!.height).toBeGreaterThanOrEqual(44);
});

test('desktop: the controls keep their rows', async ({ page }) => {
  test.skip(page.viewportSize()!.width <= 600, 'desktop layout');
  await openLog(page);
  const search = (await box(page, '#msgs-search'))!;
  const modes = (await box(page, '#msgmode-messages'))!;
  expect(search.y, 'the search is below the mode switch').toBeGreaterThan(modes.y + modes.height - 1);
  expect(search.width).toBeGreaterThan(300);
});
