// Managing map pins (2026-10-20): status and rating, a dated pin on the
// calendar, a stale edit from another device, categories, the table with bulk
// edits and merge, duplicates, saved lists with a read-only share link, and
// "been" from location history. Pins are rows on the server now
// (map_store.rs), so every check reads the server back, not just the screen.
// The server is shared with other specs: every name carries this run's id and
// every pin made here is deleted afterwards.
import { test, expect, Page } from './fixtures';

const RUN = 'e2e' + Math.random().toString(36).slice(2, 8);
// Far out in the South Pacific, so nothing real is nearby.
const BASE = { lat: -47.10, lng: -128.40 };

// The e2e server wants the owner token; the dashboard holds it.
type Api = { get(u: string): Promise<any>; post(u: string, o?: any): Promise<any>; put(u: string, o?: any): Promise<any> };
async function api(page: Page): Promise<Api> {
  if (page.url() === 'about:blank') {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
  }
  await page.waitForFunction(() => typeof (window as any)._AMUX_AUTH_TOKEN === 'string');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const headers = token ? { Authorization: `Bearer ${token}` } : {};
  return {
    get: (u: string) => page.request.get(u, { headers }),
    post: (u: string, o: any = {}) => page.request.post(u, { ...o, headers }),
    put: (u: string, o: any = {}) => page.request.put(u, { ...o, headers }),
  };
}

async function boot(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  page.on('dialog', d => d.accept());
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mapRenderStatus === 'function');
  await page.evaluate(() => (window as any).switchView('map'));
  // _mapSettings is a top-level let, so it is reached through global eval.
  await page.evaluate(() => (0, eval)("if (!_mapSettings.sidebarOpen) _mapToggleSidebar(); _mapSidebarTab('pins');"));
  await expect(page.locator('#map-pin-list')).toBeVisible();
  // The pin bar's buttons fit the sidebar (the list picker once pushed them out).
  for (const id of ['#map-dup-btn', '#map-table-btn']) await expect(page.locator(id)).toBeInViewport({ ratio: 1 });
}

async function addPin(request: Api, name: string, dLat = 0, extra: any = {}) {
  const r = await request.post('/api/map/pins', { data: { name: `${RUN} ${name}`, lat: BASE.lat + dLat, lng: BASE.lng, tags: [], ...extra } });
  if (!r.ok()) throw new Error(`POST /api/map/pins -> ${r.status()} ${(await r.text()).slice(0, 300)}`);
  return (await r.json()).pin;
}

async function serverPin(request: Api, id: string) {
  const d = await (await request.get('/api/map')).json();
  return d.pins.find((p: any) => p.id === id);
}

async function reload(page: Page) {
  await page.evaluate(() => (window as any)._mapLoad());
  await page.waitForTimeout(400);
}

function item(page: Page, name: string) {
  return page.locator('.map-pin-item', { hasText: `${RUN} ${name}` });
}

test.afterEach(async ({ page }) => {
  if (page.url() === 'about:blank') return;
  const request = await api(page);
  const d = await (await request.get('/api/map')).json();
  const ids = d.pins.filter((p: any) => String(p.name || '').startsWith(RUN)).map((p: any) => p.id);
  if (ids.length) await request.post('/api/map/pins/bulk', { data: { ids, op: 'delete' } });
  const tags = (d.tags || []).filter((t: any) => !String(t.name).startsWith(RUN));
  const tagGroups = (d.tagGroups || []).filter((c: any) => !String(c.name).startsWith(RUN));
  const lists = (d.lists || []).filter((l: any) => !String(l.name).startsWith(RUN));
  await request.put('/api/map/meta', { data: { tags, tagGroups, lists } });
});

