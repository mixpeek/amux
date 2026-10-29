// Ethan, 2026-10-29: pin file shortcuts for a worker, its groups or every
// worker from the shortcuts row, drop the "<worker>:" label that led the row,
// and give a worker a Files tab that opens its directory (its path stays).
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

const W = 'e2e-probe';

async function boot(page: import('@playwright/test').Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openExplore === 'function' && typeof (window as any).openPeek === 'function', { timeout: 20000 });
}
const pref = (page: import('@playwright/test').Page, key: string) =>
  page.evaluate(async k => { const d = await (await fetch('/api/prefs?key=' + encodeURIComponent(k))).json(); return d && d.value ? JSON.parse(d.value) : []; }, key);
const setPref = (page: import('@playwright/test').Page, key: string, value: string) =>
  page.evaluate(async ([k, v]) => { await fetch('/api/prefs', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ key: k, value: v }) }); }, [key, value] as const);

test('the shortcuts row has no scope label, and + pins for the worker or for every worker', async ({ page }, info) => {
  const dir = '/tmp';
  const own = 'files_bookmarks::' + W;
  const before = { own: await (async () => { await boot(page); return JSON.stringify(await pref(page, own)); })() };
  try {
    await page.evaluate(([d, w]) => (window as any).openExplore(d, w), [dir, W]);
    await expect(page.locator('#files-bm-add')).toBeVisible();
    await expect(page.locator('#files-bm-label'), 'no "<worker>:" label leads the row').toHaveCount(0);

    // + in a worker's view asks which scope, nearest first.
    await page.locator('#files-bm-add').click();
    const menu = page.locator('#files-pin-menu');
    await expect(menu).toBeVisible();
    const items = await menu.locator('.explore-menu-item').allTextContents();
    expect(items[0]).toBe('Pin for ' + W);
    expect(items[items.length - 1]).toBe('Pin for all workers');
    await page.screenshot({ path: info.outputPath('pin-menu.png') });
    await menu.getByText('Pin for ' + W, { exact: true }).click();
    await expect.poll(async () => (await pref(page, own)).map((b: any) => b.path)).toContain(dir);
    await expect(page.locator('#files-bookmarks-list .files-bm-chip').filter({ hasText: 'tmp' }).first()).toBeVisible();
  } finally {
    await cleanup('restore the worker pin pref', () => setPref(page, own, before.own === '[]' ? '' : before.own), info);
  }
});

test('a worker has a Files tab that opens its directory, and its path still opens it too', async ({ page }, info) => {
  await boot(page);
  const name = `e2e-files-tab-${info.project.name}-${Date.now()}`;
  const api = (path: string, method: string, body?: any) => page.evaluate(async ([p, m, b]) => {
    const r = await fetch(p, { method: m, headers: { 'Content-Type': 'application/json' }, body: b ? JSON.stringify(b) : undefined });
    return r.status; }, [path, method, body] as const);
  expect(await api('/api/sessions', 'POST', { name, dir: '/tmp' })).toBe(201);
  try {
    // The page learns about the new worker from its session list.
    await expect.poll(() => page.evaluate(async w => { await (window as any).fetchSessions(); return ((0, eval)('sessions') || []).some((s: any) => s.name === w); }, name)).toBe(true);
    await page.evaluate(w => (window as any).openPeek(w), name);
    await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
    const tab = page.locator('#peek-tab-files');
    await expect(tab).toBeVisible();
    await expect(page.locator('#peek-dir-text'), 'the directory path stays').toBeVisible();
    await expect.poll(() => page.evaluate(() => (0, eval)('peekSessionDir'))).toMatch(/\/tmp$/);
    const root = await page.evaluate(() => (0, eval)('peekSessionDir'));
    await tab.click();
    await expect(page.locator('#files-view')).toBeVisible();
    await expect.poll(() => page.evaluate(() => [(0, eval)('_filesPath'), (0, eval)('_exploreSession')])).toEqual([root, name]);
    await expect(page.locator('#files-breadcrumb')).toBeVisible();
  } finally {
    await cleanup('delete the test worker', () => api('/api/sessions/' + encodeURIComponent(name), 'DELETE'), info);
  }
});
