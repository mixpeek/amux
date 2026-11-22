import {test, expect} from './fixtures';

// Owner, 2026-11-21: a new tab opened with the previous tab's pill filters,
// and a tab that had a worker open came back, after another tab set a filter,
// on that other tab's list. A Mac app tab, a native pane and a browser tab are
// each their own page; all of them read and wrote one localStorage record.
// in-page-tab-own-list-view.spec.ts covers the in-page tab bar's iframes.
const WORKERS = [
  {name: 'alpha-one', running: true, status: 'idle', dir: '/tmp'},
  {name: 'beta-two', running: true, status: 'idle', dir: '/tmp'},
];
const READY = "typeof openPeek === 'function' && typeof render === 'function'";
const STATE = "({q: searchQuery, statuses: [...filterStatuses], tag: activeTag, input: (document.getElementById('search-input') || {}).value || ''})";
const CLEAN = {q: '', statuses: [], tag: '', input: ''};
const filterTo = (q: string, status: string) =>
  `(() => { switchView('sessions'); const i = document.getElementById('search-input'); i.value = ${JSON.stringify(q)}; onSearchInput(); filterStatuses.add(${JSON.stringify(status)}); render(); })()`;

async function open(context: any) {
  const p = await context.newPage();
  await p.goto('/#view=sessions');
  await p.waitForFunction(READY);
  return p;
}

test.beforeEach(async ({page}) => {
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: WORKERS}));
  await page.goto('/#view=sessions');
  await page.waitForFunction(READY);
});

test('a new tab opens with no filter while another tab has one', async ({page}) => {
  await page.evaluate(filterTo('alpha', 'working'));
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_list_view_v1'))).toContain('alpha');
  const b = await open(page.context());
  expect(await b.evaluate(STATE), 'the new tab is free and open').toEqual(CLEAN);
  await expect(b.locator('#search-input')).toHaveValue('');
  expect(await page.evaluate(STATE)).toEqual({q: 'alpha', statuses: ['working'], tag: '', input: 'alpha'});
});

test('a tab reloaded behind a worker keeps its own filter when another tab set a different one', async ({page}) => {
  await page.evaluate(filterTo('alpha', 'working'));
  await page.evaluate("openPeek('alpha-one')");
  const b = await open(page.context());
  await b.evaluate(filterTo('beta', 'stopped'));
  await expect.poll(() => b.evaluate(() => localStorage.getItem('amux_list_view_v1'))).toContain('beta');

  // WebKit reloads a hidden tab; the page comes back on its own view.
  await page.reload();
  await page.waitForFunction(READY);
  await page.evaluate("typeof closePeek === 'function' && peekSession && closePeek()");
  expect(await page.evaluate(STATE), 'backing out shows this tab\'s list').toEqual({q: 'alpha', statuses: ['working'], tag: '', input: 'alpha'});
  expect(await b.evaluate(STATE)).toEqual({q: 'beta', statuses: ['stopped'], tag: '', input: 'beta'});
});

test('the app reopened with no other page open restores the last view used', async ({page}) => {
  await page.evaluate(filterTo('alpha', 'working'));
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_list_view_v1'))).toContain('alpha');
  const ctx = page.context();
  await page.close({runBeforeUnload: true});
  await expect.poll(async () => {
    const probe = await ctx.newPage();
    await probe.goto('/manifest.json');
    const n = await probe.evaluate(() => Object.keys(localStorage).filter(k => k.startsWith('amux_page_alive:')).length);
    await probe.close();
    return n;
  }, {message: 'the closed page removed its beat'}).toBe(0);
  const again = await open(ctx);
  expect(await again.evaluate(STATE)).toEqual({q: 'alpha', statuses: ['working'], tag: '', input: 'alpha'});
});