test('status, rating and a date set in the editor reach the server, the list and the calendar', async ({ page }) => {
  const request = await api(page);
  // Added from search, so it carries Google's decimal rating (the 2026-10-20
  // import refused a real pin like this one).
  const pin = await addPin(request, 'Gala', 0, { rating: 4.7, ratingCount: 120 });
  await boot(page);
  await item(page, 'Gala').locator('.map-pin-edit-btn').click();
  await page.locator('#map-pin-status').selectOption('favorite');
  await page.locator('#map-pin-rating').selectOption('4');
  await page.locator('#map-pin-event').fill('2026-12-01T19:30');
  await page.locator('#map-pin-modal').getByRole('button', { name: 'Save', exact: true }).click();
  await expect(item(page, 'Gala').locator('.map-status-favorite')).toHaveText('Favorite');
  await expect(item(page, 'Gala').locator('.map-stars')).toHaveAttribute('aria-label', '4 of 5');
  await expect.poll(async () => (await serverPin(request, pin.id))?.rev).toBe(2);
  const saved = await serverPin(request, pin.id);
  expect(saved.status).toBe('favorite');
  expect(saved.myRating).toBe(4);
  expect(saved.rating).toBe(4.7);   // Google's rating survives the editor
  expect(saved.calEventId).toMatch(/^EVT-/);
  const events = await (await request.get('/api/cal-events')).json();
  const ev = (events.events || events).find((e: any) => e.id === saved.calEventId);
  expect(ev?.start).toBe('2026-12-01T19:30');
  // The editor's history names who changed what.
  await item(page, 'Gala').locator('.map-pin-edit-btn').click();
  await page.locator('#map-pin-modal').getByRole('button', { name: 'History' }).click();
  await expect(page.locator('#map-pin-history')).toContainText('update by dashboard');
  await expect(page.locator('#map-pin-history')).toContainText('status');
  await page.locator('#map-pin-modal').getByRole('button', { name: 'Cancel' }).click();
});

test('an edit made on another device is not overwritten by a stale editor', async ({ page }) => {
  const request = await api(page);
  const pin = await addPin(request, 'Stale');
  await boot(page);
  await item(page, 'Stale').locator('.map-pin-edit-btn').click();
  // Another device renames it while this editor is open (rev 1 -> 2).
  const other = await request.put(`/api/map/pins/${pin.id}`, { data: { ...pin, name: `${RUN} Renamed elsewhere`, rev: 1 } });
  expect(other.ok()).toBeTruthy();
  await page.locator('#map-pin-name').fill(`${RUN} Stale edit`);
  await page.locator('#map-pin-modal').getByRole('button', { name: 'Save', exact: true }).click();
  await expect(item(page, 'Renamed elsewhere')).toBeVisible();
  await expect(item(page, 'Stale edit')).toHaveCount(0);
  expect((await serverPin(request, pin.id)).name).toBe(`${RUN} Renamed elsewhere`);
});

test('categories: any group within a category, every category at once', async ({ page }) => {
  const request = await api(page);
  await boot(page);
  const makeTag = async (name: string, category: string) => {
    await page.locator('#map-pins-pane').getByRole('button', { name: '+ Group' }).click();
    await page.locator('#map-tag-name').fill(`${RUN} ${name}`);
    await page.locator('#map-tag-category-new').fill(`${RUN} ${category}`);
    await page.locator('#map-tag-modal').getByRole('button', { name: 'Save', exact: true }).click();
  };
  await makeTag('Ramen', 'Cuisine');
  await makeTag('KBBQ', 'Cuisine');
  await makeTag('Date', 'Vibe');
  await expect.poll(async () => ((await (await request.get('/api/map')).json()).tagGroups || []).filter((c: any) => c.name.startsWith(RUN)).length).toBe(2);
  const meta = await (await request.get('/api/map')).json();
  const tid = (n: string) => meta.tags.find((t: any) => t.name === `${RUN} ${n}`).id;
  await addPin(request, 'Ippudo', 0.001, { tags: [tid('Ramen')] });
  await addPin(request, 'Cote', 0.002, { tags: [tid('KBBQ'), tid('Date')] });
  await addPin(request, 'Ramen date', 0.003, { tags: [tid('Ramen'), tid('Date')] });
  await reload(page);
  const chip = (n: string) => page.locator('#map-tag-list .map-tag-chip', { hasText: `${RUN} ${n}` });
  await expect(page.locator('#map-tag-list .map-tag-cat-name', { hasText: `${RUN} Cuisine` })).toBeVisible();
  await chip('Ramen').click();
  await chip('KBBQ').click();
  for (const n of ['Ippudo', 'Cote', 'Ramen date']) await expect(item(page, n)).toBeVisible();
  await chip('Date').click();
  await expect(item(page, 'Ippudo')).toHaveCount(0);
  await expect(item(page, 'Cote')).toBeVisible();
  await expect(item(page, 'Ramen date')).toBeVisible();
  await page.locator('#map-tag-list .map-tag-chip', { hasText: /^All$/ }).click();
});

