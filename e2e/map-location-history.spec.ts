// Map > Location history (AMUX-5458). The timeline is routed to a fixed fake
// day so no real location data is read or written.
import { test, expect, Page } from './fixtures';

// Recorder controls and Raw export sit in a collapsed <details> at the bottom
// of the pane (5e35f6c3); open it before reaching for them.
async function openLocSettings(page: import('@playwright/test').Page) {
  const acc = page.locator('#map-loc-settings');
  if (!(await acc.evaluate(e => (e as HTMLDetailsElement).open))) await acc.locator('summary').click();
}

// Open the Map sidebar and keep it open until its tabs are on screen. The map
// applies settings that arrive from the server after load, which on a phone
// can collapse the sidebar again (translateX off screen, still "visible").
async function openMapSidebar(page: Page): Promise<void> {
  await page.evaluate(() => (window as any).switchView('map'));
  await page.waitForFunction(() => (globalThis as any).eval('_mapServerLoaded') === true);
  // The saved settings arriving must not leave the setting and the screen
  // disagreeing (it said "open" over a closed phone sidebar).
  const agree = await page.evaluate(() => {
    const open = (globalThis as any).eval('_mapSettings').sidebarOpen;
    const sb = document.getElementById('map-sidebar')!;
    const shown = !sb.classList.contains('hidden') && getComputedStyle(sb).display !== 'none';
    return open === shown;
  });
  expect(agree, 'map sidebar setting disagrees with the screen after load').toBe(true);
  await expect.poll(async () => page.evaluate(() => {
    const g: any = globalThis;
    if (!g.eval('_mapSettings').sidebarOpen) g._mapToggleSidebar();
    // A display:none element measures 0x0, which would read as "on screen".
    const r = document.getElementById('map-tab-history')!.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && r.left >= 0 && r.right <= window.innerWidth;
  }), {timeout: 15000, intervals: [400]}).toBe(true);
}

const DAY = {
  ok: true, measured: true, n_considered: 212, visits_considered: 1, from: 0, to: 0,
  segments: [
    {id: 'stop_home', kind: 'stop', start: 1790841600, end: 1790866800, duration_s: 25200, lat: 40.7411, lon: -73.9897, point_count: 12},
    {id: 'trip_w1', kind: 'trip', mode: 'walking', mode_confidence: 'measured', start: 1790866800, end: 1790867700,
      duration_s: 900, distance_m: 1180, from: [40.7411, -73.9897], to: [40.7505, -73.9934],
      bbox: [40.7411, -73.9934, 40.7505, -73.9897], point_count: 90,
      path: [[40.7411, -73.9897], [40.7450, -73.9910], [40.7505, -73.9934]]},
    {id: 'trip_t1', kind: 'trip', mode: 'train', mode_confidence: 'inferred', start: 1790868000, end: 1790869800,
      duration_s: 1800, distance_m: 21400, from: [40.7505, -73.9934], to: [40.9000, -73.9000],
      bbox: [40.7505, -73.9934, 40.9000, -73.9000], point_count: 110,
      path: [[40.7505, -73.9934], [40.8200, -73.9500], [40.9000, -73.9000]]},
  ],
};

