import { test, expect } from './fixtures';
import fs from 'fs';
import os from 'os';
import path from 'path';

// Owner, 2026-11-23: a middle-clicked relative path opened a new browser tab
// that "shows the markdown or whatever file under one of your existing tabs",
// tab 1 or tab 3, unrelated to where the link was. A worker-output link now
// opens as a viewer (?_viewer=1&_from=<worker>&_from_tab=<id>): no tab bar, no
// nav, nothing to switch. Closing it turns the browser tab into the full page
// on the tab the link came from, or a new tab on that worker when it is gone.
let dir = '', file = '';
test.beforeAll(() => {
  dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'amux-linkviewer-')));
  file = path.join(dir, 'plan.md');
  fs.writeFileSync(file, '# The plan\n\nOpened from a link.\n');
});
test.afterAll(() => { if (dir) fs.rmSync(dir, { recursive: true, force: true }); });

type Tab = { id: number; name: string; hash: string; page?: boolean };
// The bar the window the link came from left in localStorage.
async function originBar(page: import('@playwright/test').Page, tabs: Tab[], active: number) {
  await page.addInitScript(({ tabs, active }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    if (sessionStorage.getItem('seeded')) return;
    sessionStorage.setItem('seeded', '1');
    localStorage.setItem('amux_chrome_tabs2', JSON.stringify(tabs));
    localStorage.setItem('amux_chrome_active2', String(active));
  }, { tabs, active });
}
const ready = (page: import('@playwright/test').Page) =>
  page.waitForFunction(() => typeof (window as any)._linkViewerReturn === 'function');
const viewerUrl = (from: string, tab: number, target = file) =>
  '/?_viewer=1&_from=' + encodeURIComponent(from) + '&_from_tab=' + tab + '#path=' + target;

test('the viewer shows the file with no tab bar or nav, and leaves the origin bar alone', async ({ page }) => {
  const bar = [{ id: 1, name: '', hash: '' }, { id: 3, name: '', hash: '#peek=alpha' }, { id: 5, name: '', hash: '#view=board' }];
  await originBar(page, bar, 5);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await expect(page.locator('#file-title')).toContainText('plan.md');
  await expect(page.locator('body')).toHaveClass(/\blink-viewer\b/);
  for (const sel of ['#chrome-tabs-bar', '#chrome-tab-frames', '.header-row', '.tab-bar-outer'])
    await expect(page.locator(sel).first(), sel + ' is hidden in the viewer').toBeHidden();
  // The file's own close is the way out; Back shows only with no file open.
  await expect(page.locator('#link-viewer-back')).toBeHidden();
  expect(await page.evaluate('_chromeTabs.length'), 'the viewer holds no tabs of its own').toBe(1);
  // The snapshot is written every 2s by a window with a bar; the viewer's
  // placeholder tab must not replace the origin window's.
  await page.waitForTimeout(2500);
  expect(JSON.parse(await page.evaluate(() => localStorage.getItem('amux_chrome_tabs2') || '[]'))).toEqual(bar);
});

test('closing the viewer lands on the tab the link came from', async ({ page }, testInfo) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 3, name: 'work', hash: '#peek=alpha' }, { id: 5, name: '', hash: '#view=board' }], 5);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.locator('#file-overlay .file-overlay-header button[onclick="dismissFilePreview()"]').click();
  await expect(page.locator('body')).not.toHaveClass(/\blink-viewer\b/);
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  expect(await page.evaluate('_chromeTabs.map(t => t.id)'), 'the origin bar, rebuilt here').toEqual([1, 3, 5]);
  expect(await page.evaluate('_chromeActiveId'), 'on the tab the link was clicked in').toBe(3);
  expect(new URL(page.url()).search, 'the viewer query is gone from the address').toBe('');
  expect(new URL(page.url()).hash, 'the link\'s #path= is gone from the address').toBe('');
  await expect(page.locator('#chrome-tab-frames iframe[data-tab-id="3"]')).toHaveAttribute('src', '/?_tab=3#peek=alpha');
  if (testInfo.project.name === 'desktop') {
    await expect(page.locator('#chrome-tabs-bar .chrome-tab.active')).toHaveAttribute('data-tab-id', '3');
    await expect(page.locator('#chrome-tabs-bar .chrome-tab')).toHaveCount(3);
  }
  // It is now a window: its bar is saved for its own reloads.
  expect(JSON.parse(await page.evaluate(() => sessionStorage.getItem('amux_chrome_state') || '{}')).active).toBe(3);
});

