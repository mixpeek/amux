import { test, expect, type Page } from './fixtures';

/**
 * Ethan, 2026-10-17: "make a new tab ... called 'tasks' and it should be all the
 * tasks in sequential order with each worker tagged, each one clickable and any
 * other data associated with it", then "tasks still look to be displaying
 * messages not tasks".
 *
 * A task is what a worker is working on: the "Task:" line on its card, recorded
 * each time it changes. Against the real server a new test worker takes two tasks
 * from its board (one card in doing, then the next), and receives a message
 * while on the second. The Tasks tab lists the two tasks, never the message;
 * the second task's detail holds the message and its card.
 */
async function seedTwoTasks(page: Page, nonce: string) {
  const worker = 'e2e-tasks-' + nonce;
  return page.evaluate(async ({ worker, nonce }) => {
    const j = async (r: Response) => ({ status: r.status, body: await r.json().catch(() => ({})) });
    const post = (path: string, body: object) => fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).then(j);
    const patch = (path: string, body: object) => fetch(path, { method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).then(j);
    const recorded = async (n: number) => {
      for (let i = 0; i < 40; i++) {
        await fetch('/api/sessions');
        const d = await (await fetch('/api/task-history?session=' + encodeURIComponent(worker) + '&q=' + encodeURIComponent(nonce))).json();
        if ((d.total || 0) >= n) return d.total;
        await new Promise(res => setTimeout(res, 500));
      }
      return -1;
    };
    // A stopped worker is enough: its task line comes from its doing card.
    const mk = await post('/api/sessions', { name: worker, dir: '/tmp', provider: 'codex' });
    const a = await post('/api/board', { title: 'First task ' + nonce, status: 'doing', session: worker });
    const first = await recorded(1);
    // A peer worker with its own task hands off to the worker: its message
    // arrives just before the worker's second task is named.
    const peer = worker + '-peer';
    await post('/api/sessions', { name: peer, dir: '/tmp', provider: 'codex' });
    await post('/api/board', { title: 'Peer task ' + nonce, status: 'doing', session: peer });
    for (let i = 0; i < 40; i++) {
      await fetch('/api/sessions');
      const d = await (await fetch('/api/task-history?session=' + encodeURIComponent(peer))).json();
      if ((d.total || 0) >= 1) break;
      await new Promise(res => setTimeout(res, 500));
    }
    await post('/api/history', { text: 'handoff from the peer ' + nonce, type: 'session', session: worker, origin: peer });
    await patch('/api/board/' + a.body.id, { status: 'done', evidence: 'none: e2e seed' });
    const b = await post('/api/board', { title: 'Second task ' + nonce, status: 'doing', session: worker });
    const second = await recorded(2);
    const msg = await post('/api/history', { text: 'a message during the second task ' + nonce, type: 'user', session: worker });
    return { worker, peer, cards: [a.body.id, b.body.id], statuses: [mk.status, a.status, b.status], first, second, msg: msg.status };
  }, { worker, nonce });
}

