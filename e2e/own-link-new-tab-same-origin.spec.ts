import {test, expect} from './fixtures';

// Owner, 2026-11-13: middle-clicking a link opened a new tab in light mode.
// Workers write links on the server's public address (/api/deep-link's base),
// while the dashboard may be open on localhost. A plain click opens such a link
// here; a middle click let the browser open the public address, another origin
// with its own localStorage: no theme (so the server's default won), drafts,
// tab order or worker views. The link is now pointed at this origin first.
const PUBLIC = 'https://amux-public.example.test';

test('a middle-clicked link to this amux opens a new tab on this origin, keeping the theme', async ({page, baseURL}, testInfo) => {
  test.skip(testInfo.project.name === 'ios-safari', 'a phone has no middle click, modifier click or Open Link in New Tab');
  await page.route('**/api/deep-link', r => r.fulfill({json: {base: PUBLIC, file_prefix: '/f', worker_prefix: '/w', measured: true, n_considered: 1}}));
  await page.goto('/');
  await page.waitForFunction("typeof themeActivate === 'function' && typeof _ownLinkHash === 'function'");
  await expect.poll(() => page.evaluate("_ownLinkHash('" + PUBLIC + "/#view=board')")).toBe('#view=board');
  await page.evaluate(() => (window as any).themeActivate('amux-dark', {local: true}));
  await page.evaluate(href => {
    const a = document.createElement('a');
    a.id = 'own-public-link'; a.href = href; a.textContent = 'board';
    a.style.cssText = 'position:fixed;top:200px;left:200px;z-index:2147483647;font-size:24px';
    document.body.appendChild(a);
  }, PUBLIC + '/#view=board');

  // Every way a browser opens a link in a new tab starts with one of these:
  // a middle or modifier press, the context menu, or keyboard focus. Each must
  // leave the link pointing at this origin before the browser reads it.
  const here = new URL(baseURL!).origin + '/#view=board';
  for (const [ev, init] of [['pointerdown', {button: 1}], ['contextmenu', {button: 2}], ['focusin', {}]] as const) {
    await page.evaluate(href => document.getElementById('own-public-link')!.setAttribute('href', href), PUBLIC + '/#view=board');
    await page.dispatchEvent('#own-public-link', ev, init);
    expect(await page.getAttribute('#own-public-link', 'href'), ev + ' points the link here').toBe(here);
  }
  // And the tab it opens is this dashboard, on that screen, in this theme.
  const tab = await page.context().newPage();
  await tab.goto(await page.getAttribute('#own-public-link', 'href') as string);
  await tab.waitForFunction("typeof activeView !== 'undefined' && activeView === 'board'");
  expect(await tab.evaluate("document.body.dataset.theme")).toBe('amux-dark');
  await tab.close();
});
