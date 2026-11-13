import {test, expect} from './fixtures';

// Owner, 2026-11-13: close the worker in one tab of the in-page tab bar, open
// another there, switch tabs, and "the other tab will then also become that
// session". Tabs after the first are iframes, rebuilt after any reload of the
// page from a bare /?_tab=1, so each came back on whichever worker another tab
// opened last. Each tab now keeps its own address and is rebuilt on it.
test('a tab of the in-page tab bar is rebuilt on its own worker, not the one another tab opened', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [
    {name: 'tab-one-first', running: true, status: 'idle', dir: '/tmp'},
    {name: 'tab-two-own', running: true, status: 'idle', dir: '/tmp'},
    {name: 'tab-one-next', running: true, status: 'idle', dir: '/tmp'},
  ]}));
  const ready = "typeof openPeek === 'function' && typeof _chromeAddTab === 'function'";
  const frame = async (id: number) => {
    const h = await page.waitForSelector(`#chrome-tab-frames iframe[data-tab-id="${id}"]`, {state: 'attached'});
    const f = (await h.contentFrame())!;
    await f.waitForFunction("typeof openPeek === 'function'");
    return f;
  };
  await page.goto('/');
  await page.waitForFunction(ready);
  await page.evaluate(() => (window as any).openPeek('tab-one-first'));
  // _chromeTabs and _chromeActiveId are top-level lets, not window properties.
  const two = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
  const one = await page.evaluate('_chromeTabs[0].id') as number;
  const f2 = await frame(two);
  await f2.evaluate(() => (window as any).openPeek('tab-two-own'));
  // Its address is saved under its own key (read on a 1 s timer).
  await expect.poll(() => page.evaluate(id => sessionStorage.getItem('amux_chrome_tab_view:' + id), two), {timeout: 5000}).toBe('#peek=tab-two-own');

  // Tab 1: close the worker with the X, open another.
  await page.evaluate(id => (window as any)._chromeSwitchTab(id), one);
  await page.click('#peek-close-btn');
  await page.evaluate(() => (window as any).openPeek('tab-one-next'));
  await expect.poll(() => page.evaluate('peekSession')).toBe('tab-one-next');

  // The page reloads (a deploy, a discarded tab); tab 2 is rebuilt when shown.
  await page.reload();
  await page.waitForFunction(ready);
  await page.evaluate(id => (window as any)._chromeSwitchTab(id), two);
  const g2 = await frame(two);
  await expect.poll(() => g2.evaluate('peekSession'), {timeout: 8000}).toBe('tab-two-own');
  await page.waitForTimeout(1200);   // past the delayed restore
  expect(await g2.evaluate('peekSession'), 'tab 2 kept its worker').toBe('tab-two-own');
  // And tab 1 is still on its own.
  await page.evaluate(id => (window as any)._chromeSwitchTab(id), one);
  expect(await page.evaluate('peekSession')).toBe('tab-one-next');
});

const WORKERS = [
  {name: 'w-first', running: true, status: 'idle', dir: '/tmp'},
  {name: 'w-second', running: true, status: 'idle', dir: '/tmp'},
  {name: 'w-third', running: true, status: 'idle', dir: '/tmp'},
  {name: 'w-other-window', running: true, status: 'idle', dir: '/tmp'},
];
const READY = "typeof openPeek === 'function' && typeof _chromeAddTab === 'function'";
async function tabFrame(page: any, id: number) {
  const h = await page.waitForSelector(`#chrome-tab-frames iframe[data-tab-id="${id}"]`, {state: 'attached'});
  const f = (await h.contentFrame())!;
  await f.waitForFunction("typeof openPeek === 'function'");
  return f;
}
// Three tabs: the page on w-first, then iframes on w-second and w-third.
async function threeTabs(page: any) {
  await page.goto('/');
  await page.waitForFunction(READY);
  await page.evaluate(() => (window as any).openPeek('w-first'));
  const ids: number[] = [await page.evaluate('_chromeTabs[0].id')];
  for (const w of ['w-second', 'w-third']) {
    const id = await page.evaluate('_chromeAddTab(), _chromeActiveId') as number;
    const f = await tabFrame(page, id);
    await f.evaluate((n: string) => (window as any).openPeek(n), w);
    await expect.poll(() => page.evaluate((i: number) => sessionStorage.getItem('amux_chrome_tab_view:' + i), id), {timeout: 5000}).toBe('#peek=' + w);
    ids.push(id);
  }
  return ids;
}