test('Map > Location history lists a day, draws it, and focuses a trip', async ({ page }) => {
  let asked = '';
  await page.route('**/api/map/location/timeline**', async route => {
    asked = route.request().url();
    await route.fulfill({json: DAY});
  });
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_map_tab'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function' && typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  const tab = page.locator('#map-tab-history');
  await expect(tab).toBeVisible();
  await expect(page.locator('#map-pins-pane')).toBeVisible();
  await tab.click();
  await expect(tab).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#map-pins-pane')).toBeHidden();
  // The overview is the default; the day timeline is one tap away.
  await page.locator('#map-loc-open-day').click();
  await expect(page.locator('#map-loc-daypane')).toBeVisible();
  await expect(page.locator('#map-loc-summary')).toContainText('212 points');
  await expect(page.locator('#map-loc-summary')).toContainText('2 trips');
  const rows = page.locator('.map-loc-row');
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(2)).toContainText('Train (inferred)');
  // The request asks for one local day.
  const u = new URL(asked);
  expect(Number(u.searchParams.get('to')) - Number(u.searchParams.get('from'))).toBeGreaterThanOrEqual(23 * 3600);
  // Two trips drawn as lines plus the stop and the start/end markers.
  // Leaflet loads on the first Map open, so the drawing can land just after the rows.
  await expect.poll(() => page.evaluate(() => { const g: any = globalThis; const l = g.eval('_locLayer'); return l ? l.getLayers().length : 0; }),
    { timeout: 30_000 }).toBeGreaterThanOrEqual(5);
  const box = await rows.nth(1).boundingBox();
  if ((page.viewportSize()?.width || 1200) <= 600) expect(box!.height).toBeGreaterThanOrEqual(44);
  // Focusing a trip must not rewrite the saved map document (it once saved
  // sidebarOpen:false, so every later load started with the sidebar shut).
  let mapSaves = 0;
  page.on('request', req => { if (req.method() === 'POST' && req.url().includes('/api/map?replace=1')) mapSaves++; });
  await rows.nth(1).click();
  if ((page.viewportSize()?.width || 1200) <= 600) {
    // On a phone the sidebar gets out of the way so the focused trip is visible.
    await expect.poll(() => page.evaluate(() => (globalThis as any).eval('_mapSettings').sidebarOpen)).toBe(false);
    await page.waitForTimeout(400);
    expect(mapSaves, 'focusing a trip saved the map settings').toBe(0);
  } else {
    await expect(rows.nth(1)).toHaveClass(/active/);
  }
  // Previous day asks again for a different window.
  const before = asked;
  await openMapSidebar(page);
  await page.locator('.map-loc-step').first().click();
  await expect.poll(() => asked).not.toBe(before);
});

test('Map > Location history says so when a day has nothing, and shows the iPhone bridge status', async ({ page }) => {
  await page.route('**/api/map/location/timeline**', route => route.fulfill({json: {ok: true, measured: true, n_considered: 0, segments: []}}));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_map_tab', 'history'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  await page.evaluate(() => (window as any)._mapSidebarTab('history'));
  await page.evaluate(() => (window as any)._locShowDay());
  await expect(page.locator('#map-loc-list .map-loc-empty')).toContainText('Location history');
  // The native app calls back with its status; the switch and counts render.
  await page.evaluate(() => (window as any).__amuxNativeLocation({enabled: true, authorization: 'whenInUse', precise: false, motion: 'on', pending: 7}));
  // 5e35f6c3: the recorder controls live in the collapsed Settings section.
  await openLocSettings(page);
  const native = page.locator('#map-loc-native');
  await expect(native).toBeVisible();
  await expect(native).toContainText('While using the app only');
  await expect(native).toContainText('approximate');
  await expect(native).toContainText('Waiting to upload: 7');
  await expect(native.getByRole('button', {name: 'Turn off'})).toBeVisible();
});

const STATS = {
  ok: true, measured: true, n_considered: 5400, days_with_data: 3, bucket: 'day',
  totals: {
    walking: {distance_m: 1609.344 * 12.5, moving_s: 4400, trips: 5, avg_speed_mps: 1.41},
    driving: {distance_m: 1609.344 * 300, moving_s: 3600, trips: 2, avg_speed_mps: 13.3},
    running: {distance_m: 1609.344 * 9, moving_s: 3000, trips: 3, avg_speed_mps: 3},
    train: {distance_m: 1609.344 * 8.5, moving_s: 900, trips: 1, avg_speed_mps: 15},
  },
  buckets: [], top_places: [], new_places: [], places_considered: 7, longest_trip: null,
  areas: [
    {key: 'a_1', name: 'New York, NY', name_status: 'named', visits: 1842, time_s: 9e5, lat: 40.71, lon: -73.99},
    {key: 'a_2', name: null, name_status: 'pending', visits: 312, time_s: 2e5, lat: 47.6, lon: -122.3},
    {key: 'a_3', name: 'Hudson Valley, NY', name_status: 'named', visits: 188, time_s: 1e5, lat: 41.7, lon: -74.0},
    {key: 'a_4', name: 'Los Angeles, CA', name_status: 'named', visits: 142, time_s: 9e4, lat: 34.05, lon: -118.24},
    {key: 'a_5', name: 'San Francisco, CA', name_status: 'named', visits: 128, time_s: 8e4, lat: 37.77, lon: -122.42},
    {key: 'a_6', name: 'Boston, MA', name_status: 'named', visits: 40, time_s: 5e4, lat: 42.36, lon: -71.06},
  ],
  areas_considered: 6,
};
const HEAT = {ok: true, measured: true, n_considered: 900, cell_deg: 0.00045, n_cells: 4, truncated: false,
  points_by_mode: {driving: 600, walking: 250, still: 50},
  cells: [[40.7411, -73.9897, 600, 'driving'], [40.75, -73.99, 250, 'walking'], [40.9, -73.9, 50, 'still'], [40.76, -73.98, 20, 'driving']]};