test('Tasks lists the tasks a worker took on, in order, and a task opens its card and messages', async ({ page }) => {
  // The seed waits up to 40 s for the server to record two tasks and a peer's.
  test.setTimeout(120_000);
  const nonce = Date.now().toString(36);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', { timeout: 20000 });
  const seed = await seedTwoTasks(page, nonce);
  expect(seed.statuses, JSON.stringify(seed)).toEqual([201, 201, 201]);
  expect([seed.first, seed.second], JSON.stringify(seed)).toEqual([1, 2]);

  await page.locator('#tab-tasks').click();
  await expect(page.locator('#tasks-view')).toBeVisible();
  const fetched = page.waitForResponse(r => r.url().includes('/api/task-history?') && r.url().includes('q=' + nonce));
  await page.locator('#tasks-search').fill(nonce);
  await fetched;

  // The peer's task matches the nonce too (the timeline below needs its
  // lane); these checks are about the worker's own tasks.
  const items = page.locator('#tasks-list .task-item')
    .filter({ has: page.locator('.msg-sess-link', { hasText: new RegExp('^' + seed.worker + '$') }) });
  await expect(items).toHaveCount(2, { timeout: 15000 });
  await expect(items.nth(0).locator('.task-title')).toHaveText('First task ' + nonce);
  await expect(items.nth(1).locator('.task-title')).toHaveText('Second task ' + nonce);
  // Numbers are places in the whole list, which also holds the peer's task.
  const nums = await items.locator('.task-num').allTextContents();
  expect(nums.map(t => Number(t.replace('#', ''))), nums.join(' ')).toEqual([expect.any(Number), expect.any(Number)]);
  expect(Number(nums[0].slice(1)) < Number(nums[1].slice(1)), nums.join(' ')).toBe(true);
  for (let i = 0; i < 2; i++) await expect(items.nth(i).locator('.msg-sess-link')).toHaveText(seed.worker);
  await expect(items.nth(0)).toContainText(/ran \d/);
  await expect(items.nth(1)).toContainText('current');
  // A message is never a task row.
  await expect(page.locator('#tasks-list')).not.toContainText('a message during the second task');

  // The second task opens: started, current, its card and the message it received.
  await items.nth(1).locator('.task-row .task-title').click();
  const detail = items.nth(1).locator('.task-detail');
  await expect(detail).toBeVisible();
  await expect(detail).toContainText('Started');
  await expect(detail).toContainText('Current task');
  await expect(detail.locator('.task-card-open')).toContainText(String(seed.cards[1]));
  await expect(detail.locator('.task-msgs')).toContainText('a message during the second task ' + nonce, { timeout: 10000 });
  // The first task ended when the second began, and did not receive that message.
  await items.nth(0).locator('.task-row .task-title').click();
  const first = items.nth(0).locator('.task-detail');
  await expect(first).toContainText('Ended');
  await expect(first.locator('.task-msgs')).not.toContainText('a message during the second task');
  // Clicking again closes it.
  await items.nth(0).locator('.task-row .task-title').click();
  await expect(first).toBeHidden();

  // Each row says it links one message and its card; the explainer says these
  // are not board issues (Ethan, 2026-10-18).
  // The explanation is an info button at the end of the search row (one control
  // row, 2026-10-23): its label says it, and its panel opens with the detail.
  await expect(page.locator('#tasks-view .tasks-about summary')).toHaveAttribute('aria-label', /not board issues/);
  await expect(page.locator('#tasks-view .tasks-about .row-note-body')).toContainText('not board issues');
  // The server's count, so a wrong chip says whether the API or the render is wrong.
  const api = await page.evaluate(async ({ w, n }) => (await (await fetch('/api/task-history?order=asc&session=' + encodeURIComponent(w) + '&q=' + encodeURIComponent(n))).json()).tasks.map((t: any) => t.messages), { w: seed.worker, n: nonce });
  expect(api, JSON.stringify(api)).toEqual([expect.any(Number), 2]);
  await expect(items.nth(1).locator('.task-link-msgs')).toContainText('2');
  await expect(items.nth(1).locator('.task-row .task-link-card')).toContainText(String(seed.cards[1]));
  // The connection track has one tick for that message, and it focuses the row.
  await expect(detail.locator('.task-tick')).toHaveCount(2);
  // The peer's message is the handoff that started it; a worker with no git
  // checkout says its commits were not read rather than showing none.
  await expect(detail.locator('.task-handoff')).toContainText(seed.peer);
  await expect(detail.locator('.task-commits')).toContainText('Commits not read');
  // Two ticks: the handoff and the message sent during the task.
  await expect(detail.locator('.task-tick')).toHaveCount(2);
  await detail.locator('.task-tick').last().click();
  await expect(detail.locator('.task-msg').first()).toBeVisible();

  // Search reaches the message text and shows where it hit; Names does not.
  const hit = page.waitForResponse(r => r.url().includes('/api/task-history?') && r.url().includes('in=all'));
  await page.locator('#tasks-search').fill('during the second task ' + nonce);
  await hit;
  await expect(items).toHaveCount(1, { timeout: 15000 });
  await expect(items.nth(0).locator('.task-title')).toHaveText('Second task ' + nonce);
  await expect(items.nth(0).locator('.task-hit mark')).toHaveText('during the second task ' + nonce);
  await page.locator('#tasks-in button[data-in="names"]').click();
  await expect(page.locator('#tasks-count')).toHaveText('No tasks match', { timeout: 15000 });
  await page.locator('#tasks-in button[data-in="all"]').click();
  await expect(items).toHaveCount(1, { timeout: 15000 });
  // The snippet opens that message in Messages.
  await items.nth(0).locator('.task-hit').click();
  await expect(page.locator('#tab-messages')).toHaveClass(/active/);
  await expect(page.locator('#msgs-search')).toHaveValue(/^MSG-\d+$/);

  // Timeline: the worker's lane holds both tasks; a bar opens its task in the list.
  await page.locator('#tab-tasks').click();
  await page.locator('#tasks-search').fill(nonce);
  await page.locator('#tasks-mode button[data-mode="timeline"]').click();
  const lane = page.locator('#tasks-list .tl-row[data-lane="' + seed.worker + '"]');
  await expect(lane.locator('.tl-bar')).toHaveCount(2, { timeout: 15000 });
  // The handoff draws one arrow from the peer's lane to the second task.
  await expect(page.locator('#tasks-list .tl-row[data-lane="' + seed.peer + '"] .tl-bar')).toHaveCount(1);
  await expect(page.locator('#tasks-list .tl-arrows')).toHaveAttribute('data-arrows', '1');
  await expect(lane.locator('.tl-bar[data-from="' + seed.peer + '"]')).toHaveCount(1);
  // Both tasks lasted seconds, so their bars overlap on this lane's scale;
  // the click goes to the bar itself.
  await lane.locator('.tl-bar').first().dispatchEvent('click');
  await expect(page.locator('#tasks-mode button[data-mode="list"]')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('#tasks-list .task-item.open .task-detail')).toBeVisible({ timeout: 15000 });

  // The card opens on the board, and its detail lists the task that came from
  // it, which opens back here.
  await items.nth(1).locator('.task-row .task-link-card').click();
  await expect(page.locator('#tab-board')).toHaveClass(/active/);
  await expect(page.locator('body')).toContainText('Second task ' + nonce, { timeout: 10000 });
  const back = page.locator('#bd-worker-tasks');
  await expect(back).toBeVisible({ timeout: 15000 });
  await expect(back).toContainText('Second task ' + nonce);
  await back.locator('button', { hasText: 'Second task ' + nonce }).click();
  await expect(page.locator('#tab-tasks')).toHaveClass(/active/);
  await expect(page.locator('#tasks-list .task-item.open .task-title')).toHaveText('Second task ' + nonce, { timeout: 15000 });

  // In Messages, the message names the task its worker was on, and opens it.
  await page.locator('#tab-messages').click();
  // The Messages tab opens on a channel; the message log is "All messages".
  const allMsgs = page.locator('#cv-all-item');
  if (await allMsgs.isVisible()) await allMsgs.click();
  else await page.evaluate(() => (window as any)._cvSelect?.('@all'));
  await page.locator('#msgs-search').fill('during the second task ' + nonce);
  await page.locator('#msgs-search').press('Enter');
  const row = page.locator('#msgs-list [data-msg-key]', { hasText: 'a message during the second task ' + nonce }).first();
  await expect(row.locator('.msg-task-link')).toContainText('during: Second task ' + nonce, { timeout: 15000 });
  // Scrolled to the list's top edge, the sticky day header covers the chip on phones.
  await row.locator('.msg-task-link').dispatchEvent('click');
  await expect(page.locator('#tab-tasks')).toHaveClass(/active/);
  await expect(page.locator('#tasks-list .task-item.open .task-title')).toHaveText('Second task ' + nonce, { timeout: 15000 });

  // A task's link opens that task on a fresh load.
  const link = await page.evaluate(async (w) => {
    const d = await (await fetch('/api/task-history?order=asc&session=' + encodeURIComponent(w))).json();
    return (window as any)._tasksLink(w, d.tasks[0].ts);
  }, seed.worker);
  expect(link).toContain('#task=');
  await page.goto(link);
  await expect(page.locator('#tasks-list .task-item.open .task-title')).toHaveText('First task ' + nonce, { timeout: 20000 });
});

