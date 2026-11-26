import {test, expect} from './fixtures';

// Owner, 2026-11-23: "Dragging to order tabs in Amux on the web is not
// working." The in-page tab strip ("Tab 1", "Tab 2", +) never had a drag; only
// the ⊞ menus did. A tab now drags along the strip with the pointer. One tab
// is the page itself and the others are iframes; a drop only moves labels.
const WORKERS = [
  {name: 'alpha-one', running: true, status: 'idle', dir: '/tmp'},
  {name: 'beta-two', running: true, status: 'idle', dir: '/tmp'},
];
const READY = "typeof openPeek === 'function' && typeof _chromeAddTab === 'function' && !!document.getElementById('chrome-tabs-bar')._chromeSortable";
const STRIP = "[...document.querySelectorAll('#chrome-tabs-bar .chrome-tab')].map(e => +e.dataset.tabId)";
const filterTo = (q: string) =>
  `(() => { switchView('sessions'); const i = document.getElementById('search-input'); i.value = ${JSON.stringify(q)}; onSearchInput(); render(); })()`;

async function tabFrame(page: any, id: number) {
  const h = await page.waitForSelector(`#chrome-tab-frames iframe[data-tab-id="${id}"]`, {state: 'attached'});
  const f = (await h.contentFrame())!;
  await f.waitForFunction("typeof openPeek === 'function'");
  return f;
}

// Press on one tab and move across the strip in small steps, as a hand does.
async function dragTab(page: any, fromId: number, toId: number, midway?: () => Promise<unknown>) {
  const a = (await page.locator(`.chrome-tab[data-tab-id="${fromId}"]`).boundingBox())!;
  const b = (await page.locator(`.chrome-tab[data-tab-id="${toId}"]`).boundingBox())!;
  const x = a.x + a.width / 2, y = a.y + a.height / 2;
  const tx = b.x + b.width / 2 + (b.x > a.x ? 6 : -6);
  await page.mouse.move(x, y);
  await page.mouse.down();
  for (let i = 1; i <= 16; i++) {
    await page.mouse.move(x + (tx - x) * i / 16, y); await page.waitForTimeout(25);
    if (i === 8 && midway) await midway();
  }
  await page.mouse.up();
  // A click within 400ms of a drop is the drop's own and is swallowed.
  await page.waitForTimeout(450);
}

// On a phone the strip is not shown at all, so the drag adds nothing there.
test('a phone shows no tab strip to drag', async ({page}, testInfo) => {
  test.skip(testInfo.project.name === 'desktop', 'the phone projects check this');
  await page.goto('/#view=sessions');
  await page.waitForFunction("typeof _chromeAddTab === 'function'");
  await expect(page.locator('#chrome-tabs-bar')).toBeHidden();
  expect(await page.evaluate("getComputedStyle(document.documentElement).getPropertyValue('--chrome-tab-h').trim()")).toBe('0px');
});

test.describe('desktop', () => {
test.beforeEach(async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: WORKERS}));
  await page.goto('/#view=sessions');
  await page.waitForFunction(READY);
});

test('dragging a later tab along the strip reorders it, and the order survives a reload', async ({page}) => {
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const three = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  expect(await page.evaluate(STRIP)).toEqual([one, two, three]);

  await dragTab(page, three, two);
  await expect.poll(() => page.evaluate(STRIP)).toEqual([one, three, two]);
  expect(await page.evaluate('_chromeTabs.map(t => t.id)'), 'the list follows the strip').toEqual([one, three, two]);
  // The drop's click is swallowed: the active tab does not change under it.
  expect(await page.evaluate('_chromeActiveId')).toBe(three);

  await page.reload();
  await page.waitForFunction(READY);
  expect(await page.evaluate(STRIP), 'the order is kept across a reload').toEqual([one, three, two]);
});

// Owner, 2026-11-23: after a drag, going back to a tab loaded the worker list
// and then its worker. The page tab used to be whichever was first, so a drop
// in front of it moved the page and rebuilt the old first as a new iframe. A
// drop now only moves labels: no tab reloads, and each keeps its own view.
test('dragging a tab to the front reloads nothing, and each tab keeps its own view', async ({page}) => {
  await page.evaluate(filterTo('alpha'));
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f2 = await tabFrame(page, two);
  await f2.evaluate(filterTo('beta'));
  // Marks on the live windows: a reload or a rebuilt iframe loses them.
  await page.evaluate("window.__livePage = 1");
  await f2.evaluate("window.__liveTab = 1");

  await dragTab(page, two, one);
  await expect.poll(() => page.evaluate(STRIP)).toEqual([two, one]);
  expect(await page.evaluate('_chromePageId'), 'the page is still the tab it was').toBe(one);
  expect(await f2.evaluate('window.__liveTab'), 'the dragged tab was not reloaded').toBe(1);
  expect(await f2.evaluate('searchQuery')).toBe('beta');

  await page.locator(`.chrome-tab[data-tab-id="${one}"]`).click();
  await expect(page.locator('#chrome-tab-frames')).not.toHaveClass(/active/);
  expect(await page.evaluate('window.__livePage'), 'the page tab was not reloaded').toBe(1);
  expect(await page.evaluate('searchQuery'), 'the page tab keeps its filter').toBe('alpha');

  await page.locator(`.chrome-tab[data-tab-id="${two}"]`).click();
  await expect(page.locator(`#chrome-tab-frames iframe[data-tab-id="${two}"]`)).toBeVisible();
  expect(await f2.evaluate('window.__liveTab'), 'switching back shows the same live tab').toBe(1);

  // After a reload the order and the page tab are kept, and the iframe tab is
  // rebuilt on its own place.
  await page.reload();
  await page.waitForFunction(READY);
  expect(await page.evaluate(STRIP)).toEqual([two, one]);
  expect(await page.evaluate('_chromePageId')).toBe(one);
  const g2 = await tabFrame(page, two);
  await g2.evaluate("switchView('sessions')");
  expect(await g2.evaluate('searchQuery')).toBe('beta');
});