test('when the origin tab is gone, closing opens a new tab on that worker', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 5, name: '', hash: '#view=board' }], 1);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('dismissFilePreview()');
  expect(await page.evaluate('_chromeTabs.map(t => t.id)')).toEqual([1, 5, 6]);
  expect(await page.evaluate('_chromeActiveId')).toBe(6);
  expect(new URL(page.url()).hash).toBe('');
  await expect(page.locator('#chrome-tab-frames iframe[data-tab-id="6"]')).toHaveAttribute('src', '/?_tab=6#peek=alpha');
});

test('an origin tab that moved to another worker is not returned to; one showing the worker is', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 3, name: '', hash: '#peek=beta' }, { id: 4, name: '', hash: '#peek=alpha&tab=messages' }], 1);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('dismissFilePreview()');
  expect(await page.evaluate('_chromeActiveId')).toBe(4);
  expect(await page.evaluate('_chromeTabs.length')).toBe(3);
});

test('the first tab as origin: the page itself goes to the worker', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '#peek=alpha' }, { id: 2, name: '', hash: '#view=board' }], 2);
  await page.goto(viewerUrl('alpha', 1));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('dismissFilePreview()');
  expect(await page.evaluate('_chromeActiveId')).toBe(1);
  await expect(page.locator('#chrome-tab-frames')).not.toHaveClass(/\bactive\b/);
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#peek=alpha');
});

// A dragged bar keeps the page on its own tab, wherever it sits (the snapshot
// marks it page: true). The return must make THAT tab the page, not the first.
test('a page tab that is not first: the return keeps the page on it', async ({ page }) => {
  await originBar(page, [{ id: 2, name: '', hash: '#view=board' }, { id: 1, name: '', hash: '#peek=alpha', page: true }], 2);
  await page.goto(viewerUrl('alpha', 1));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('dismissFilePreview()');
  expect(await page.evaluate('_chromeTabs.map(t => t.id)'), 'the order is kept').toEqual([2, 1]);
  expect(await page.evaluate('_chromePageId'), 'the page stays on the tab the snapshot named').toBe(1);
  expect(await page.evaluate('_chromeActiveId')).toBe(1);
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#peek=alpha');
  expect(await page.evaluate(() => sessionStorage.getItem('amux_chrome_tab_view:2')), 'the other tab keeps its address').toBe('#view=board');
});

test('a folder link shows Back, which returns to the origin tab', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 3, name: '', hash: '#peek=alpha' }], 1);
  await page.goto(viewerUrl('alpha', 3, dir));
  await ready(page);
  const back = page.locator('#link-viewer-back');
  await expect(back).toBeVisible({ timeout: 30_000 });
  const box = await back.boundingBox();
  expect(box!.height, 'Back is a 44px touch target').toBeGreaterThanOrEqual(44);
  await back.click();
  await expect(back).toBeHidden();
  expect(await page.evaluate('_chromeActiveId')).toBe(3);
});

test('a link in a worker reply names that worker and the tab it sits in', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }], 1);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekHtml === 'function');
  const href = await page.evaluate(() => {
    eval("peekSession = 'alpha'; peekSessionDir = '/private/tmp/amux-e2e-lv'");
    const d = document.createElement('div');
    d.innerHTML = (window as any)._peekHtml('Wrote notes/plan.md\n');
    eval('peekSession = null');
    return (d.querySelector('a.md-link') as HTMLAnchorElement).getAttribute('href');
  });
  const u = new URL(href!);
  expect(u.searchParams.get('_viewer')).toBe('1');
  expect(u.searchParams.get('_from')).toBe('alpha');
  expect(u.searchParams.get('_from_tab')).toBe('1');
  expect(u.hash).toBe('#path=/private/tmp/amux-e2e-lv/notes/plan.md');
});

test('an internal close (Show in folder) is not a dismissal: the viewer stays, with Back', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 3, name: '', hash: '#peek=alpha' }], 1);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  // Act once the file has loaded: a load still in flight would reopen it (AF-1399).
  await expect(page.locator('#file-overlay')).toHaveClass(/mdx-on/, { timeout: 30_000 });
  await page.evaluate('_fileShowInFolder()');
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await expect(page.locator('body'), 'still a viewer').toHaveClass(/\blink-viewer\b/);
  await expect(page.locator('#link-viewer-back')).toBeVisible();
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#path=' + dir);
});