test("a worker's Tasks tab lists its tasks newest first, with links, never its messages", async ({ page }) => {
  // Ethan, 2026-10-17: "the tasks tab is still wrong in the worker details
  // page. it shows messages it should be task names from the model." and
  // 2026-10-18: the same rich list as the Tasks view, linked to its messages
  // and cards, and saying these are not board issues.
  const nonce = Date.now().toString(36);
  const worker = 'e2e-probe';
  const message = 'an owner message that is not a task name ' + nonce;
  const names = ['Fix the tasks tab ' + nonce, 'Investigate the reminder ' + nonce];
  const now = Date.now() / 1000;
  const urls: string[] = [];
  await page.route('**/api/task-history?**', route => {
    const u = new URL(route.request().url());
    if (u.searchParams.get('session') !== worker) return route.continue();
    urls.push(u.search);
    return route.fulfill({ json: { measured: true, n_considered: 2, total: 2, offset: 0, limit: 100, order: 'desc', tasks: [
      { session: worker, task: names[0], previous: names[1], card: 'AMUX-1', card_title: 'A card', card_status: 'doing', source: 'board', ts: now - 60, ended: null, messages: 3, board_off: false },
      { session: worker, task: names[1], previous: '', card: '', source: 'generated_title', ts: now - 3600, ended: now - 60, messages: 0, board_off: true },
    ] } });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
  const status = await page.evaluate(async ({ worker, message }) => (await fetch('/api/history', { method: 'POST',
    headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text: message, type: 'user', session: worker }) })).status,
    { worker, message });
  expect(status).toBe(200);

  await page.evaluate((w) => (window as any).openPeek(w), worker);
  await expect(page.locator('#peek-overlay')).toBeVisible();
  // The tab sits in a row that scrolls sideways. On WebKit the row can still be
  // settling after Playwright scrolls the tab into view, and the tap lands
  // beside it: "click action done" with no tab chosen (ios-safari on 19e1e55ca,
  // both attempts). Tap again until it takes; a handler that never works still fails.
  const tasksTab = page.locator('#peek-tab-tasks');
  await expect(async () => {
    await tasksTab.click();
    await expect(tasksTab).toHaveClass(/active/, { timeout: 1000 });
  }).toPass({ timeout: 15_000 });
  const panel = page.locator('#peek-tasks-panel');
  await expect(panel.locator('.tasks-about summary')).toHaveAttribute('aria-label', /not board issues/);
  const items = panel.locator('.task-item');
  await expect(items).toHaveCount(2, { timeout: 15000 });
  expect(urls[0], 'the worker tab asks for newest first').toContain('order=desc');
  await expect(items.nth(0).locator('.task-title')).toHaveText(names[0]);
  await expect(items.nth(0).locator('.task-num')).toHaveText('#2');
  await expect(items.nth(0)).toContainText('current');
  await expect(items.nth(0).locator('.task-link-msgs')).toContainText('3');
  await expect(items.nth(0).locator('.task-link-card')).toContainText('AMUX-1');
  await expect(items.nth(0).locator('.task-link-card')).toContainText('doing');
  await expect(items.nth(1).locator('.task-title')).toHaveText(names[1]);
  await expect(items.nth(1)).toContainText('board off');
  await expect(panel).not.toContainText(message);
  await expect(page.locator('#peek-tasks-count')).toContainText('2 tasks, newest first');
  const box = await items.nth(0).locator('.task-row').boundingBox();
  expect(box!.height, 'a 44px touch row').toBeGreaterThanOrEqual(44);
  // Search here covers messages too.
  await panel.locator('#peek-tasks-search').fill('reminder');
  await expect.poll(() => urls.some(u => u.includes('q=reminder') && u.includes('in=all'))).toBe(true);
});
