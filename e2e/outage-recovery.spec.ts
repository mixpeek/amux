import { test, expect, Page, allowUnusedRoute } from './fixtures';
import type { Route } from '@playwright/test';

async function setup(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_device_name', 'Outage regression');
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openBoardDetail === 'function');
}
async function createCard(page: Page, title: string) {
  return page.evaluate(async title => {
    const r = await fetch('/api/board', { method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({title, desc: 'Original description', type: 'chore', status: 'todo'}) });
    if (!r.ok) throw new Error(await r.text());
    return r.json();
  }, title);
}
async function openCard(page: Page, id: string) {
  await page.evaluate(id => (window as any).openBoardDetail(id), id);
  await expect(page.locator('#bd-key')).toHaveText(id);
  await page.waitForFunction(id => (window as any).eval('_bdHydrated && _bdLoadedIdentity?.id') === id, id);
  await page.locator('#bd-tab-edit').click();
}
async function queue(page: Page) {
  return page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]'));
}
const save = (page: Page) => page.locator('#bd-edit-footer button[onclick="boardDetailSave()"]').click();

test('Saved means an exact card was committed and survives reload', async ({page}) => {
  await setup(page);
  const card = await createCard(page, 'Outage happy');
  await openCard(page, card.id);
  await page.locator('#bd-title').fill('Durable happy edit');
  await save(page);
  await expect(page.locator('#bd-save-status')).toHaveText('Saved');
  expect(await queue(page)).toHaveLength(0);
  await page.reload();
  await openCard(page, card.id);
  await expect(page.locator('#bd-title')).toHaveValue('Durable happy edit');
});

for (const failure of [{status:500, reason:'pool timeout'}, {status:507, reason:'server ENOSPC'}]) {
test(`${failure.reason} retains gate and title intent across reload then retries durably`, async ({page}) => {
  await setup(page);
  const card = await createCard(page, 'Outage pool timeout');
  await openCard(page, card.id);
  let fail = true;
  let attempts = 0;
  await page.route(`**/api/board/${card.id}`, async route => {
    if (route.request().method() !== 'PATCH') return route.continue();
    attempts++;
    if (fail) return route.fulfill({status: failure.status, body: failure.reason});
    return route.continue();
  });
  await page.locator('#bd-title').fill('Queued title survives');
  await page.locator('#bd-tab-edit').click();
  await page.locator('#bd-gate').fill('Verify the durable write');
  await save(page);
  await expect(page.locator('#bd-save-status')).toContainText('Not saved');
  await expect(page.locator('#conn-status').first()).not.toHaveText('Live');
  await expect.poll(async () => (await queue(page)).length).toBe(1);
  const persisted = (await queue(page))[0];
  expect(JSON.parse(persisted.options.body).gate).toEqual(['Verify the durable write']);
  await page.reload();
  expect((await queue(page))[0].id).toBe(persisted.id);
  await openCard(page, card.id);
  await expect(page.locator('#bd-title')).toHaveValue('Queued title survives');
  await expect(page.locator('#bd-gate')).toHaveValue('Verify the durable write');
  fail = false;
  await page.evaluate(() => (window as any).runSyncBanner());
  await expect.poll(async () => (await queue(page)).length).toBe(0);
  await openCard(page, card.id);
  await expect(page.locator('#bd-title')).toHaveValue('Queued title survives');
  expect(attempts).toBeGreaterThanOrEqual(2);
});

}

test('in-flight replay remains durable and concurrent flushes send once', async ({page}) => {
  await setup(page);
  const card = await createCard(page, 'Outage replay');
  await page.evaluate(async card => {
    (window as any).eval('online = false');
    await (window as any)._queueOp('/api/board/' + card.id, {method: 'PATCH', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({title: 'Replayed once', expect_rev: card.rev})});
  }, card);
  let release!: () => void;
  const hold = new Promise<void>(resolve => { release = resolve; });
  let writes = 0;
  await page.route(`**/api/board/${card.id}`, async route => {
    if (route.request().method() !== 'PATCH') return route.continue();
    writes++;
    await hold;
    await route.continue().catch(() => {});
  });
  await page.evaluate(() => {
    (window as any).eval('online = true');
    void (window as any).runSyncBanner();
    void (window as any).runSyncBanner();
  });
  await expect.poll(() => writes).toBe(1);
  expect(await queue(page)).toHaveLength(1);
  // Durable queue must still contain the request while the response is pending.
  release();
  await expect.poll(async () => (await queue(page)).length).toBe(0);
  expect(writes).toBe(1);
});

test('device ENOSPC refuses submission and preserves the edit', async ({page}) => {
  await setup(page);
  const card = await createCard(page, 'Outage storage');
  await openCard(page, card.id);
  await page.locator('#bd-title').fill('Do not lose this draft');
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function(key, value) {
      if (key === 'amux_offline_queue') throw new DOMException('No space left', 'QuotaExceededError');
      return original.call(this, key, value);
    };
  });
  await save(page);
  await expect(page.locator('#bd-save-status')).toContainText('Not saved');
  await expect(page.locator('#bd-title')).toHaveValue('Do not lose this draft');
  expect(await queue(page)).toHaveLength(0);
  const durable = await page.evaluate(async id => (await (await fetch('/api/board/' + id)).json()).title, card.id);
  expect(durable).toBe(card.title);
});

