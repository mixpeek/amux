// The Photos tab (Ethan, 2026-11-08: "Create a new amux tab called Photos").
// Welcome and BYO Mixpeek connection, the sample library's grid at 10,000
// items, search with suggestions and filters, Explore collections, the viewer
// with its Info panel, and returning to the exact place. Every project, the
// 375px phone included.
import { test, expect, Page } from './fixtures';

async function openPhotos(page: Page, conn: object = { configured: false, status: 'disconnected' }) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    for (const k of Object.keys(localStorage)) if (k.startsWith('amux_photos_')) localStorage.removeItem(k);
  });
  await page.route(/\/api\/photos\/mixpeek(\?.*)?$/, (r) => r.request().method() === 'GET' ? r.fulfill({ json: conn }) : r.fallback());
  await page.goto('/#view=photos');
  await expect(page.locator('#photos-view .ph-app')).toBeVisible({ timeout: 20_000 });
}
async function enterSample(page: Page) {
  await page.locator('[data-act="sample"]').click();
  await expect(page.locator('.ph-tile').first()).toBeVisible();
}
const noSideScroll = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1);

test('first open welcomes, then the library is a dated grid that renders only what is near the screen', async ({ page }) => {
  await openPhotos(page);
  const welcome = page.locator('.ph-welcome');
  await expect(welcome).toBeVisible();
  await expect(welcome.locator('[data-act="connect"]')).toHaveText('Connect Mixpeek');
  await expect(welcome.locator('a[href="https://mixpeek.com"]')).toHaveText('Get started with Mixpeek');
  await enterSample(page);
  expect(await page.evaluate(() => (window as any).AmuxPhotos._source().count)).toBe(10000);
  await expect(page.locator('.ph-day').first()).toBeVisible();
  // Virtualized: a screenful of tiles, not ten thousand.
  const tiles = await page.locator('.ph-tile').count();
  expect(tiles).toBeGreaterThan(5);
  expect(tiles).toBeLessThan(600);
  await expect(page.locator('.ph-badge').first()).toBeAttached();
  // Far down the library, the date pins at the top and the rows there are drawn.
  await page.locator('.ph-body').evaluate((b) => { b.scrollTop = b.scrollHeight * 0.6; });
  await expect(page.locator('.ph-sticky')).toBeVisible();
  await expect(page.locator('.ph-tile').first()).toBeVisible();
  expect(await noSideScroll(page)).toBe(true);
});

test('search in plain words: suggestions, results, a videos filter, and recent searches', async ({ page }) => {
  await openPhotos(page);
  await enterSample(page);
  const box = page.locator('.ph-search input');
  await box.click();
  await box.fill('sun');
  await expect(page.locator('.ph-suggest button').first()).toBeVisible();
  await box.fill('sunset');
  await box.press('Enter');
  await expect(page.locator('.ph-results')).toContainText('results for sunset');
  const n = Number((await page.locator('.ph-results').innerText()).replace(/[^0-9]/g, ''));
  expect(n).toBeGreaterThan(50);
  await page.locator('.ph-filters [data-type="video"]').click();
  await expect(page.locator('.ph-filters [data-type="video"]')).toHaveClass(/on/);
  const nv = Number((await page.locator('.ph-results').innerText()).replace(/[^0-9]/g, ''));
  expect(nv).toBeGreaterThan(0);
  expect(nv).toBeLessThan(n);
  // Every tile drawn is a video now.
  const tiles = page.locator('.ph-tile');
  expect(await tiles.count()).toBe(await page.locator('.ph-tile .ph-badge').count());
  // A query that matches nothing says so.
  await box.fill('zebra crossing in antarctica');
  await box.press('Enter');
  await expect(page.locator('.ph-state')).toContainText('No matches');
  // Search home lists recent searches.
  await page.locator('[data-act="clearq"]').click();
  await expect(page.locator('h3', { hasText: 'Recent searches' })).toBeVisible();
  await expect(page.locator('.ph-chip[data-q="sunset"]')).toBeVisible();
});

