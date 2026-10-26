// A boot that stalls recovers by itself (Ethan, 2026-10-25, the iPhone app over
// 5G after clearing caches: "Failed to load. Try reloading." with nothing from
// the page for 70s, then a manual Reload loaded it at once). The first app.js
// request hangs here: the screen says it is still loading, not failed; at 20s
// the page reloads itself once and boots; the stall is reported with the file
// that was slow. A second stall after that reload gives up, keeping Reload.
import { test, expect } from './fixtures';

test('a stalled first load reloads itself once and boots', async ({ page }) => {
  test.setTimeout(90_000);
  const beacons: any[] = [];
  await page.route('**/api/client-debug', r => { try { beacons.push(JSON.parse(r.request().postData() || '{}')); } catch (e) {} return r.fulfill({ json: { ok: true } }); });
  let n = 0;
  await page.route(/\/app\.js(\?.*)?$/, async r => {
    if (n++ === 0) return;   // never answered: the stall
    return r.continue();
  });
  await page.goto('/', { waitUntil: 'commit' });
  await expect(page.locator('#js-fallback-msg')).toBeVisible({ timeout: 12_000 });
  await expect(page.locator('#js-fallback-msg')).toHaveText(/Still loading/);
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', null, { timeout: 45_000 });
  await expect(page.locator('#js-fallback')).toBeHidden();
  const stall = beacons.find(b => b.verdict === 'boot_stalled');
  expect(stall, JSON.stringify(beacons.map(b => b.verdict))).toBeTruthy();
  expect(stall.action).toBe('reloaded');
  expect(stall.resources.some((r: any) => r.f.startsWith('/app.js') && r.done === 0)).toBe(true);
});

test('a second stall gives up and keeps Reload, without looping', async ({ page }) => {
  test.setTimeout(90_000);
  const beacons: any[] = [];
  await page.route('**/api/client-debug', r => { try { beacons.push(JSON.parse(r.request().postData() || '{}')); } catch (e) {} return r.fulfill({ json: { ok: true } }); });
  await page.route(/\/app\.js(\?.*)?$/, () => {});   // every request hangs
  await page.goto('/', { waitUntil: 'commit' });
  await expect.poll(() => beacons.filter(b => b.verdict === 'boot_stalled').map(b => b.action), { timeout: 60_000 }).toEqual(['reloaded', 'gave_up']);
  await expect(page.getByRole('button', { name: 'Reload' })).toBeVisible();
  await page.waitForTimeout(3000);
  expect(beacons.filter(b => b.verdict === 'boot_stalled').length, 'no third reload').toBe(2);
});