test('the table edits many pins at once and merges two into one', async ({ page }) => {
  const request = await api(page);
  const a = await addPin(request, 'Alpha', 0.010, { notes: 'note A' });
  const b = await addPin(request, 'Bravo', 0.011, { notes: 'note B' });
  const c = await addPin(request, 'Charlie', 0.012);
  await boot(page);
  await page.locator('#map-search').fill(RUN);
  await page.locator('#map-table-btn').click();
  const row = (id: string) => page.locator(`.map-table tr[data-pin="${id}"]`);
  await expect(row(a.id)).toBeVisible();
  for (const id of [a.id, b.id, c.id]) await row(id).locator('input[type=checkbox]').check();
  await expect(page.locator('#map-bulk-bar .map-bulk-n')).toHaveText('3 selected');
  await page.locator('#map-bulk-bar select[aria-label="Set status"]').selectOption('been');
  await expect.poll(async () => (await serverPin(request, c.id))?.status).toBe('been');
  expect((await serverPin(request, a.id)).status).toBe('been');
  await expect(row(c.id)).toContainText('Been');
  await row(c.id).locator('input[type=checkbox]').uncheck();
  await expect(page.locator('#map-bulk-bar .map-bulk-n')).toHaveText('2 selected');
  await page.locator('#map-bulk-bar').getByRole('button', { name: 'Merge' }).click();
  await expect.poll(async () => await serverPin(request, b.id)).toBeUndefined();
  const merged = await serverPin(request, a.id);
  expect(merged.notes).toBe('note A\n\nnote B');
  await expect(row(b.id)).toHaveCount(0);
  await page.locator('#map-table-btn').click();
  await page.locator('#map-search').fill('');
});

test('likely duplicates are offered and merged from the Duplicates window', async ({ page }) => {
  const request = await api(page);
  const a = await addPin(request, "Joe's Pizza", 0.020);
  const b = await addPin(request, "Joe's Pizza Broadway", 0.02012);
  await boot(page);
  await page.locator('#map-dup-btn').click();
  const pair = page.locator('.map-dup-pair', { hasText: `${RUN} Joe's Pizza Broadway` });
  await expect(pair).toContainText('similar name nearby');
  await pair.getByRole('button', { name: 'Keep first' }).click();
  await expect.poll(async () => (await serverPin(request, b.id)) === undefined || (await serverPin(request, a.id)) === undefined).toBe(true);
  await expect(page.locator('.map-dup-pair', { hasText: `${RUN} Joe's Pizza` })).toHaveCount(0);
  await page.locator('#map-dup-modal').getByRole('button', { name: 'Close' }).click();
});

