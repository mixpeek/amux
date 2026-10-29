import {test, expect, Page, allowUnusedRoute} from './fixtures';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
// Top-level let/const in app.js are global bindings, not window properties.
declare const _nav: any;
declare const activeView: string;
declare const _HK_IS_MAC: boolean;
declare let _NAV_MAX: number;

// Ethan, 2026-10-28: "create some kind of back functionality that doesnt take
// up a lot of real estate ... so i can go back to a previous page i had in
// amux some kind of breadcrumbs". One ‹ button at the left of the header,
// hidden until there is somewhere to go back to; a long press lists the
// recent places; the browser's Back and Mod+[ use the same history.

test.beforeEach(async ({page}) => {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: []}));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).navBack === 'function' && typeof _nav === 'object');
  await page.waitForFunction(() => _nav.pos === 0);
});

const view = (page: Page) => page.evaluate(() => activeView);
async function go(page: Page, v: string) {
  await page.evaluate(x => (window as any).switchView(x), v);
  await page.waitForFunction(x => !_nav.restoring && _nav.list[_nav.pos] && _nav.list[_nav.pos].id === x, v);
}

test('back returns to the previous tab, and the button shows only when there is one', async ({page}) => {
  const back = page.locator('#nav-back-btn');
  await expect(back).toBeHidden();
  const start = await view(page);
  await go(page, 'board');
  await go(page, 'scheduler');
  await expect(back).toBeVisible();
  // It takes no row of its own: it sits in the header row beside the brand.
  const [b, h] = await Promise.all([back.boundingBox(), page.locator('#brand-header').boundingBox()]);
  expect(Math.abs((b!.y + b!.height / 2) - (h!.y + h!.height / 2))).toBeLessThan(12);
  await back.click();
  await expect.poll(() => view(page)).toBe('board');
  await back.click();
  await expect.poll(() => view(page)).toBe(start);
  await expect(back).toBeHidden();
  // Forward goes the other way.
  await page.evaluate(() => (window as any).navForward());
  await expect.poll(() => view(page)).toBe('board');
});

test('back closes a file and returns to where it was opened from', async ({page}) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-nav-'));
  const f = join(dir, 'notes.md');
  await writeFile(f, '# Notes\n\nhello\n');
  await go(page, 'board');
  const before = await page.evaluate(() => _nav.pos);
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await page.waitForFunction(b => _nav.pos === b + 1, before);
  await expect(page.locator('#file-overlay')).toHaveClass(/active/);
  await page.evaluate(() => (window as any).navBack());
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await expect.poll(() => view(page)).toBe('board');
});

test('a long press lists recent places, newest first, and picking one goes there', async ({page}) => {
  const start = await view(page);
  await go(page, 'board');
  await go(page, 'scheduler');
  await go(page, 'calendar');
  await page.locator('#nav-back-btn').click({button: 'right'});
  const items = page.locator('#nav-history-menu .nav-history-item');
  await expect(items).toHaveCount(4);
  await expect(items.nth(0)).toHaveClass(/current/);
  await items.nth(3).click();
  await expect.poll(() => view(page)).toBe(start);
  await expect(page.locator('#nav-history-menu')).toHaveCount(0);
});

test('Mod+[ and the browser\'s own Back go back too', async ({page}) => {
  await go(page, 'board');
  await go(page, 'scheduler');
  const mac = await page.evaluate(() => _HK_IS_MAC);
  await page.locator('body').click({position: {x: 5, y: 300}});
  await page.keyboard.press((mac ? 'Meta' : 'Control') + '+BracketLeft');
  await expect.poll(() => view(page)).toBe('board');
  await go(page, 'calendar');
  await page.goBack();
  await expect.poll(() => view(page)).toBe('board');
});

// Land review F1: past the cap the oldest place is dropped, and Back must
// still land on the right place (the stored browser indices did not shift).
test('back still lands on the right place after the oldest places are dropped', async ({page}) => {
  await page.evaluate(() => { _NAV_MAX = 3; });
  for (const v of ['board', 'scheduler', 'calendar', 'files', 'messages']) await go(page, v);
  expect(await page.evaluate(() => _nav.list.map((p: any) => p.id))).toEqual(['calendar', 'files', 'messages']);
  await page.evaluate(() => (window as any).navBack());
  await expect.poll(() => view(page)).toBe('files');
  await page.evaluate(() => (window as any).navBack());
  await expect.poll(() => view(page)).toBe('calendar');
  await expect(page.locator('#nav-back-btn')).toBeHidden();
});

// Owner, 2026-10-29: "when i click a file i should have the same back button -
// make sure the back button is throughout amux ... and make sure its not in
// the way". The file view and the worker view cover the header, so each shows
// the same ‹ in its own title row, beside the title and never over it.
async function inRowBesideTitle(page: Page, back: string, title: string) {
  const [b, t] = await Promise.all([page.locator(back).boundingBox(), page.locator(title).boundingBox()]);
  expect(b && t, 'both drawn').toBeTruthy();
  // Same row: their centres line up.
  expect(Math.abs((b!.y + b!.height / 2) - (t!.y + t!.height / 2))).toBeLessThan(14);
  // Not in the way: it ends before the title starts.
  expect(b!.x + b!.width).toBeLessThanOrEqual(t!.x + 1);
}

test('a file shows the same back button in its own header, and it goes back', async ({page}) => {
  const dir = await mkdtemp(join(tmpdir(), 'amux-nav-'));
  const f = join(dir, 'plan.md');
  await writeFile(f, '# Plan\n\nhello\n');
  await go(page, 'board');
  await page.evaluate(p => (window as any).openFilePreview(p), f);
  await page.waitForFunction(() => !_nav.restoring && _nav.list[_nav.pos] && _nav.list[_nav.pos].k === 'file');
  const back = page.locator('#file-overlay .nav-back-btn');
  await expect(back).toBeVisible();
  await expect(back).toHaveAttribute('title', /Back to/);
  await inRowBesideTitle(page, '#file-overlay .nav-back-btn', '#file-title');
  await back.click();
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await expect.poll(() => view(page)).toBe('board');
});

test('a worker shows the same back button beside its name, and it goes back', async ({page}) => {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{name: 'nav-w', running: true, status: 'idle', dir: '/tmp'}]}));
  await page.route(/\/api\/sessions\/nav-w\/peek\?/, r => r.fulfill({json: {name: 'nav-w', live: 'output', history: ''}}));
  await page.route(/\/api\/sessions\/nav-w\/subagents$/, r => r.fulfill({json: {session: 'nav-w', subagents: []}}));
  allowUnusedRoute(page, /\/api\/sessions\/nav-w\/subagents$/);
  allowUnusedRoute(page, /\/api\/sessions\/nav-w\/peek\?/);
  // Registered after the page loaded, so the worker list may not poll again first.
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await go(page, 'board');
  await page.evaluate(() => (window as any).openPeek('nav-w'));
  await page.waitForFunction(() => _nav.list[_nav.pos] && _nav.list[_nav.pos].k === 'peek');
  const back = page.locator('#peek-overlay .nav-back-btn');
  await expect(back).toBeVisible();
  await inRowBesideTitle(page, '#peek-overlay .nav-back-btn', '#peek-title');
  await back.click();
  await expect(page.locator('#peek-overlay')).toBeHidden();
  await expect.poll(() => view(page)).toBe('board');
});
