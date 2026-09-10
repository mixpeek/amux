import { test, expect, Page } from './fixtures';

const workers = [
  { name: 'mixpeek-general', status: 'idle', last_activity: 9999 },
  { name: 'waiting-one', status: 'waiting', last_activity: 9000 },
  { name: 'worker-b', status: 'active', last_activity: 100 },
  { name: 'worker-a', status: 'active', last_activity: 100 },
  { name: 'pinned-idle', status: 'idle', pinned: true, last_activity: 1 },
  { name: 'api-error', status: 'api_error', last_activity: 500 },
  { name: 'rate-limit', status: 'rate_limited', last_activity: 600 },
  { name: 'unattributed', status: 'unattributed', last_activity: 8000 },
  { name: 'stopped', status: 'active', running: false, pinned: true, last_activity: 90000 },
].map(w => ({ running: true, pinned: false, dir: '/tmp/', provider: 'codex', tags: [], ...w }));
// Pin to top remains above every status bucket, including stopped workers.
const ordered = ['pinned-idle', 'stopped', 'worker-a', 'worker-b', 'waiting-one', 'unattributed', 'api-error',
  'rate-limit', 'mixpeek-general'];

async function prepare(page: Page, savedSort: string | null = null) {
  await page.addInitScript(({ workers, savedSort }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_sessions_cache', JSON.stringify(workers));
    localStorage.removeItem('amux_frozen');
    localStorage.removeItem('amux_card_order');
    if (savedSort === null) localStorage.removeItem('amux_sort_mode');
    else localStorage.setItem('amux_sort_mode', savedSort);
  }, { workers, savedSort });
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: workers }));
  await page.goto('/');
  await expect(page.locator('#cards .card[data-session]')).toHaveCount(workers.length);
}

async function names(page: Page) {
  return page.locator('#cards .card[data-session]').evaluateAll(nodes => nodes.map(n => (n as HTMLElement).dataset.session));
}

test('default status order keeps mixpeek-general with idle workers and updates on status changes', async ({ page }, testInfo) => {
  await prepare(page);
  await expect(page.locator('#tile-sort-btn')).toHaveAttribute('title', 'Sort: Status');
  expect(await names(page)).toEqual(ordered);
  await page.screenshot({ path: testInfo.outputPath('worker-status-order.png'), fullPage: true });
  await page.evaluate(() => {
    eval("sessions.find(s => s.name === 'mixpeek-general').status = 'active'; render();");
  });
  expect((await names(page)).slice(0, 5)).toEqual(['pinned-idle', 'stopped', 'mixpeek-general', 'worker-a', 'worker-b']);
});

test('grouped order, a single remaining group, and freeze use the same status buckets', async ({ page }) => {
  await prepare(page, 'obsolete-sort');
  await page.evaluate(() => {
    eval("_tagGroupCollapsed.stopped = false; setLayoutMode('group');");
  });
  expect(await names(page)).toEqual(ordered);
  await page.evaluate(() => (window as any).toggleFreeze());
  expect(await names(page)).toEqual(ordered);
  await page.evaluate(() => {
    (window as any).toggleFreeze();
    eval("sessions = sessions.filter(s => s.status === 'idle'); render();");
  });
  expect(await names(page)).toEqual(['pinned-idle', 'mixpeek-general']);
});

test('explicit name sorting remains available and a broken status order announces itself', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async r => {
    beacons.push(r.request().postDataJSON());
    await r.fulfill({ json: { ok: true } });
  });
  await prepare(page, 'alpha');
  await expect(page.locator('#tile-sort-btn')).toHaveAttribute('title', 'Sort: Name (A–Z)');
  await page.evaluate(() => (window as any).setSortMode('status'));
  expect(await names(page)).toEqual(ordered);
  await expect.poll(() => beacons.find(b => b.kind === 'worker-status-order')).toMatchObject({
    verdict: 'status-order-ok', measured: true, n_considered: workers.filter(w => !w.pinned).length,
  });
  // Exercise the actual DOM checker independently of the sort implementation.
  await page.evaluate(() => {
    const cards = document.getElementById('cards')!;
    cards.prepend(cards.querySelector('[data-session="mixpeek-general"]')!);
    (window as any)._checkWorkerStatusOrder();
  });
  await expect.poll(() => beacons.find(b => b.verdict === 'status-order-violation')).toMatchObject({
    measured: true, n_considered: workers.filter(w => !w.pinned).length,
    violation: { before: 'mixpeek-general', after: 'worker-a' },
  });
});
