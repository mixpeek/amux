// The Mac app's Freeze Workspace (desktop/amux-workspace.swift) sets
// window.__amuxFrozen on every page. A page showing a worker then keeps that
// worker: opening another one, the close button and Escape are refused with a
// toast and a frozen_refused beacon (owner, 2026-11-03: "freeze a workspace, so
// i can't change the tabs, names of tabs, workers etc"). Unfrozen, all three
// work again. Worker endpoints are routed; openPeek and closePeek are real.
import {test, expect, Page} from './fixtures';

async function setup(page: Page, diag: any[]) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  const workers = ['fz-a', 'fz-b'];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: workers.map(name => ({name, running: true, status: 'idle', dir: '/tmp/' + name}))}));
  await page.route(/\/api\/sessions\/fz-[ab]\/peek\?/, r => r.fulfill({json: {name: 'fz', live: 'Worker output', history: ''}}));
  await page.route(/\/api\/sessions\/fz-[ab]\/subagents$/, r => r.fulfill({json: {session: 'fz', subagents: []}}));
  await page.route('**/api/client-debug', r => { try { diag.push(JSON.parse(r.request().postData() || '{}')); } catch {} return r.fulfill({json: {ok: true}}); });
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction("typeof openPeek === 'function' && typeof closePeek === 'function'");
  await page.evaluate(() => (window as any).openPeek('fz-a'));
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
}

// peekSession is a script-scope `let`, so it is read by name, not off window.
const shown = (page: Page) => page.evaluate(
  "({worker: peekSession, open: document.getElementById('peek-overlay').classList.contains('active')})") as Promise<{worker: string, open: boolean}>;

test('a frozen workspace keeps the worker a page shows', async ({page}) => {
  const diag: any[] = [];
  await setup(page, diag);
  await page.evaluate(() => { (window as any).__amuxFrozen = true; });

  await page.evaluate(() => (window as any).openPeek('fz-b'));
  expect(await shown(page)).toEqual({worker: 'fz-a', open: true});
  await expect(page.locator('#toast')).toContainText('frozen');

  await page.locator('#peek-close-btn').click();
  expect(await shown(page)).toEqual({worker: 'fz-a', open: true});
  await page.keyboard.press('Escape');
  expect(await shown(page)).toEqual({worker: 'fz-a', open: true});

  // Reopening the worker it already shows is not a change.
  await page.evaluate(() => (window as any).openPeek('fz-a'));
  expect(await shown(page)).toEqual({worker: 'fz-a', open: true});

  await expect.poll(() => diag.filter(d => d.kind === 'frozen_refused').map(d => d.what)).toEqual(
    expect.arrayContaining(['switch_worker', 'close_worker']));

  await page.evaluate(() => { (window as any).__amuxFrozen = false; });
  await page.evaluate(() => (window as any).openPeek('fz-b'));
  expect(await shown(page)).toEqual({worker: 'fz-b', open: true});
  await page.locator('#peek-close-btn').click();
  await expect.poll(async () => (await shown(page)).open).toBe(false);
});