test('a saved list can be shared read-only, shows no notes, and the link can be turned off', async ({ page, browser }) => {
  const request = await api(page);
  await addPin(request, 'Want one', 0.030, { status: 'want', notes: 'SECRET-NOTE' });
  await addPin(request, 'Been one', 0.031, { status: 'been' });
  await boot(page);
  await page.locator('#map-search').fill(RUN);
  await page.locator('#map-status-list .map-status-chip[data-status="want"]').click();
  await expect(item(page, 'Want one')).toBeVisible();
  await expect(item(page, 'Been one')).toHaveCount(0);
  await page.locator('#map-list-select').selectOption('__new');
  await page.locator('#map-list-name').fill(`${RUN} To try`);
  await page.locator('#map-list-modal').getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.locator('#map-list-bar')).toBeVisible();
  await page.locator('#map-list-share-btn').click();
  const link = page.locator('#map-list-share-link');
  await expect(link).toBeVisible();
  const url = await link.getAttribute('href');
  // A visitor with no amux session.
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const visitor = await ctx.newPage();
  await visitor.goto(url!);
  await expect(visitor.locator('h1')).toHaveText(`${RUN} To try`);
  await expect(visitor.locator('li', { hasText: `${RUN} Want one` })).toBeVisible();
  await expect(visitor.locator('body')).not.toContainText('Been one');
  await expect(visitor.locator('body')).not.toContainText('SECRET-NOTE');
  await page.locator('#map-list-share-btn').click();
  await expect(page.locator('#map-list-share-status')).toHaveText('Not shared');
  const gone = await visitor.goto(url!);
  expect(gone?.status()).toBe(404);
  await ctx.close();
  await page.locator('#map-list-select').selectOption('');
  await page.locator('#map-search').fill('');
});

// Saving a list sends it to the server without waiting. Sharing right after
// used to reach the server first and get "no saved list" (CI, 2026-10-24); the
// held save below makes that order certain instead of load-dependent.
test('sharing a list that was just saved waits for the save to land', async ({ page }) => {
  const request = await api(page);
  await addPin(request, 'Fresh one', 0.032, { status: 'want' });
  await boot(page);
  await page.locator('#map-search').fill(RUN);
  await expect(item(page, 'Fresh one')).toBeVisible();
  let shareBeforeSave = false, saved = false;
  await page.route(/\/api\/map\/meta$/, async r => {
    if (r.request().method() !== 'PUT') return r.fallback();
    await new Promise(res => setTimeout(res, 1500));
    await r.fallback();
    saved = true;
  });
  page.on('request', q => { if (/\/api\/map\/lists\/.+\/share$/.test(q.url()) && q.method() === 'POST' && !saved) shareBeforeSave = true; });
  await page.locator('#map-list-select').selectOption('__new');
  await page.locator('#map-list-name').fill(`${RUN} Fresh`);
  await page.locator('#map-list-modal').getByRole('button', { name: 'Save', exact: true }).click();
  await page.locator('#map-list-share-btn').click();
  await expect(page.locator('#map-list-share-link')).toBeVisible({ timeout: 10000 });
  expect(shareBeforeSave, 'the share request waited for the list to be stored').toBe(false);
  await page.unrouteAll({ behavior: 'wait' });
  await page.locator('#map-list-share-btn').click();
  await expect(page.locator('#map-list-share-status')).toHaveText('Not shared');
  await page.locator('#map-list-select').selectOption('');
  await page.locator('#map-search').fill('');
});

test('a long visit from location history marks a pin been and keeps a hand-set favorite', async ({ page }) => {
  const request = await api(page);
  const want = await addPin(request, 'Visited', 0.040, { status: 'want' });
  const fav = await addPin(request, 'Fav visited', 0.050, { status: 'favorite' });
  const t = Date.UTC(2026, 0, 15, 18) / 1000;
  const visits = [
    { id: `${RUN}-v1`, arrival: t, departure: t + 1800, lat: BASE.lat + 0.04001, lon: BASE.lng, h_acc: 20 },
    { id: `${RUN}-v2`, arrival: t + 86400, departure: t + 86400 + 3600, lat: BASE.lat + 0.05, lon: BASE.lng, h_acc: 20 },
  ];
  const up = await request.post('/api/map/location/visits', { data: { device: RUN, visits } });
  expect(up.ok()).toBeTruthy();
  // Matching runs after the upload; it also has its own endpoint.
  await expect.poll(async () => (await serverPin(request, want.id))?.status, { timeout: 15000 }).toBe('been');
  const v = await serverPin(request, want.id);
  expect(v.visits.count).toBe(1);
  expect(v.statusSource).toBe('visits');
  expect((await serverPin(request, fav.id)).status).toBe('favorite');
  await boot(page);
  await expect(item(page, 'Visited').locator('.map-status-been')).toBeVisible();
  await expect(item(page, 'Visited')).toContainText('1 visit');
});
