// Worker Messages tab on a phone: the messages get the screen (Ethan,
// 2026-10-23: "fix this real estate on phone ... the most important part in
// this case messages"). Search, kind and menu share one row; each row's text
// spans the card; chrome that repeats the screen (this worker's name, the
// filter's own kind, the normal "direct" path) or serves the terminal is gone.
import { test, expect } from './fixtures';

const W = 'e2e-space-w';
// getBoundingClientRect: WebKit's boundingBox() answers null for this staged panel.
const rect = (l: any) => l.evaluate((e: Element) => { const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; });

async function showMessages(page: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekMessagesRender === 'function');
  await page.evaluate((w: string) => {
    let el: HTMLElement | null = document.getElementById('peek-messages-panel');
    (el as HTMLElement).style.display = 'flex';
    (el as HTMLElement).style.height = '1200px';
    el = (el as HTMLElement).parentElement;
    while (el) {
      el.hidden = false; el.removeAttribute('inert'); el.removeAttribute('aria-hidden');
      if (getComputedStyle(el).display === 'none') el.style.display = 'block';
      if (getComputedStyle(el).visibility === 'hidden') el.style.visibility = 'visible';
      if (getComputedStyle(el).pointerEvents === 'none') el.style.pointerEvents = 'auto';
      el = el.parentElement;
    }
    const now = Date.now();
    const row = (id: number, text: string, extra: any = {}) => ({ id, text, session: w, type: 'user', kind: 'human',
      sender: 'owner', origin: 'owner', delivery: 'direct', time: now - id * 60000, ts: now - id * 60000, ...extra });
    (0, eval)(`peekSession = ${JSON.stringify(w)}; _peekMsgFilter = 'human'; _peekMsgLoading = false;`);
    (window as any).__rows = [row(2, 'OK, so now give me an update on the two workers progress with respect to both the priorities'),
                              row(1, 'stop increasing spend this is insane our entire goal was serverless', { delivery: 'queued' })];
    (0, eval)(`_peekMsgRows = window.__rows.slice().reverse(); _peekMsgRowsFor = peekSession;
               _peekMsgCounts = {all: 366, human: 275, session: 52, schedule: 19}; _peekMsgCountsFor = peekSession;`);
    // The tab switch would fetch this worker's real history over the seeded rows.
    (0, eval)('_peekMessagesLoad = () => {}');
    try { (window as any).setPeekTab('messages'); } catch { /* no worker is open; the dataset is set first */ }
    (window as any)._peekMessagesRender();
  }, W);
}

