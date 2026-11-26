// Native confirm(), prompt() and alert() are blocked in the installed PWA: the
// iOS app's web view returns from them with no box shown, so the control did
// nothing there (primitive integrity audit 2026-11-26, P2). Every one now asks
// through the dashboard's own dialog. This drives three of them from three
// areas through the real controls, on desktop and the 375px phone: the Disk
// tab's purge (a confirm, then a typed PURGE prompt with no default), a map
// pin's Delete (a confirm raised from inside the map's own modal, which sits
// above the normal dialog layer), and a Files row's Rename and Delete.
//
// The native functions are replaced with counters, so a call that slipped
// back to a native dialog fails here as well as in tests/dashboard_assets.rs.
import { mkdtemp, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, basename } from 'node:path';
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { cleanup } from './teardown';
import { appDialog, answerConfirm, answerPrompt } from './app-dialogs';

const RUN = 'e2e' + Math.random().toString(36).slice(2, 8);

async function boot(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    (window as any).__native = 0;
    window.confirm = () => { (window as any).__native++; return false; };
    window.prompt = () => { (window as any).__native++; return null; };
    window.alert = () => { (window as any).__native++; };
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._AMUX_AUTH_TOKEN === 'string' && typeof (window as any).switchView === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  return token ? { Authorization: `Bearer ${token}` } : {};
}

async function nativeCalls(page: Page) {
  return page.evaluate(() => (window as any).__native as number);
}

// The dialog is the top element where its buttons are: nothing (the map's
// modal, a sheet) is drawn over it.
async function expectOnTop(page: Page) {
  const btn = appDialog(page).locator('#modal-btns button').first();
  await expect(btn).toBeVisible();
  const box = (await btn.boundingBox())!;
  const top = await page.evaluate(([x, y]) => {
    const el = document.elementFromPoint(x, y);
    return !!el && !!el.closest('#modal-backdrop');
  }, [box.x + box.width / 2, box.y + box.height / 2]);
  expect(top, 'the shared dialog is drawn above whatever raised it').toBe(true);
  // The phone keeps the dialog inside the screen.
  const vw = page.viewportSize()!.width;
  const modal = (await appDialog(page).locator('.modal-box').boundingBox())!;
  expect(modal.x).toBeGreaterThanOrEqual(0);
  expect(modal.x + modal.width).toBeLessThanOrEqual(vw + 1);
  if (vw <= 600) expect(box.height, '44px touch target').toBeGreaterThanOrEqual(44);
}

test('Disk: purging staged files asks, then wants PURGE typed; Cancel and a wrong word delete nothing', async ({ page }) => {
  // The reclaim API is stubbed: a real purge here would delete the developer's
  // own quarantined files (as e2e/disk-cleanup-actions.spec.ts does).
  const GB = 1024 ** 3;
  const volume = { measured: true, df_free: 280 * GB, df_total: 1800 * GB, staged_bytes: 9 * GB, staged_batches: 1 };
  const deletes: string[] = [];
  await page.route(/\/api\/reclaim\/scan\/latest/, r => r.fulfill({ json: {
    scan: { id: 'scan-1', status: 'done', finished_at: Math.floor(Date.now() / 1000) - 600, df_free: 4 * GB, df_total: 1800 * GB, dirs_walked: 1, files_walked: 1, bytes_seen: 1 },
    findings: [], totals: [], skipped: [], ownership: { measured: true, n_considered: 0 }, volume_now: volume } }));
  await page.route(/\/api\/reclaim\/quarantine(\?.*)?$/, r => r.fulfill({ json: { root: '/Users/x/.amux/quarantine', items: [],
    batches: [{ id: 'B1', status: 'staged', item_count: 75, bytes: 9 * GB, created_at: 1 }], volume_now: volume } }));
  await page.route(/\/api\/reclaim\/quarantine\/[^/?]+\?confirm=/, r => {
    deletes.push(r.request().method() + ' ' + new URL(r.request().url()).pathname);
    return r.fulfill({ json: { actually_freed: 9 * GB } });
  });
  allowUnusedRoute(page, /\/api\/reclaim\/quarantine\/[^/?]+\?confirm=/);
  await boot(page);
  await page.evaluate(() => (window as any).switchView('disk'));
  const purge = page.locator('#reclaim-purge-all');
  await expect(purge).toBeVisible();

  // The confirm: Cancel deletes nothing.
  await purge.click();
  await expect(appDialog(page).locator('#modal-msg')).toContainText('PERMANENTLY DELETE every quarantined file?');
  await expectOnTop(page);
  await answerConfirm(page, false);
  await expect(appDialog(page)).toBeHidden();

  // Confirmed, then the typed word: an empty box opens, and anything but
  // PURGE (here the lower-case word) cancels, exactly as prompt() did.
  await purge.click();
  await answerConfirm(page, true, 'Delete');
  await expect(appDialog(page).locator('#modal-prompt-input')).toHaveValue('');
  await answerPrompt(page, 'purge');
  await expect(page.locator('#toast')).toContainText('Cancelled');
  await purge.click();
  await answerConfirm(page, true, 'Delete');
  await answerPrompt(page, null);
  await expect(page.locator('#toast')).toContainText('Cancelled');
  expect(deletes).toEqual([]);

  await purge.click();
  await answerConfirm(page, true, 'Delete');
  await answerPrompt(page, 'PURGE');
  await expect.poll(() => deletes).toEqual(['DELETE /api/reclaim/quarantine/B1']);
  expect(await nativeCalls(page)).toBe(0);
});