// Closing the page tab moves the page to the tab beside it, wherever it sits.
test('closing the page tab when it is not first moves the page to its neighbour', async ({page}) => {
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const three = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  await dragTab(page, one, three);
  await expect.poll(() => page.evaluate(STRIP)).toEqual([two, three, one]);
  await page.locator(`.chrome-tab[data-tab-id="${one}"] .chrome-tab-close`).click();
  await expect.poll(() => page.evaluate(STRIP)).toEqual([two, three]);
  expect(await page.evaluate('_chromePageId'), 'the page moved to the tab beside the closed one').toBe(three);
  await expect(page.locator(`#chrome-tab-frames iframe[data-tab-id="${three}"]`)).toHaveCount(0);
});

test('the close button and a plain click still work on a draggable tab', async ({page}) => {
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  await page.locator(`.chrome-tab[data-tab-id="${one}"]`).click();
  await expect.poll(() => page.evaluate('_chromeActiveId')).toBe(one);
  await page.locator(`.chrome-tab[data-tab-id="${two}"] .chrome-tab-close`).click();
  await expect.poll(() => page.evaluate(STRIP)).toEqual([one]);
});

// Owner, 2026-11-23: the hide chevron sat below the middle of the tabs, and
// once the strip was hidden it was "very very small and hard to press".
test('the hide chevron is centred on the tabs, a line runs under the strip, and the pull tab is easy to hit', async ({page}) => {
  const btn = page.locator('.chrome-tab-collapse');
  // Centred with one tab (no ✕) and with two.
  for (const n of [1, 2]) {
    if (n === 2) await page.evaluate('_chromeAddTab()');
    const b = (await btn.boundingBox())!, t = (await page.locator('.chrome-tab.active').boundingBox())!;
    expect(Math.abs((b.y + b.height / 2) - (t.y + t.height / 2)), `chevron centred on the tab row with ${n} tab(s)`).toBeLessThanOrEqual(1);
  }
  expect(await page.evaluate("getComputedStyle(document.getElementById('chrome-tabs-bar')).boxShadow"), 'the line under the strip').toContain('inset');

  await btn.click();
  await expect.poll(() => page.evaluate('_chromeCollapsed')).toBe(true);
  const p = (await btn.boundingBox())!;
  expect(p.width, 'the pull tab is wide enough to catch').toBeGreaterThanOrEqual(60);
  expect(Number(await btn.evaluate(e => getComputedStyle(e).opacity)), 'the pull tab is not faded out').toBeGreaterThanOrEqual(0.7);
  // Its hit area reaches below what it shows.
  await page.mouse.click(p.x + p.width / 2, p.y + p.height + 6);
  await expect.poll(() => page.evaluate('_chromeCollapsed'), 'a click just below the pull tab shows the strip').toBe(false);
});

// Sortable shares the drag in progress across every instance, and destroying
// any of them ends it with no drop. The board re-renders on live updates even
// while hidden (renderBoard from _ensureArchivedCount, measured in the land
// gate's run), and that stranded the tab under the pointer. A render that
// destroys Sortables now waits for the drag and runs after it.
test('a board re-render while a tab is held does not strand the drag, and runs after the drop', async ({page}) => {
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const three = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  await page.evaluate(() => {
    const w = window as any, orig = w.renderBoard;
    w.__boardRenders = [];
    w.renderBoard = function () { w.__boardRenders.push(!!(w.Sortable && w.Sortable.dragged)); return orig.apply(this, arguments); };
    // Every Sortable destroyed, and whether a drag was live when it was.
    w.__destroys = [];
    const d = w.Sortable.prototype.destroy;
    w.Sortable.prototype.destroy = function () { w.__destroys.push(!!w.Sortable.dragged); return d.apply(this, arguments); };
  });
  // The worker cards carry their own Sortable only in the Manual sort.
  await page.evaluate("sortMode = 'manual'; initSortable()");
  expect(await page.evaluate('!!_sortable'), 'the worker cards have a Sortable to destroy').toBe(true);
  await dragTab(page, three, two, () => page.evaluate('renderBoard(), initSortable()'));
  await expect.poll(() => page.evaluate(STRIP)).toEqual([one, three, two]);
  await expect(page.locator('#chrome-tabs-bar .tab-drag-floating')).toHaveCount(0);
  // Nothing was destroyed under the live drag, and the render that waited ran
  // after the drop. The board also re-renders on its own, so count, not match.
  expect(await page.evaluate('window.__destroys.filter(Boolean).length'), 'a Sortable was destroyed mid-drag').toBe(0);
  const renders = await page.evaluate('window.__boardRenders') as boolean[];
  expect(renders.indexOf(true), 'the board was asked to render during the drag').toBeGreaterThanOrEqual(0);
  await expect.poll(() => page.evaluate('window.__boardRenders.lastIndexOf(false) > window.__boardRenders.indexOf(true)'),
    'the held render ran after the drop').toBe(true);
});
});
