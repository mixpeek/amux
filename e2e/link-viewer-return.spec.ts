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

type Tab = { id: number; name: string; hash: string };
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
  await page.locator('#file-overlay .file-overlay-header button[onclick="closeFilePreview()"]').click();
  await expect(page.locator('body')).not.toHaveClass(/\blink-viewer\b/);
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  expect(await page.evaluate('_chromeTabs.map(t => t.id)'), 'the origin bar, rebuilt here').toEqual([1, 3, 5]);
  expect(await page.evaluate('_chromeActiveId'), 'on the tab the link was clicked in').toBe(3);
  expect(new URL(page.url()).search, 'the viewer query is gone from the address').toBe('');
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
  await page.evaluate('closeFilePreview()');
  expect(await page.evaluate('_chromeTabs.map(t => t.id)')).toEqual([1, 5, 6]);
  expect(await page.evaluate('_chromeActiveId')).toBe(6);
  await expect(page.locator('#chrome-tab-frames iframe[data-tab-id="6"]')).toHaveAttribute('src', '/?_tab=6#peek=alpha');
});

test('an origin tab that moved to another worker is not returned to; one showing the worker is', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '' }, { id: 3, name: '', hash: '#peek=beta' }, { id: 4, name: '', hash: '#peek=alpha&tab=messages' }], 1);
  await page.goto(viewerUrl('alpha', 3));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('closeFilePreview()');
  expect(await page.evaluate('_chromeActiveId')).toBe(4);
  expect(await page.evaluate('_chromeTabs.length')).toBe(3);
});

test('the first tab as origin: the page itself goes to the worker', async ({ page }) => {
  await originBar(page, [{ id: 1, name: '', hash: '#peek=alpha' }, { id: 2, name: '', hash: '#view=board' }], 2);
  await page.goto(viewerUrl('alpha', 1));
  await ready(page);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await page.evaluate('closeFilePreview()');
  expect(await page.evaluate('_chromeActiveId')).toBe(1);
  await expect(page.locator('#chrome-tab-frames')).not.toHaveClass(/\bactive\b/);
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#peek=alpha');
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
