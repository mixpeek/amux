// The Proxies tab and the general tunnel are retired (MSG-95893): Pages is the
// one way to share a local app. The tab, its view and its API are gone, an old
// #view=proxies link lands on Pages, and a general tunnel start says where the
// feature went instead of publishing anything.
import { test, expect } from './fixtures';

test('the Proxies tab is gone and its links land on Pages', async ({ page }) => {
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
  expect(api.body.error).toContain('Pages');
});
