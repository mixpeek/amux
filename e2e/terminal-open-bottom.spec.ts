import { test, expect, Page } from './fixtures';
import type { Route } from '@playwright/test';

const history = 'Earlier context\n'.repeat(160) + '› Locate this earlier request\n' + 'Earlier answer\n'.repeat(160);
const live = 'Latest tool result\n'.repeat(30) + 'LATEST_OUTPUT_END';
async function prepare(page: Page, provider = 'claude', query = '', cached = false) {
  const pending: Route[] = [];
  const worker = {name:'bottom-test', provider, running:true, status:'idle', dir:'/tmp'};
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[worker]}));
  await page.route('**/api/sessions/bottom-test/peek?*', r => { pending.push(r); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(async ({worker,query,cached,history}) => {
    eval('sessions = ['+JSON.stringify(worker)+'];');
    if (cached) await eval('_idb.set("peek_bottom-test", '+JSON.stringify({history,output:'CACHED_OUTPUT_END',time:Date.now()})+');');
    (window as any).openPeek(worker.name, query ? {query} : undefined);
    (window as any)._stopPeekPoll();
  }, {worker,query,cached,history});
  await expect.poll(() => pending.length).toBeGreaterThanOrEqual(2);
  return {
    live: pending.find(r => new URL(r.request().url()).searchParams.has('live'))!,
    full: pending.find(r => !new URL(r.request().url()).searchParams.has('live'))!,
  };
}
async function paint(route: Route, full: boolean) {
  await route.fulfill({json:{name:'bottom-test',live,pane_cols:90,...(full ? {history} : {})}});
}
async function bottom(page: Page) {
  await expect.poll(() => page.locator('#peek-body').evaluate(el => el.scrollHeight-el.scrollTop-el.clientHeight)).toBeLessThan(2);
  await expect(page.locator('#peek-body')).toContainText('LATEST_OUTPUT_END');
}

for (const provider of ['claude','codex','gemini']) for (const first of ['live','history']) {
  test(`${provider} opens at bottom when ${first} arrives first and layout changes late`, async ({page}, info) => {
    const routes = await prepare(page, provider);
    await paint(first === 'live' ? routes.live : routes.full, first === 'history');
    await bottom(page);
    await paint(first === 'live' ? routes.full : routes.live, first === 'live');
    await expect(page.locator('#peek-body')).toContainText('Earlier context');
    await bottom(page);
    // Late plan/composer height and terminal width reflow used to lose the anchor.
    await page.evaluate(() => {
      const body = document.getElementById('peek-body')!;
      body.style.maxHeight = '240px';
      body.style.setProperty('--peek-cols','35');
    });
    await bottom(page);
    await page.screenshot({path:info.outputPath('open-bottom.png')});
  });
}

test('deliberate scroll-up during history load holds until Jump to bottom', async ({page}) => {
  const routes = await prepare(page);
  await paint(routes.live,false); await bottom(page);
  await page.locator('#peek-body').hover();
  await page.mouse.wheel(0,-400);
  await expect.poll(() => page.locator('#peek-body').evaluate(el => el.scrollHeight-el.scrollTop-el.clientHeight)).toBeGreaterThan(100);
  const before = await page.locator('#peek-body').evaluate(el => el.scrollTop);
  await paint(routes.full,true);
  await expect.poll(() => page.evaluate('!!_peekBufferedOutput')).toBe(true);
  expect(await page.locator('#peek-body').evaluate(el => el.scrollTop)).toBe(before);
  await page.locator('#peek-body .scroll-lock-badge').click();
  await expect(page.locator('#peek-body')).toContainText('Earlier context');
  await bottom(page);
});

test('Locate keeps its earlier match after delayed history and layout', async ({page}) => {
  const routes = await prepare(page,'claude','Locate this earlier request');
  await paint(routes.live,false); await paint(routes.full,true);
  await expect(page.locator('.peek-highlight.current')).toBeVisible();
  const before = await page.locator('#peek-body').evaluate(el => el.scrollTop);
  await page.evaluate(() => { document.getElementById('peek-body')!.style.maxHeight='240px'; });
  await expect(page.locator('#peek-body')).toHaveJSProperty('scrollTop', before);
  expect(await page.locator('#peek-body').evaluate(el => el.scrollHeight-el.scrollTop-el.clientHeight)).toBeGreaterThan(100);
});

test('cached opening follows through fresh live/history replacement', async ({page}) => {
  const routes = await prepare(page,'claude','',true);
  await expect(page.locator('#peek-body')).toContainText('CACHED_OUTPUT_END');
  await expect.poll(() => page.locator('#peek-body').evaluate(el => el.scrollHeight-el.scrollTop-el.clientHeight)).toBeLessThan(2);
  await paint(routes.live,false); await paint(routes.full,true);
  await bottom(page);
});

test('cached Locate keeps its match while fresh output arrives', async ({page}) => {
  const routes = await prepare(page,'claude','Locate this earlier request',true);
  await expect(page.locator('.peek-highlight.current')).toBeVisible();
  const top = await page.locator('#peek-body').evaluate(el => el.scrollTop);
  await paint(routes.live,false); await paint(routes.full,true);
  await expect(page.locator('#peek-body')).toHaveJSProperty('scrollTop',top);
  expect(await page.locator('#peek-body').evaluate(el => el.scrollHeight-el.scrollTop-el.clientHeight)).toBeGreaterThan(100);
});
