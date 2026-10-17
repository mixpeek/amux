// Maps stopwatch for run time trials (AMUX-6114; Ethan, 2026-10-16: "add a
// stopwatch thing to the maps tab and that fulfills our time trial use case").
// Driven through the real UI; GPS arrives the way the phone sends it, as owner
// writes to /api/map/location/points.
import { test, expect, Page } from './fixtures';
import { boot } from './lifecycle/evidence';

type Win = Window & { switchView: (v: string) => void; _AMUX_AUTH_TOKEN: string };

async function openMap(page: Page) {
  await boot(page);
  await page.evaluate(() => (window as unknown as Win).switchView('map'));
  await expect(page.locator('#map-container')).toBeVisible();
}

test('Maps stopwatch: a trial is timed, measured from GPS, saved, and its route opens', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  await openMap(page);
  const token = await page.evaluate(() => (window as unknown as Win)._AMUX_AUTH_TOKEN);
  const auth = { Authorization: `Bearer ${token}` };

  await page.locator('#map-sw-btn').click();
  const panel = page.locator('#map-sw');
  await expect(panel).toBeVisible();
  await expect(page.locator('#map-sw-btn')).toHaveAttribute('aria-pressed', 'true');
  await page.locator('#map-sw-start').click();
  await expect(page.locator('#map-sw-stop')).toBeVisible();
  await expect(page.locator('#map-sw-start')).toBeHidden();
  const start = await page.evaluate(() => JSON.parse(localStorage.getItem('amux_map_sw') || 'null')?.start as number);
  expect(start).toBeGreaterThan(0);

  // Four fixes ~100m apart along a meridian, plus one wild fix (h_acc 200m)
  // 2km away that must not be counted.
  const base = { lat: 40.7, lon: -74.0 };
  const pts = [0, 1, 2, 3].map(i => ({ id: `sw-${info.project.name}-${i}`, ts: start + 0.2 + i * 0.2, lat: base.lat + i * 0.0009, lon: base.lon, h_acc: 5, source: 'live' }));
  pts.splice(2, 0, { id: `sw-${info.project.name}-wild`, ts: start + 0.5, lat: base.lat + 0.018, lon: base.lon, h_acc: 200, source: 'live' });
  const r = await request.post('/api/map/location/points', { headers: auth, data: { device: `e2e-${info.project.name}`, points: pts } });
  expect(r.status(), await r.text()).toBeLessThan(300);

  // A running trial survives a reload of the app.
  await page.waitForTimeout(1200);
  await page.reload();
  await openMap(page);
  await expect(page.locator('#map-sw')).toBeVisible();
  await expect(page.locator('#map-sw-stop')).toBeVisible();
  await expect(page.locator('#map-sw-time')).not.toHaveText('0:00');

  await page.locator('#map-sw-lap').click();
  await expect(page.locator('#map-sw-laps li')).toHaveCount(1);
  await page.locator('#map-sw-stop').click();
  await expect(page.locator('#map-sw-start')).toBeVisible();
  await expect(page.locator('#map-sw-past li')).toHaveCount(1);
  const trial = await page.evaluate(() => (0, eval)('_mapTrials')[0]);
  expect(trial.distance_m, 'four good fixes ~100m apart; the 200m-accuracy fix is ignored').toBeGreaterThan(280);
  expect(trial.distance_m).toBeLessThan(320);
  expect(trial.splits.length, 'one lap plus the final segment').toBe(2);
  await expect(page.locator('#map-sw-past')).toContainText('0.30 km');

  // Saved in the map document: survives a reload from the server.
  // The save is fire-and-forget from the page, so wait for it to land.
  await expect.poll(async () => {
    const doc = await (await request.get('/api/map', { headers: auth })).json();
    return (doc.trials || []).some((t: any) => t.id === trial.id);
  }, { timeout: 15_000 }).toBe(true);
  await page.reload();
  await openMap(page);
  await page.locator('#map-sw-btn').click();
  await expect(page.locator('#map-sw-past li')).toHaveCount(1);

  // Route: the Location history view of the trial's window.
  await page.locator('#map-sw-past li .btn').first().click();
  await expect(page.locator('#map-tab-history')).toHaveAttribute('aria-selected', 'true');
  expect(await page.evaluate(() => (0, eval)('_locRange'))).toEqual({ from: Math.floor(trial.start), to: Math.ceil(trial.end) });

  const box = await panel.boundingBox();
  const vw = page.viewportSize()!.width;
  expect(box!.x + box!.width, 'the panel fits the screen').toBeLessThanOrEqual(vw);
  await page.screenshot({ path: info.outputPath('map-stopwatch.png') });
});