test('Explore opens a collection; the viewer navigates, shows Info, and closing returns to the same place', async ({ page }) => {
  await openPhotos(page);
  await enterSample(page);
  await page.locator('[data-nav="explore"]:visible').first().click();
  const mosaic = page.locator('.ph-mosaic').first();
  await expect(mosaic).toBeVisible();
  const label = (await mosaic.locator('.ph-mosaic-cap b').innerText()).trim();
  await mosaic.click();
  await expect(page.locator('.ph-title h2')).toHaveText(label);

  // Back to the library, scrolled well down.
  await page.locator('[data-nav="photos"]:visible').first().click();
  await page.locator('.ph-body').evaluate((b) => { b.scrollTop = 4000; });
  await page.waitForTimeout(150);
  // A tile in the middle of the screen, so clicking it does not scroll.
  const first = await page.evaluate(() => {
    const b = document.querySelector('.ph-body')!.getBoundingClientRect();
    const t = [...document.querySelectorAll('.ph-tile')].find((x) => { const r = x.getBoundingClientRect(); return r.top > b.top + 80 && r.bottom < b.bottom - 80; }) as HTMLElement | undefined;
    return t ? t.dataset.id || null : null;
  });
  expect(first).toBeTruthy();
  await page.locator('.ph-tile[data-id="' + first + '"]').click();
  const viewer = page.locator('.ph-viewer');
  await expect(viewer).toBeVisible();
  const before = await page.locator('.ph-body').evaluate((b) => b.scrollTop);
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('i');
  const info = page.locator('.ph-info');
  await expect(info).toBeVisible();
  for (const s of ['General', 'Location', 'File', 'Mixpeek']) await expect(info.locator('h4').filter({ hasText: new RegExp('^' + s) })).toBeVisible();
  await expect(info.locator('.ph-src-l.embedded').first()).toHaveText('From the file');
  await expect(info.locator('.ph-src-l.file').first()).toHaveText('File system');
  await expect(info.locator('.ph-map')).toBeVisible();
  await expect(info.locator('details summary')).toContainText('All metadata as read');
  // Similar media is offered.
  await page.keyboard.press('i');
  await expect(page.locator('.ph-v-strip button').first()).toBeVisible();
  // The viewer's own controls are not covered by the dashboard's tab strip.
  const close = page.locator('.ph-viewer [data-act="v-close"]');
  const box = (await close.boundingBox())!;
  const hit = await page.evaluate(([x, y]) => !!document.elementFromPoint(x, y)?.closest('[data-act="v-close"]'), [box.x + box.width / 2, box.y + box.height / 2]);
  expect(hit).toBe(true);
  await page.keyboard.press('Escape');
  await expect(viewer).toHaveCount(0);
  const after = await page.locator('.ph-body').evaluate((b) => b.scrollTop);
  // Back where it was: at most the one row it takes to show the item the
  // viewer ended on (one step on from the one opened).
  expect(Math.abs(after - before)).toBeLessThan(400);
  // Switching away and back keeps the place too.
  await page.locator('[data-nav="explore"]:visible').first().click();
  await page.locator('[data-nav="photos"]:visible').first().click();
  expect(Math.abs((await page.locator('.ph-body').evaluate((b) => b.scrollTop)) - after)).toBeLessThan(5);
});

test('connect Mixpeek: a refused key says so on the key field; a working one connects and the key never reaches the page', async ({ page }) => {
  const posts: any[] = [];
  await openPhotos(page);
  await page.route(/\/api\/photos\/mixpeek$/, async (r) => {
    if (r.request().method() !== 'POST') return r.fallback();
    const b = r.request().postDataJSON();
    posts.push(b);
    if (b.api_key === 'wrong') return r.fulfill({ status: 422, json: { status: 'key_rejected', field: 'api_key', message: 'The instance refused this API key.' } });
    return r.fulfill({ json: { configured: true, status: 'connected', endpoint: b.endpoint, namespace: b.namespace, namespace_name: 'Family photos', key_set: true, message: 'Connected to Family photos.' } });
  });
  await page.locator('.ph-welcome [data-act="connect"]').click();
  const form = page.locator('.ph-sheet');
  await form.locator('input[name="endpoint"]').fill('http://localhost:8000');
  await form.locator('input[name="api_key"]').fill('wrong');
  await form.locator('input[name="namespace"]').fill('ns_photos');
  await expect(form.locator('input[name="endpoint"]')).toHaveValue('http://localhost:8000');
  await form.locator('button[type="submit"]').click();
  await expect(form.locator('.ph-form-msg')).toContainText('refused this API key');
  await expect(form.locator('[data-field="api_key"]')).toHaveClass(/bad/);
  await form.locator('input[name="api_key"]').fill('sk_good_key');
  await form.locator('button[type="submit"]').click();
  await expect(form).toHaveCount(0);
  await expect(page.locator('.ph-banner')).toContainText('Connected to Family photos');
  await expect(page.locator('.ph-tile').first()).toBeVisible();
  expect(posts.map((p) => p.namespace)).toEqual(['ns_photos', 'ns_photos']);
  const leaked = await page.evaluate(() => document.documentElement.innerHTML.includes('sk_good_key') || JSON.stringify(localStorage).includes('sk_good_key'));
  expect(leaked).toBe(false);
});

test('select, favorite and find it in Favorites', async ({ page }) => {
  await openPhotos(page);
  await enterSample(page);
  const t = page.locator('.ph-tile').first();
  const id = await t.getAttribute('data-id');
  await t.hover();
  await t.locator('.ph-check').click();
  await expect(page.locator('.ph-selbar')).toContainText('1 selected');
  await page.locator('[data-act="fav-sel"]').click();
  await page.locator('[data-nav="albums"]:visible').first().click();
  await page.locator('.ph-mosaic', { hasText: 'Favorites' }).click();
  await expect(page.locator('.ph-tile[data-id="' + id + '"]')).toBeVisible();
  expect(await noSideScroll(page)).toBe(true);
});