test('Map > Location history overview: mode switches, period, distances, top places and a per-mode heatmap', async ({ page }) => {
  const statsAsked: string[] = [], heatAsked: string[] = [];
  await page.route('**/api/map/location/stats**', async route => {
    statsAsked.push(route.request().url());
    await route.fulfill({json: STATS});
  });
  await page.route('**/api/map/location/heatmap**', async route => { heatAsked.push(route.request().url()); await route.fulfill({json: HEAT}); });
  await page.route('**/api/map/location/export**', route => route.fulfill({body: 'id,ts\na,1\nb,2\n', contentType: 'text/csv',
    headers: {'content-disposition': 'attachment; filename="amux-location-1-2.csv"', 'x-amux-rows': '2', 'x-amux-truncated': '0'}}));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_map_tab', 'history');
    localStorage.removeItem('amux_loc_period'); localStorage.removeItem('amux_loc_modes_off'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  await page.evaluate(() => (window as any)._mapSidebarTab('history'));
  // Default period is All; the request spans ten years.
  await expect(page.locator('#map-loc-seg button[data-p="all"]')).toHaveAttribute('aria-checked', 'true');
  await expect.poll(() => statsAsked.length).toBeGreaterThan(0);
  let q = new URL(statsAsked.at(-1)!).searchParams;
  expect(Number(q.get('to')) - Number(q.get('from'))).toBe(3650 * 86400);
  // Mode switches: Drive, Walk, Bike always; Run, Train and Stays because this
  // period has them.
  const modes = page.locator('#map-loc-modes [role="switch"]');
  await expect(modes).toHaveCount(6);
  await expect(page.locator('#map-loc-modes')).toContainText('Drive');
  await expect(page.locator('#map-loc-modes')).toContainText('Bike');
  // Distances in the reader's units (miles for en-US), "none" for a mode with nothing.
  const dist = page.locator('#map-loc-dist');
  await expect(dist.locator('.map-loc-dist-row').filter({hasText: 'Drive'})).toContainText('300 mi');
  await expect(dist.locator('.map-loc-dist-row').filter({hasText: 'Walk'})).toContainText('12.5 mi');
  await expect(dist.locator('.map-loc-dist-row').filter({hasText: 'Bike'})).toContainText('none');
  // On a phone the first screen holds the period, the distances and the top of
  // Top Places: six mode rows once filled it (Ethan: conservative real estate).
  if ((page.viewportSize()?.width || 1200) <= 600) {
    const fit = await page.evaluate(() => {
      const ov = document.getElementById('map-loc-overview')!; ov.scrollTop = 0;
      const bottom = ov.getBoundingClientRect().bottom;
      const below = (sel: string) => document.querySelector(sel)!.getBoundingClientRect().bottom <= bottom + 0.5;
      return {seg: below('#map-loc-seg'), dist: below('#map-loc-dist'), place: below('#map-loc-places .map-loc-place')};
    });
    expect(fit).toEqual({seg: true, dist: true, place: true});
  }
  // Top places: five, named or pending, then All Places.
  const places = page.locator('#map-loc-places .map-loc-place');
  await expect(places).toHaveCount(5);
  await expect(places.first()).toContainText('New York, NY');
  await expect(places.first()).toContainText('1,842 visits');
  await expect(places.nth(1)).toContainText('Naming this area');
  await page.locator('#map-loc-allplaces').click();
  await expect(places).toHaveCount(6);
  // Heatmap: one dot per cell, a legend per mode shown.
  const dots = () => page.evaluate(() => { const l = (globalThis as any).eval('_locHeatLayer'); return l ? l.getLayers().length : 0; });
  await expect.poll(dots).toBe(4);
  await expect(page.locator('#map-loc-legend .map-loc-legend-row')).toHaveCount(3);
  await expect(page.locator('#map-loc-legend')).toContainText('More activity');
  // Every mode row is a full-height tap target that is not overlapped: on a
  // short phone the cards once shrank and the rows sat over the period control.
  for (const r of await page.locator('#map-loc-modes [role="switch"]').all()) {
    const hit = await r.evaluate(el => { el.scrollIntoView({block: 'center'}); const b = el.getBoundingClientRect();
      return {h: b.height, own: el.contains(document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2))}; });
    expect(hit.h).toBeGreaterThanOrEqual(44);
    expect(hit.own).toBe(true);
  }
  // Switching Drive off filters the distances and the heatmap.
  await page.locator('#map-loc-modes [data-mode="driving"]').click();
  await expect(page.locator('#map-loc-modes [data-mode="driving"]')).toHaveAttribute('aria-checked', 'false');
  await expect(dist).not.toContainText('Drive');
  await expect.poll(dots).toBe(2);
  await expect(page.locator('#map-loc-legend .map-loc-legend-row')).toHaveCount(2);
  // Period: 7D asks for seven days.
  await page.locator('#map-loc-seg button[data-p="7d"]').click();
  await expect.poll(() => new URL(statsAsked.at(-1)!).searchParams.get('from')).not.toBe(q.get('from'));
  q = new URL(statsAsked.at(-1)!).searchParams;
  expect(Number(q.get('to')) - Number(q.get('from'))).toBe(7 * 86400);
  expect(new URL(heatAsked.at(-1)!).searchParams.get('from')).toBe(q.get('from'));
  // Export lives in the Settings section at the bottom and goes through fetch.
  await openLocSettings(page);
  const exported = page.waitForRequest(r => r.url().includes('/api/map/location/export'));
  await page.locator('.map-loc-export').getByRole('button', {name: 'CSV'}).click();
  expect(new URL((await exported).url()).searchParams.get('format')).toBe('csv');
  await expect(page.getByText('Exported 2 raw points')).toBeVisible();
});