test('a cold second card cannot save controls from the previous card', async ({page}) => {
  await setup(page);
  const first = await createCard(page, 'Previous card');
  const second = await createCard(page, 'Requested card');
  await openCard(page, first.id);
  let release!: () => void;
  const hold = new Promise<void>(resolve => { release = resolve; });
  await page.route(`**/api/board/${second.id}`, async route => { await hold; await route.continue(); });
  await page.evaluate(id => {
    (window as any).eval('boardItems = boardItems.filter(i => i.id !== ' + JSON.stringify(id) + ')');
    void (window as any).openBoardDetail(id);
  }, second.id);
  await page.locator('#bd-title').fill('Wrong card edit');
  await save(page);
  expect(await queue(page)).toHaveLength(0);
  release();
  await expect(page.locator('#bd-key')).toHaveText(second.id);
  const unchanged = await page.evaluate(async id => (await (await fetch('/api/board/' + id)).json()).title, first.id);
  expect(unchanged).toBe('Previous card');
});

test('a peer edit causes a visible revision conflict without clobbering either draft', async ({page, request}) => {
  await setup(page);
  const card = await createCard(page, 'Revision base');
  await openCard(page, card.id);
  await page.locator('#bd-title').fill('My retained draft');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const peer = await request.patch(`/api/board/${card.id}`, {headers: {Authorization: `Bearer ${token}`},
    data: {title: 'Peer committed title', expect_rev: card.rev}});
  expect(peer.ok()).toBeTruthy();
  await save(page);
  await expect(page.locator('#bd-save-status')).toContainText('Not saved');
  const pending = await queue(page);
  expect(pending[0].state).toBe('blocked');
  expect(JSON.parse(pending[0].options.body).title).toBe('My retained draft');
  const durable = await page.evaluate(async id => (await (await fetch('/api/board/' + id)).json()).title, card.id);
  expect(durable).toBe('Peer committed title');
});

test('a board read failure overrides a live SSE connection', async ({page}) => {
  await setup(page);
  await page.route('**/api/board/statuses', route => route.fulfill({status: 500, body: 'timed out waiting for connection'}));
  await page.evaluate(async () => {
    (window as any).eval('_liveSSE = true; online = true');
    await (window as any).fetchBoard();
    (window as any).eval('_liveSSE = true; online = true');
    (window as any).updateConnectionStatus();
  });
  await expect(page.locator('#conn-status').first()).toHaveText('Sync error');
});


test('a second tab cannot replay an edit whose original request is still in flight', async ({page, context}) => {
  await setup(page);
  const card = await createCard(page, 'Cross-tab delivery');
  await openCard(page, card.id);
  const second = await context.newPage(); await setup(second);
  let release!: () => void; const hold = new Promise<void>(resolve => { release = resolve; });
  let writes = 0;
  const matcher = `**/api/board/${card.id}`;
  const intercept = async (route: Route) => {
    if (route.request().method() !== 'PATCH') return route.continue();
    writes++; await hold; await route.continue();
  };
  // Each tab uses the existing counted page stub. A second delivery is the
  // failure under test; no request from that tab is the expected result.
  await page.route(matcher, intercept);
  await second.route(matcher, intercept);
  allowUnusedRoute(second, matcher); // the delivery lock must prevent its PATCH
  await page.locator('#bd-title').fill('One committed delivery'); await save(page);
  await expect.poll(() => writes).toBe(1);
  await second.evaluate(() => { void (window as any).runSyncBanner(); });
  expect(await queue(second)).toHaveLength(1);
  release();
  await expect(page.locator('#bd-save-status')).toHaveText('Saved');
  await expect.poll(async () => (await queue(second)).length).toBe(0);
  expect(writes).toBe(1);
  await second.close();
});

test('typing during a save keeps the newer draft through reload', async ({page}) => {
  await setup(page); const card = await createCard(page, 'Concurrent editing');
  await openCard(page, card.id);
  let release!: () => void; const hold = new Promise<void>(resolve => { release = resolve; });
  let writes = 0;
  await page.route(`**/api/board/${card.id}`, async route => {
    if (route.request().method() !== 'PATCH') return route.continue();
    writes++; await hold; await route.continue();
  });
  await page.locator('#bd-title').fill('First saved edit'); await save(page);
  await expect.poll(() => writes).toBe(1);
  await page.locator('#bd-title').fill('Newer retained draft'); release();
  await expect(page.locator('#bd-save-status')).toContainText('newer changes not saved');
  await page.reload(); await openCard(page, card.id);
  await expect(page.locator('#bd-title')).toHaveValue('Newer retained draft');
  const durable = await page.evaluate(async id => (await (await fetch('/api/board/' + id)).json()).title, card.id);
  expect(durable).toBe('First saved edit');
});
