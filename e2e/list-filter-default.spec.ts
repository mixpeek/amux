// Save as default (filters panel) and how a new tab of the in-page tab bar starts.
//
// A saved default wins; with none, a new tab copies the tab you came from.
// Search text is never copied. Clear all empties only the tab it is pressed in.
// Pressing Save as default again on a matching default clears it.
import {test, expect} from './fixtures';

const WORKERS = [
  {name: 'alpha-one', running: true, status: 'idle', dir: '/tmp'},
  {name: 'beta-two', running: true, status: 'idle', dir: '/tmp'},
];
const READY = "typeof openPeek === 'function' && typeof _chromeAddTab === 'function'";
const STATE = "({q: searchQuery, statuses: [...filterStatuses], input: (document.getElementById('search-input') || {}).value || ''})";
const filterTo = (q: string, status: string) =>
  `(() => { switchView('sessions'); const i = document.getElementById('search-input'); i.value = ${JSON.stringify(q)}; onSearchInput(); filterStatuses.clear(); filterStatuses.add(${JSON.stringify(status)}); render(); })()`;
const DEFAULT_KEY = 'amux_list_default_v1';

async function tabFrame(page: any, id: number) {
  const h = await page.waitForSelector(`#chrome-tab-frames iframe[data-tab-id="${id}"]`, {state: 'attached'});
  const f = (await h.contentFrame())!;
  await f.waitForFunction("typeof openPeek === 'function'");
  return f;
}

test.beforeEach(async ({page}) => {
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: WORKERS}));
  await page.goto('/#view=sessions');
  await page.waitForFunction(READY);
});

async function pressDefault(page: any) {
  await page.evaluate('openFiltersModal()');
  await page.locator('#filter-default-btn').click();
  await page.evaluate('closeFiltersModal()');
}

test('a saved default is what a new tab starts with, not the tab it came from', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.evaluate(filterTo('alpha', 'working'));
  await pressDefault(page);
  await page.evaluate('openFiltersModal()');
  await expect(page.locator('#filter-default-btn')).toHaveText('Default ✓ (tap to clear)');
  await page.evaluate('closeFiltersModal()');

  // The page moves on; the default must not follow it.
  await page.evaluate(filterTo('', 'stopped'));
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f2 = await tabFrame(page, two);
  await f2.evaluate("switchView('sessions')");
  expect(await f2.evaluate(STATE), 'the default wins over the tab it came from; search text is not copied').toEqual({q: '', statuses: ['working'], input: ''});
});

test('with no default, a new tab copies the tab it came from, and clear all in the new tab leaves the default alone', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.evaluate(filterTo('', 'blocked'));
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f2 = await tabFrame(page, two);
  await f2.evaluate("switchView('sessions')");
  expect(await f2.evaluate(STATE)).toEqual({q: '', statuses: ['blocked'], input: ''});

  // Clear all in the new tab empties that tab; the first tab keeps its filter.
  await f2.evaluate("clearAllFilters()");
  expect(await f2.evaluate(STATE)).toEqual({q: '', statuses: [], input: ''});
  expect(await page.evaluate(STATE)).toEqual({q: '', statuses: ['blocked'], input: ''});
  expect(await page.evaluate(() => localStorage.getItem('amux_list_default_v1'))).toBeNull();
});

test('pressing Save as default again clears it, and clear all never changes it', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.evaluate(filterTo('', 'waiting'));
  await pressDefault(page);
  expect(await page.evaluate(() => localStorage.getItem('amux_list_default_v1'))).toContain('waiting');

  // Clear all on the page does not touch the saved default.
  await page.evaluate('clearAllFilters()');
  expect(await page.evaluate(() => localStorage.getItem('amux_list_default_v1'))).toContain('waiting');

  // Set the same filter again: the button now reads as the saved default, and pressing it clears the default.
  await page.evaluate(filterTo('', 'waiting'));
  await page.evaluate('openFiltersModal()');
  await expect(page.locator('#filter-default-btn')).toHaveText('Default ✓ (tap to clear)');
  await page.locator('#filter-default-btn').click();
  await expect(page.locator('#filter-default-btn')).toHaveText('Save as default');
  expect(await page.evaluate(() => localStorage.getItem('amux_list_default_v1'))).toBeNull();
  await page.evaluate('closeFiltersModal()');
});

// The modal is a phone surface too: the default section sits in its scroll
// body, so it must not push past the screen edge.
test('on a phone the filters panel carries Save as default inside the screen', async ({page}, testInfo) => {
  test.skip(testInfo.project.name === 'desktop', 'phone layout only');
  await page.evaluate(filterTo('', 'working'));
  await page.evaluate('openFiltersModal()');
  const btn = page.locator('#filter-default-btn');
  await btn.scrollIntoViewIfNeeded();
  await expect(btn).toBeVisible();
  await btn.click();
  await expect(btn).toHaveText('Default ✓ (tap to clear)');
  const box = await btn.boundingBox();
  const width = page.viewportSize()!.width;
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(width);
  await page.evaluate('closeFiltersModal()');
  await page.screenshot({path: testInfo.outputPath('filters-default-phone.png')});
});
