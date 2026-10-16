// A press on a worker card must survive a list render that lands during it.
//
// render() rewrites the whole card list. When it ran between pointerdown and
// pointerup the press went down on one node and came up on its replacement,
// the browser fired no click, and the tap did nothing. That is how
// LC-COMPOSER-FILES lost its card-menu tap on ios-safari (run 38917609108):
// no menu, no card-menu-geo beacon, and the pressed button missing from the
// next DOM snapshot. Without the deferral in render() this test fails on
// desktop and ios-safari alike.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('a card menu press survives a card-list render between press and release', async ({ page, request }, info) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).render === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `press-render-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  try {
    await page.goto('/#view=sessions');
    const btn = page.locator(`#cards .card[data-session="${name}"] .card-menu-btn`).locator('visible=true').first();
    await expect(btn).toBeVisible({ timeout: 15_000 });
    await btn.scrollIntoViewIfNeeded();
    // One atomic read of a live node: boundingBox() can return null when a
    // poll replaces the card between resolving it and measuring it.
    const box = await btn.evaluate(el => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; });
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.evaluate(() => { (window as any).render(); });
    await page.mouse.up();
    await expect(page.locator('.card-menu.open [data-worker-action="peek-terminal"]')).toBeVisible({ timeout: 3_000 });
    // The render was deferred, not dropped: the counter the beacon reports moved.
    expect(await page.evaluate('_cardsRenderDeferredN')).toBeGreaterThan(0);
  } finally {
    await cleanup('close menu', () => page.evaluate(() => (window as any).closeAllMenus?.()));
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: {} }));
  }
});