test('Map: a pin\'s Delete asks above the pin editor; Cancel keeps the pin', async ({ page }, info) => {
  const headers = await boot(page);
  const r = await page.request.post('/api/map/pins', { headers, data: { name: `${RUN} pin`, lat: -47.2, lng: -128.5, tags: [] } });
  expect(r.ok(), await r.text()).toBeTruthy();
  const pin = (await r.json()).pin;
  const onServer = async () => ((await (await page.request.get('/api/map', { headers })).json()).pins as any[]).some(p => p.id === pin.id);
  try {
    await page.waitForFunction(() => typeof (window as any)._mapRenderStatus === 'function');
    await page.evaluate(() => (window as any).switchView('map'));
    await page.evaluate(() => (0, eval)("if (!_mapSettings.sidebarOpen) _mapToggleSidebar(); _mapSidebarTab('pins');"));
    await page.locator('#map-search').fill(RUN);
    const item = page.locator('.map-pin-item', { hasText: `${RUN} pin` });
    await expect(item).toBeVisible();

    await item.locator('.map-pin-edit-btn').click();
    await expect(page.locator('#map-pin-modal')).toBeVisible();
    await page.locator('#map-pin-delete-btn').click();
    await expect(appDialog(page).locator('#modal-msg')).toHaveText('Delete this pin?');
    await expectOnTop(page);
    await answerConfirm(page, false);
    await expect(appDialog(page)).toBeHidden();
    await expect(page.locator('#map-pin-modal'), 'Cancel leaves the editor open').toBeVisible();
    expect(await onServer()).toBe(true);

    await page.locator('#map-pin-delete-btn').click();
    await answerConfirm(page, true, 'Delete');
    await expect(page.locator('#map-pin-modal')).toBeHidden();
    await expect.poll(onServer).toBe(false);
    await expect(item).toHaveCount(0);
    expect(await nativeCalls(page)).toBe(0);
  } finally {
    await cleanup('delete the test pin', () => page.request.post('/api/map/pins/bulk', { headers, data: { ids: [pin.id], op: 'delete' } }), info);
  }
});

test('Files: a row\'s Rename opens on the current name and Delete asks first', async ({ page }, info) => {
  test.setTimeout(90_000);
  const dir = await mkdtemp(join(tmpdir(), 'amux-e2e-dialogs-'));
  await writeFile(join(dir, 'draft.txt'), 'hello');
  try {
    await boot(page);
    // files_cwd is server-wide and other specs set it in parallel; this page
    // alone reads this test's folder (as lifecycle/files-upload.spec.ts does).
    await page.route(u => u.pathname === '/api/prefs' && u.searchParams.get('key') === 'files_cwd',
      route => route.request().method() === 'GET' ? route.fulfill({ json: { key: 'files_cwd', value: dir } }) : route.continue());
    await page.reload();
    await page.locator('#tab-files').click();
    await expect(page.locator('#files-breadcrumb')).toContainText(basename(dir));
    const row = (n: string) => page.locator('#files-body .fe-row').filter({ hasText: n });
    await expect(row('draft.txt')).toBeVisible();
    const menu = page.locator('.explore-menu-popup');

    // Rename: the box holds the old name; Cancel renames nothing.
    await row('draft.txt').getByTitle('Options', { exact: true }).click();
    await menu.getByRole('button', { name: 'Rename', exact: true }).click();
    await expect(appDialog(page).locator('#modal-prompt-input')).toHaveValue('draft.txt');
    await expectOnTop(page);
    await answerPrompt(page, null);
    await expect(appDialog(page)).toBeHidden();
    expect(await readdir(dir)).toEqual(['draft.txt']);
    await row('draft.txt').getByTitle('Options', { exact: true }).click();
    await menu.getByRole('button', { name: 'Rename', exact: true }).click();
    await answerPrompt(page, 'final.txt');
    await expect(row('final.txt')).toBeVisible();
    await expect.poll(() => readdir(dir)).toEqual(['final.txt']);

    // Delete: Cancel keeps the file, Delete removes it.
    await row('final.txt').getByTitle('Options', { exact: true }).click();
    await menu.getByRole('button', { name: 'Delete file', exact: true }).click();
    await expect(appDialog(page).locator('#modal-msg')).toHaveText('Delete "final.txt"? This cannot be undone.');
    await answerConfirm(page, false);
    await expect(appDialog(page)).toBeHidden();
    expect(await readdir(dir)).toEqual(['final.txt']);
    await row('final.txt').getByTitle('Options', { exact: true }).click();
    await menu.getByRole('button', { name: 'Delete file', exact: true }).click();
    await answerConfirm(page, true, 'Delete');
    await expect(row('final.txt')).toHaveCount(0);
    await expect.poll(() => readdir(dir)).toEqual([]);
    expect(await nativeCalls(page)).toBe(0);
  } finally {
    await cleanup('remove the temp dir', () => rm(dir, { recursive: true, force: true }), info);
  }
});
