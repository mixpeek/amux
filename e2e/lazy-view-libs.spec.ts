// Performance pass, 2026-10-19: sixteen CDN libraries loaded as blocking
// scripts on every page load, so DOMContentLoaded waited on Chart.js, SheetJS,
// gridstack, Leaflet and FullCalendar even though each serves one view. They
// now load on first use; Quill (its notes view was removed) is gone.
import { test, expect } from './fixtures';

const LAZY = /chart\.umd|xlsx\.full|gridstack|leaflet|fullcalendar|quill/;

test('view-only libraries stay off the page load and arrive when their view opens', async ({ page }) => {
  test.setTimeout(90_000);
  const fetched: string[] = [];
  page.on('request', r => { if (LAZY.test(r.url())) fetched.push(r.url()); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.waitForLoadState('load');
  expect(fetched).toEqual([]);
  expect(await page.evaluate(() => [typeof (window as any).Chart, typeof (window as any).L, typeof (window as any).Quill]))
    .toEqual(['undefined', 'undefined', 'undefined']);

  await page.evaluate(() => (window as any).switchView('calendar'));
  await expect(page.locator('#fc-container.fc')).toBeVisible({ timeout: 30_000 });
  expect(fetched.some(u => /fullcalendar/.test(u))).toBe(true);

  await page.evaluate(() => (window as any).switchView('map'));
  await expect(page.locator('#map-container.leaflet-container')).toBeVisible({ timeout: 30_000 });
  // The cluster plugin rides with Leaflet, so ~67 overlapping pins still cluster.
  expect(await page.evaluate(() => typeof (window as any).L.markerClusterGroup)).toBe('function');

  // Opening the same view again does not reload anything.
  const n = fetched.length;
  await page.evaluate(() => (window as any).switchView('calendar'));
  await expect(page.locator('#fc-container.fc')).toBeVisible();
  expect(fetched.length).toBe(n);
});

test('a library that fails to load is reported and retried on the next open', async ({ page }) => {
  test.setTimeout(60_000);
  let block = true;
  const beacons: any[] = [];
  await page.route(/fullcalendar/, route => block ? route.abort() : route.fallback());
  await page.route('**/api/client-debug', route => {
    try { beacons.push(route.request().postDataJSON()); } catch (e) {}
    return route.fulfill({ status: 200, body: '{}' });
  });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('calendar'));
  await expect.poll(() => beacons.find(b => b.kind === 'lazy-lib')).toMatchObject({ verdict: 'lazy_lib_failed', lib: 'fullcalendar', measured: true });
  block = false;
  await page.evaluate(() => { (window as any).switchView('sessions'); (window as any).switchView('calendar'); });
  await expect(page.locator('#fc-container.fc')).toBeVisible({ timeout: 30_000 });
});
