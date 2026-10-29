// The Proxies tab and the general tunnel are retired (MSG-95893): the Apps tab
// (named Pages until 2026-10-29) is the one way to share a local app. The tab,
// its view and its API are gone, an old #view=proxies link lands on Apps, and a general tunnel start says where the
// feature went instead of publishing anything.
import { test, expect } from './fixtures';

test('the Proxies tab is gone and its links land on Apps', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
  await expect(page.locator('#tab-proxies')).toHaveCount(0);
  await expect(page.locator('#proxies-view')).toHaveCount(0);
  await expect(page.locator('#tab-customizer-menu [data-tab-id="proxies"]')).toHaveCount(0);

  await page.goto('/#view=proxies');
  for (const h of ['Inside amux', 'Public apps', 'Public files']) await expect(page.getByRole('heading', { name: h })).toBeVisible();
  expect(await page.evaluate(() => (0, eval)('activeView'))).toBe('pages');

  const api = await page.evaluate(async () => {
    const h = Object.assign({ 'Content-Type': 'application/json' }, (window as any)._authHeaders());
    const list = await fetch('/api/proxies', { headers: h });
    const start = await fetch('/api/tunnel/start', { method: 'POST', headers: h, body: '{"port":3000}' });
    return { list: list.status, start: start.status, body: await start.json() };
  });
  expect(api.list).toBe(404);
  expect(api.start).toBe(410);
  expect(api.body.error).toContain('Apps tab');
  expect(api.body.use).toBe('/#view=apps');
});

// The Pages tab was renamed Apps (Ethan, 2026-10-29). #view=apps is its address
// now; #view=pages is kept as an alias because workers and messages already
// carry those links. Both land on the same tab, and the address bar then reads
// the canonical #view=apps.
test('#view=pages and #view=apps both open the Apps tab', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  for (const hash of ['#view=pages', '#view=apps']) {
    await page.goto('/' + hash);
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    await expect(page.locator('#pages-view')).toBeVisible();
    await expect(page.locator('#pages-view .pages-h1')).toHaveText('Apps');
    await expect(page.locator('#tab-pages .tab-lbl')).toHaveText('Apps');
    await expect(page.locator('#tab-pages')).toHaveClass(/\bactive\b/);
    expect(await page.evaluate(() => (0, eval)('activeView'))).toBe('pages');
    await expect.poll(() => page.evaluate(() => location.hash)).toBe('#view=apps');
    // Leave the tab so the next goto is a real navigation, not a no-op.
    await page.goto('/#view=board');
    await expect(page.locator('#pages-view')).toBeHidden();
  }
});