test('Map > Location history overview says when a period has nothing', async ({ page }) => {
  await page.route('**/api/map/location/stats**', route => route.fulfill({json: {ok: true, measured: true, n_considered: 0, days_with_data: 0,
    totals: {}, buckets: [], top_places: [], new_places: [], places_considered: 0, areas: [], areas_considered: 0}}));
  await page.route('**/api/map/location/heatmap**', route => route.fulfill({json: {ok: true, measured: true, n_considered: 0, cells: [], n_cells: 0, points_by_mode: {}}}));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_map_tab', 'history'); localStorage.setItem('amux_loc_period', '7d'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  await page.evaluate(() => (window as any)._mapSidebarTab('history'));
  await expect(page.locator('#map-loc-dist')).toHaveText('Nothing recorded in the last 7 days.');
  await expect(page.locator('#map-loc-places')).toContainText('No places in the last 7 days.');
  await expect(page.locator('#map-loc-measured')).toHaveText('No points in the last 7 days.');
  await expect(page.locator('#map-loc-legend')).toHaveCount(0);
});

test('Map > Location history shows the iPhone recording detail and delivered vs stored', async ({ page }) => {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_map_tab', 'history'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  await page.evaluate(() => (window as any)._mapSidebarTab('history'));
  await page.evaluate(() => (window as any).__amuxNativeLocation({enabled: true, authorization: 'always', precise: true, motion: 'on',
    pending: 0, mode: 'full', delivered: 1200, stored: 1198}));
  // 5e35f6c3: the recorder controls live in the collapsed Settings section.
  await openLocSettings(page);
  const native = page.locator('#map-loc-native');
  await expect(native).toContainText('Fixes delivered / stored: 1200 / 1198');
  await expect(native).toContainText('2 not stored');
  await expect(native.getByRole('button', {name: 'Full detail'})).toHaveAttribute('aria-pressed', 'true');
  await page.evaluate(() => (window as any).__amuxNativeLocation({enabled: true, authorization: 'always', mode: 'saver', delivered: 5, stored: 5}));
  await expect(native.getByRole('button', {name: 'Battery saver'})).toHaveAttribute('aria-pressed', 'true');
  await expect(native).not.toContainText('not stored');
});