test('phone: one toolbar row, full-width message text, no repeated chrome', async ({ page }) => {
  await showMessages(page);
  const phone = (page.viewportSize()?.width || 1200) <= 600;
  const rows = page.locator('#peek-messages-list .pm-row');
  await expect(rows).toHaveCount(2);
  await expect(page.locator('#peek-overlay')).toHaveAttribute('data-peek-tab', 'messages');
  if (!phone) {
    // Wide screens keep the chips, the count, the checkbox and the caret.
    await expect(page.locator('#peek-messages-filter .msg-kind-chip').first()).toBeVisible();
    await expect(page.locator('#peek-msgs-kind')).toBeHidden();
    await expect(rows.first().locator('.pm-check')).toBeVisible();
    await expect(rows.first().locator('.msg-caret')).toBeVisible();
    await expect(rows.first().locator('.pm-menu-phone')).toHaveCount(3);
    return;
  }
  // Toolbar: search, kind picker and menu in ONE row; no date trigger, no count, no chip row.
  const kind = page.locator('#peek-msgs-kind');
  await expect(kind).toBeVisible();
  await expect(kind).toHaveValue('human');
  await expect(kind.locator('option:checked')).toHaveText('Human 275');
  await expect(page.locator('#peek-messages-filter')).toBeHidden();
  await expect(page.locator('#peek-messages-count')).toBeHidden();
  // The date picker swaps the input for its own button. That button must
  // carry the input's pm-wide, or "Pick a date" takes the search row.
  const trig = await page.evaluate(() => {
    const inp: any = document.getElementById('peek-msgs-date-jump');
    if (!inp._dpTrigger) (window as any)._dpEnhance(inp);
    const t = inp._dpTrigger as HTMLElement;
    return t ? { exists: true, display: getComputedStyle(t).display } : { exists: false, display: '' };
  });
  expect(trig.exists, 'the date input is enhanced into a trigger').toBe(true);
  expect(trig.display, 'the trigger hides on a phone like the input it replaced').toBe('none');
  const bar = await rect(page.locator('#peek-messages-panel .peek-tasks-add'));
  expect(bar!.height, 'toolbar is one 44px row plus padding').toBeLessThanOrEqual(60);
  const s = await rect(page.locator('#peek-messages-search'));
  const k = await rect(kind);
  expect(Math.abs(s!.y - k!.y), 'search and kind share the row').toBeLessThan(4);
  expect(k!.height).toBeGreaterThanOrEqual(44);
  // Search keeps most of the row, and a control squeezing it is beaconed.
  expect(s!.width / bar!.width, 'search keeps most of the row').toBeGreaterThan(0.45);
  const beacons: string[] = [];
  page.on('request', r => { if ((r.postData() || '').includes('peek-msgs-search-squeezed')) beacons.push(r.url()); });
  await page.evaluate(() => {
    const extra = document.createElement('button'); extra.id = 'e2e-squeeze'; extra.style.cssText = 'flex:0 0 160px;min-height:44px';
    document.querySelector('#peek-messages-panel .peek-tasks-add')!.appendChild(extra);
    (window as any)._peekMessagesRender();
    document.getElementById('e2e-squeeze')!.remove();
  });
  await expect.poll(() => beacons.length, { message: 'squeezed search is beaconed' }).toBeGreaterThan(0);
  // Picking a kind from the picker sets the filter the chips used to set.
  await kind.selectOption('session');
  await expect.poll(() => page.evaluate(() => (0, eval)('_peekMsgFilter'))).toBe('session');
  await page.evaluate(() => { (0, eval)(`_peekMsgFilter = 'human'`); (window as any)._peekMessagesRender(); });

  // Rows: the text spans the card.
  const first = rows.first();
  const card = await rect(first);
  const body = await rect(first.locator('.msg-body'));
  expect(body!.width / card!.width, 'message text uses the card width').toBeGreaterThan(0.85);
  await expect(first.locator('.pm-check')).toBeHidden();
  await expect(first.locator('.msg-caret')).toBeHidden();
  await expect(first.locator('.msg-id-badge')).toBeHidden();
  await expect(first.locator('.msg-deliv-direct')).toBeHidden();
  // A non-normal delivery path still shows.
  await expect(rows.nth(1).getByText('queued', { exact: true })).toBeVisible();
  // Nothing repeats the screen: not this worker's name, not the filter's kind.
  const meta = (await first.locator('.msg-body').evaluate(b => (b.previousElementSibling as HTMLElement | null)?.innerText
    || (b.parentElement!.firstElementChild as HTMLElement).innerText)) || '';
  expect(meta).not.toContain(W);
  expect(meta).not.toMatch(/^Human\b/);
  expect(meta).toContain('owner');
  const dots = await rect(first.locator('.pm-dots'));
  expect(dots!.width).toBeGreaterThanOrEqual(44);
  expect(dots!.y - card!.y, 'menu sits in the header corner').toBeLessThan(8);

  // The controls moved into the row menu, and work there.
  await first.locator('.pm-dots').click();
  const menu = first.locator('.msg-menu');
  await expect(menu.getByText('Copy MSG-2')).toBeVisible();
  await menu.getByText('Collapse').click();
  await expect(first.locator('.msg-body')).toHaveClass(/collapsed/);
  await first.locator('.pm-dots').click();
  await first.locator('.msg-menu').getByText('Select', { exact: true }).click();
  await expect(page.locator('#peek-messages-panel')).toHaveClass(/pm-selecting/);
  await expect(page.locator('#pm-selbar')).toBeVisible();
  await expect(rows.first().locator('.pm-check')).toBeVisible();
  await expect(rows.first().locator('.pm-check')).toBeChecked();
  // After a re-render cycle the state holds.
  await page.evaluate(() => (window as any)._peekMessagesRender());
  await expect(rows.first().locator('.pm-check')).toBeChecked();
  await expect(rows.first().locator('.msg-body')).toHaveClass(/collapsed/);
});

// The filter, up/down and Find used to leave content tabs on a phone; since
// 2026-10-24 they work on every tab's own items (peek-toolbar-every-tab.spec.ts),
// so they stay. The folder path still leaves where it is not context.
test('phone: the toolbar stays on every tab, the folder path leaves content tabs', async ({ page }) => {
  await showMessages(page);
  const phone = (page.viewportSize()?.width || 1200) <= 600;
  const shown = (sel: string) => page.locator(sel).evaluate(e => getComputedStyle(e).display !== 'none');
  const set = (t: string) => page.evaluate(t => { document.getElementById('peek-overlay')!.dataset.peekTab = t; }, t);
  await set('terminal');
  expect(await shown('#peek-msg-nav')).toBe(true);
  expect(await shown('#peek-find-toggle')).toBe(true);
  expect(await shown('.peek-dir-bar')).toBe(true);
  for (const t of ['messages', 'tasks', 'issues', 'memory', 'cost', 'scope']) {
    await set(t);
    expect(await shown('#peek-msg-nav'), t).toBe(true);
    expect(await shown('#peek-find-toggle'), t).toBe(true);
    expect(await shown('.peek-dir-bar'), t).toBe(!phone);
  }
  // The folder path stays where it is context.
  for (const t of ['git', 'commits', 'shell', 'transcript']) {
    await set(t);
    expect(await shown('.peek-dir-bar'), t).toBe(true);
  }
  // Split, worker menu and close stay on every tab.
  await set('messages');
  expect(await shown('#peek-split-toggle')).toBe(true);
  expect(await shown('#peek-close-btn')).toBe(true);
});
