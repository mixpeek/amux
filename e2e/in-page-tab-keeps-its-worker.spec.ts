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
  await expect.poll(() => page.evaluate(id => localStorage.getItem('amux_chrome_tab_view:' + id), two), {timeout: 5000}).toBe('#peek=tab-two-own');

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
