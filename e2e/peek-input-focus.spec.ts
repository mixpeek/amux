// Opening a worker puts you in its input, ready to type (owner, 2026-10-28:
// "when i open a worker make the input focused"). With a mouse or trackpad;
// a touch phone keeps the keyboard down until you tap.
import {test, expect, Page} from './fixtures';

async function open(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{name: 'focus-worker', running: true, status: 'idle', dir: '/tmp/f'}]}));
  await page.route(/\/api\/sessions\/focus-worker\/peek\?/, r => r.fulfill({json: {name: 'focus-worker', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/focus-worker\/subagents$/, r => r.fulfill({json: {session: 'focus-worker', subagents: []}}));
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction("typeof openPeek === 'function'");
  await page.evaluate(() => (window as any).openPeek('focus-worker'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
}

test('opening a worker focuses its input', async ({page}) => {
  test.skip(await page.evaluate(() => !matchMedia('(hover: hover) and (pointer: fine)').matches), 'a touch device keeps the keyboard down');
  await open(page);
  await expect(page.locator('#peek-cmd-input')).toBeFocused();
  // And typing goes straight in.
  await page.keyboard.type('hello');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('hello');
});

test('opening a worker with a search keeps the search box focused, not the input', async ({page}) => {
  test.skip(await page.evaluate(() => !matchMedia('(hover: hover) and (pointer: fine)').matches), 'pointer devices only');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [{name: 'focus-worker', running: true, status: 'idle', dir: '/tmp/f'}]}));
  await page.route(/\/api\/sessions\/focus-worker\/peek\?/, r => r.fulfill({json: {name: 'focus-worker', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/focus-worker\/subagents$/, r => r.fulfill({json: {session: 'focus-worker', subagents: []}}));
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction("typeof openPeek === 'function'");
  await page.evaluate(() => (window as any).openPeek('focus-worker', {query: 'error'}));
  await page.waitForTimeout(300);
  await expect(page.locator('#peek-cmd-input')).not.toBeFocused();
});
