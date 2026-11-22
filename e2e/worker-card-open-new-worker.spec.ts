import { test, expect } from './fixtures';

// Owner, 2026-11-22: a worker just created "doesn't open unless you click the
// terminal block and the terminal block doesn't always get started
// immediately ... just clicking the text just folds the card rather than
// opening the worker." The preview block was the card's only single-click way
// in, and a new worker has no preview until its pane prints. The name is now a
// link to the worker, and a running worker with no output yet still shows the
// block, saying so.
const WORKERS = [
  { name: 'just-made', running: true, status: 'idle', dir: '/tmp', preview_lines: [] },
  { name: 'has-output', running: true, status: 'idle', dir: '/tmp', preview_lines: ['$ make test', 'ok'] },
  { name: 'stopped-one', running: false, status: '', dir: '/tmp' },
];

async function workers(page: import('@playwright/test').Page) {
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: WORKERS }));
  await page.goto('/#view=sessions');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await expect(page.locator('.card[data-session="just-made"]')).toBeVisible({ timeout: 15_000 });
}
const card = (page: import('@playwright/test').Page, n: string) => page.locator(`.card[data-session="${n}"]`);
const folded = (page: import('@playwright/test').Page, n: string) => card(page, n).evaluate(el => el.classList.contains('expanded'));

test('a click on a new worker\'s name opens it and does not fold its card', async ({ page }) => {
  await workers(page);
  const before = await folded(page, 'just-made');
  const name = card(page, 'just-made').locator('a.card-name-link');
  await expect(name).toHaveText('just-made');
  await expect(name).toHaveAttribute('href', '/#peek=just-made');
  await name.click();
  await expect.poll(() => page.evaluate('peekSession'), { timeout: 10_000 }).toBe('just-made');
  expect(await folded(page, 'just-made'), 'the card did not fold').toBe(before);
});

test('a running worker with no output yet shows a block that opens it', async ({ page }) => {
  await workers(page);
  // The block lives in the card's panel, shown when the card is unfolded.
  await page.evaluate(() => { for (const n of ['just-made', 'has-output', 'stopped-one']) (window as any).toggle(n); });
  await expect.poll(() => folded(page, 'just-made')).toBe(true);
  const pending = card(page, 'just-made').locator('.card-preview-lines.card-preview-pending');
  await expect(pending).toHaveText('No output yet · open');
  await expect(card(page, 'has-output').locator('.card-preview-pending'), 'a worker with output shows its output').toHaveCount(0);
  await expect(card(page, 'has-output').locator('.card-preview-lines')).toContainText('make test');
  await expect(card(page, 'stopped-one').locator('.card-preview-lines'), 'a stopped worker gets no block').toHaveCount(0);
  const before = await folded(page, 'just-made');
  await pending.click();
  await expect.poll(() => page.evaluate('peekSession'), { timeout: 10_000 }).toBe('just-made');
  expect(await folded(page, 'just-made')).toBe(before);
});

test('a middle or ⌘/ctrl press on the name is left to the browser', async ({ page }) => {
  await workers(page);
  const out = await card(page, 'just-made').locator('a.card-name-link').evaluate((a: HTMLAnchorElement) => {
    const cancelled: Record<string, boolean> = {};
    const press = (k: string, type: string, init: MouseEventInit) => {
      const rec = (e: Event) => { cancelled[k] = e.defaultPrevented; e.preventDefault(); };
      a.addEventListener(type, rec, { once: true });
      a.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, ...init }));
      a.removeEventListener(type, rec);
    };
    press('middle', 'auxclick', { button: 1 });
    press('ctrl', 'click', { button: 0, ctrlKey: true });
    press('meta', 'click', { button: 0, metaKey: true });
    return { cancelled, peek: (window as any).peekSession || null };
  });
  expect(out).toEqual({ cancelled: { middle: false, ctrl: false, meta: false }, peek: null });
});

test('a click elsewhere on the card still folds it', async ({ page }) => {
  await workers(page);
  const before = await folded(page, 'has-output');
  await card(page, 'has-output').locator('.card-header-top').click({ position: { x: 2, y: 2 }, force: true });
  await expect.poll(() => folded(page, 'has-output')).toBe(!before);
  expect(await page.evaluate('peekSession || null')).toBeNull();
});
