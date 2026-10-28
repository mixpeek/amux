import {test, expect, Page} from './fixtures';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
// Top-level let/const in app.js are global bindings, not window properties.
declare const _nav: any;
declare const activeView: string;
declare const _HK_IS_MAC: boolean;

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
  const before = await page.evaluate(() => _nav.pos);
  await page.evaluate(x => (window as any).switchView(x), v);
  await page.waitForFunction(b => _nav.pos === b + 1, before);
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