// Owner, 2026-11-23: "if I close the file, the path param ... remains in my
// browser URL". A plain #path= link (a copied link, Open in new window) opens
// the Files view under the file; dismissing the file left #path=<folder>.
test('dismissing the file a #path= link opened clears the address', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }], 1);
  await page.goto('/#path=' + file);
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  // Act once the file has loaded: a load still in flight would reopen it (AF-1399).
  await expect(page.locator('#file-overlay')).toHaveClass(/mdx-on/, { timeout: 30_000 });
  await page.keyboard.press('Escape');
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await expect.poll(() => page.evaluate(() => location.hash), { message: 'no #path= left behind' }).toBe('');
  await page.waitForTimeout(1200);   // a late address sync must not put it back
  expect(await page.evaluate(() => location.hash)).toBe('');
  expect(await page.evaluate('activeView')).toBe('sessions');
});

test('Show in folder from a #path= link keeps the folder address', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }], 1);
  await page.goto('/#path=' + file);
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  // Act once the file has loaded: a load still in flight would reopen it (AF-1399).
  await expect(page.locator('#file-overlay')).toHaveClass(/mdx-on/, { timeout: 30_000 });
  await page.evaluate('_fileShowInFolder()');
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#path=' + dir);
  expect(await page.evaluate('activeView')).toBe('files');
});

test('a page whose address moved off the file keeps that address when the file is dismissed', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }], 1);
  await page.goto('/#path=' + file);
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  // A card link moved the address while the file stayed open.
  await page.evaluate(() => history.replaceState(null, '', location.pathname + '#task=T-1'));
  await page.evaluate('dismissFilePreview()');
  expect(await page.evaluate(() => location.hash)).toBe('#task=T-1');
  expect(await page.evaluate('activeView'), 'the view is not swept away').toBe('files');
});

// Owner, 2026-11-23: closing the viewer "goes to the worker board and then
// enters the worker". The tab returned to is rebuilt on #peek=<worker>, and it
// showed the Workers list until the worker list loaded. It now opens straight
// on the worker: the fleet stays hidden until the worker opens.
const SLOW_FLEET = [{ name: 'alpha', running: true, status: 'idle', dir: '/tmp' }, { name: 'beta', running: true, status: 'idle', dir: '/tmp' }];
async function slowFleet(page: import('@playwright/test').Page, fleet = SLOW_FLEET) {
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, async r => {
    await new Promise(res => setTimeout(res, 1500));
    await r.fulfill({ json: fleet }).catch(() => {});
  });
}
const fleetShown = () => {
  const v = document.getElementById('session-view');
  const peek = document.getElementById('peek-overlay')?.classList.contains('active');
  return { listShown: !!v && getComputedStyle(v).visibility !== 'hidden' && !!document.querySelector('#session-view .card'), peek: !!peek };
};

test('the tab returned to opens straight on the worker, never the Workers list first', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar, whose tab is rebuilt, is desktop only');
  await slowFleet(page);
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 3, name: '', hash: '#peek=alpha' }], 1);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('dismissFilePreview()');
  const h = await page.waitForSelector('#chrome-tab-frames iframe[data-tab-id="3"]', { state: 'attached' });
  const f = (await h.contentFrame())!;
  const seen: { listShown: boolean; peek: boolean }[] = [];
  const t0 = Date.now();
  while (Date.now() - t0 < 15_000) {
    const s = await f.evaluate(fleetShown).catch(() => null);
    if (s) { seen.push(s); if (s.peek) break; }
    await page.waitForTimeout(40);
  }
  expect(seen.some(s => s.peek), 'the worker opened in the tab').toBe(true);
  expect(seen.filter(s => s.listShown && !s.peek), 'the Workers list never showed before the worker').toEqual([]);
  expect(await f.evaluate(() => document.documentElement.classList.contains('boot-peek-pending'))).toBe(false);
});

test('a #peek= address for a worker that is gone shows the Workers list, not a blank page', async ({ page }) => {
  test.setTimeout(60_000);
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: SLOW_FLEET }));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  const gaveUp = page.waitForEvent('console', { predicate: m => m.text().includes('peek_boot_reveal: worker_not_found'), timeout: 20_000 });
  await page.goto('/#peek=gone-worker');
  await ready(page);
  expect(await page.evaluate(() => document.documentElement.classList.contains('boot-peek-pending')), 'hidden while it looks').toBe(true);
  await gaveUp;   // revealed when the look gives up, and says why; not only by the 12s backstop
  expect(await page.evaluate(() => document.documentElement.classList.contains('boot-peek-pending'))).toBe(false);
  await expect(page.locator('#session-view')).toBeVisible();
  expect(await page.evaluate('peekSession')).toBeFalsy();
});

test('a page opened without a worker address is not held hidden', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/#view=board');
  await ready(page);
  expect(await page.evaluate(() => document.documentElement.classList.contains('boot-peek-pending'))).toBe(false);
});
