import {test, expect} from './fixtures';

// Owner, 2026-11-16: a filter set in a tab came back in the first tab after a
// reload, because each tab read and wrote one shared list-view record. Each tab
// keeps its own copy. A new tab starts from the saved default, else copies the
// tab you came from; search text is never copied (owner, 2026-11-23).
const WORKERS = [
  {name: 'alpha-one', running: true, status: 'idle', dir: '/tmp'},
  {name: 'beta-two', running: true, status: 'idle', dir: '/tmp'},
];
const READY = "typeof openPeek === 'function' && typeof _chromeAddTab === 'function'";
const STATE = "({q: searchQuery, statuses: [...filterStatuses], input: (document.getElementById('search-input') || {}).value || ''})";
const CLEAN = {q: '', statuses: [], input: ''};
const filterTo = (q: string, status: string) =>
  `(() => { switchView('sessions'); const i = document.getElementById('search-input'); i.value = ${JSON.stringify(q)}; onSearchInput(); filterStatuses.clear(); filterStatuses.add(${JSON.stringify(status)}); render(); })()`;

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

test('a new tab of the in-page tab bar copies the tab you came from, without its search', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.evaluate(filterTo('alpha', 'working'));
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f2 = await tabFrame(page, two);
  await f2.evaluate("switchView('sessions')");
  expect(await f2.evaluate(STATE)).toEqual({q: '', statuses: ['working'], input: ''});
  await expect(f2.locator('#search-input')).toHaveValue('');
  // The first tab still has its own.
  expect(await page.evaluate(STATE)).toEqual({q: 'alpha', statuses: ['working'], input: 'alpha'});
});

test('a filter set in a tab stays in that tab across a reload, and out of the first tab', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f2 = await tabFrame(page, two);
  await f2.evaluate(filterTo('beta', 'stopped'));
  await expect.poll(() => page.evaluate(id => sessionStorage.getItem('amux_list_view_v1:tab:' + id), two)).toContain('beta');

  await page.reload();
  await page.waitForFunction(READY);
  expect(await page.evaluate(STATE), 'the first tab is not given the other tab\'s filter').toEqual(CLEAN);
  await page.evaluate(id => (window as any)._chromeSwitchTab(id), two);
  const g2 = await tabFrame(page, two);
  expect(await g2.evaluate(STATE), 'the tab is rebuilt on its own filter').toEqual({q: 'beta', statuses: ['stopped'], input: 'beta'});
});

test('closing the first tab carries the next tab\'s list view to the page', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.evaluate(filterTo('alpha', 'working'));
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f2 = await tabFrame(page, two);
  await f2.evaluate(filterTo('beta', 'stopped'));
  await expect.poll(() => page.evaluate(id => sessionStorage.getItem('amux_list_view_v1:tab:' + id), two)).toContain('beta');

  await page.evaluate(id => (window as any)._chromeCloseTab(id), one);
  await expect.poll(() => page.evaluate(STATE)).toEqual({q: 'beta', statuses: ['stopped'], input: 'beta'});
  expect(await page.evaluate(id => sessionStorage.getItem('amux_list_view_v1:tab:' + id), two), 'the moved tab\'s copy is dropped').toBeNull();

  // A tab opened now (it may reuse an id) copies the tab it came from: the page.
  const three = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const f3 = await tabFrame(page, three);
  await f3.evaluate("switchView('sessions')");
  expect(await f3.evaluate(STATE)).toEqual({q: '', statuses: ['stopped'], input: ''});
});