// AMUX-5550: a day whose live fixes stopped (2026-10-03) read as stops back to
// back. The server now returns a "gap" between two places with no fixes, and
// the timeline must say the move happened and was not recorded.
test('Map > Location history shows an unrecorded move as a gap row and a dashed line', async ({ page }) => {
  const GAP_DAY = {
    ok: true, measured: true, n_considered: 4, visits_considered: 2, from: 0, to: 0,
    segments: [
      {id: 'stop_visit_a', kind: 'stop', start: 1791043856, end: 1791051308, duration_s: 7452, lat: 40.73492, lon: -74.00258, point_count: 1},
      {id: 'gap_stop_visit_a', kind: 'gap', start: 1791051308, end: 1791051309, duration_s: 1,
        from: [40.73492, -74.00258], to: [40.73852, -74.00286], distance_m: 401, point_count: 0},
      {id: 'stop_visit_b', kind: 'stop', start: 1791051309, end: 1791052051, duration_s: 742, lat: 40.73852, lon: -74.00286, point_count: 0},
    ],
  };
  await page.route('**/api/map/location/timeline**', route => route.fulfill({json: GAP_DAY}));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_map_tab'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function' && typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  await page.locator('#map-tab-history').click();
  await page.locator('#map-loc-open-day').click();
  await expect(page.locator('#map-loc-summary')).toContainText('1 move not recorded');
  const rows = page.locator('.map-loc-row');
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(1)).toContainText('Moved, not recorded');
  await expect(rows.nth(1)).toContainText('phone sent no GPS');
  // Drawn as a dashed line, never as a measured trip.
  await expect.poll(() => page.evaluate(() => {
    const l = (globalThis as any).eval('_locLayer');
    return l ? l.getLayers().filter((x: any) => x.options && x.options.dashArray).length : -1;
  }), { timeout: 30_000 }).toBe(1);
});

// 2026-10-04: an overnight stop read "12:10 PM–12:41 PM · 24 h 31 min" with
// no date, and an empty distance left "0 trips · · 0 min moving".
test('Map > Location history dates an edge on another day and never prints an empty distance', async ({ page }) => {
  const start = new Date(); start.setHours(0, 0, 0, 0);
  const t0 = start.getTime() / 1000;
  const DAY2 = {
    ok: true, measured: true, n_considered: 2, visits_considered: 2, from: 0, to: 0,
    segments: [
      {id: 'stop_visit_overnight', kind: 'stop', start: t0 - 3 * 3600, end: t0 + 10 * 3600, duration_s: 13 * 3600, lat: 40.7373, lon: -74.0086, point_count: 0},
      {id: 'stop_visit_morning', kind: 'stop', start: t0 + 10.5 * 3600, end: t0 + 11 * 3600, duration_s: 1800, lat: 40.7372, lon: -74.0084, point_count: 0},
    ],
  };
  await page.route('**/api/map/location/timeline**', route => route.fulfill({json: DAY2}));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_map_tab'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function' && typeof (window as any)._mapSidebarTab === 'function');
  await openMapSidebar(page);
  await page.locator('#map-tab-history').click();
  await page.locator('#map-loc-open-day').click();
  const yesterday = new Date((t0 - 3 * 3600) * 1000).toLocaleDateString([], {month: 'short', day: 'numeric'});
  const rows = page.locator('.map-loc-row');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0)).toContainText(yesterday + ',');
  await expect(rows.nth(1)).not.toContainText(yesterday);
  const summary = await page.locator('#map-loc-summary').textContent();
  expect(summary, 'no empty slot between separators').not.toMatch(/·\s*·/);
  expect(summary).toMatch(/0 (mi|km)/);
});