// Owner, 2026-11-13: "if I'm in a tab and I close the tab, the next tab, which
// replaces the now-closed tab, becomes the session of the tab I closed." The
// first tab is the page itself; the next one was shown AS the page, still on
// the closed tab's worker.
for (const [activeBefore, label] of [[0, 'while it is open'], [1, 'while the next tab is open']] as const) {
  test('closing the first tab ' + label + ' shows the next tab on its own worker', async ({page}, testInfo) => {
    test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
    await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: WORKERS}));
    const ids = await threeTabs(page);
    await page.evaluate((id: number) => (window as any)._chromeSwitchTab(id), ids[activeBefore]);
    await page.evaluate((id: number) => (window as any)._chromeCloseTab(id), ids[0]);
    expect(await page.evaluate('_chromeTabs.map(t => t.id)')).toEqual([ids[1], ids[2]]);
    expect(await page.evaluate('_chromeActiveId'), 'the next tab is the open one').toBe(ids[1]);
    await expect.poll(() => page.evaluate('peekSession'), {timeout: 8000}).toBe('w-second');
    expect(await page.locator(`#chrome-tab-frames iframe[data-tab-id="${ids[1]}"]`).count(), 'its iframe is gone; the page is it now').toBe(0);
    // The third tab is untouched.
    await page.evaluate((id: number) => (window as any)._chromeSwitchTab(id), ids[2]);
    expect(await (await tabFrame(page, ids[2])).evaluate('peekSession')).toBe('w-third');
  });
}

// Two amux windows shared one tab list and one set of tab addresses through
// localStorage: each overwrote the other's, and both gave a new tab the same id.
test('two amux windows keep separate tab bars', async ({page}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'the in-page tab bar is hidden on a phone');
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: WORKERS}));
  const ids = await threeTabs(page);
  // The snapshot follows a worker opened inside a tab (a 2 s check) ...
  await expect.poll(() => page.evaluate(() => JSON.parse(localStorage.getItem('amux_chrome_tabs2') || '[]').map((t: any) => t.hash)), {timeout: 6000})
    .toEqual(['#peek=w-first', '#peek=w-second', '#peek=w-third']);
  // ... so a new window starts from the tabs as they are ...
  const other = await page.context().newPage();
  await other.goto('/');
  await other.waitForFunction(READY);
  expect(await other.evaluate('_chromeTabs.map(t => t.id)')).toEqual(ids);
  await other.evaluate((id: number) => (window as any)._chromeSwitchTab(id), ids[2]);
  await expect.poll(async () => (await tabFrame(other, ids[2])).evaluate('peekSession'), {timeout: 8000}).toBe('w-third');
  // ... and from then on its tabs are its own.
  await other.evaluate((id: number) => (window as any)._chromeSwitchTab(id), ids[1]);
  const of = await tabFrame(other, ids[1]);
  await expect.poll(() => of.evaluate('peekSession'), {timeout: 8000}).toBe('w-second');
  await of.evaluate(() => (window as any).openPeek('w-other-window'));
  await other.evaluate(() => (window as any)._chromeAddTab());
  await other.waitForTimeout(1500);
  expect(await page.evaluate((id: number) => sessionStorage.getItem('amux_chrome_tab_view:' + id), ids[1]), 'the first window keeps its tab').toBe('#peek=w-second');
  expect(await page.evaluate('_chromeTabs.length'), 'the other window\'s new tab is not added here').toBe(3);
  await page.reload();
  await page.waitForFunction(READY);
  expect(await page.evaluate('_chromeTabs.map(t => t.id)'), 'a reload reads this window\'s own bar').toEqual(ids);
  await page.evaluate((id: number) => (window as any)._chromeSwitchTab(id), ids[1]);
  await expect.poll(async () => (await tabFrame(page, ids[1])).evaluate('peekSession'), {timeout: 8000}).toBe('w-second');
  await other.close();
});
