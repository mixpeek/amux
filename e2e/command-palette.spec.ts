// Cmd/Ctrl-K finds any amux thing by name and opens it (Ethan, 2026-10-19:
// "cmd + k i can quickly get the worker, group, etc."). Workers, groups and
// tabs are matched in the page; cards come from the server's /api/palette.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

async function boot(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openCmdk === 'function' && typeof (window as any).fetchSessions === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const ui = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  return { auth: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, ui };
}

async function palette(page: Page, q: string) {
  await page.keyboard.press('Control+K');
  const input = page.locator('#cmdk-input');
  await expect(input).toBeFocused();
  await input.fill(q);
  return input;
}

test('Cmd-K opens a worker, a card, a tab and a group by name', async ({ page, request }, info) => {
  test.skip(info.project.name !== 'desktop', 'keyboard flow; the button is covered on phones below');
  test.setTimeout(90_000);
  const { auth, ui } = await boot(page);
  const stamp = Date.now();
  const worker = `cmdk-${stamp}`;
  const group = `cmdkgrp${stamp}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name: worker, dir: '/tmp', start: false, tags: [group] } })).status()).toBe(201);
  const card = await request.post('/api/board', { headers: auth, data: { title: `Quokka migration plan ${stamp}`, status: 'todo' } });
  expect(card.ok()).toBeTruthy();
  const cardId = (await card.json()).id;
  try {
    await expect.poll(() => page.evaluate(async (n) => {
      await (window as any).fetchSessions();
      return (0, eval)('sessions').some((s: any) => s.name === n);
    }, worker), { timeout: 30_000 }).toBe(true);

    // Worker: first row, Enter opens its peek.
    let input = await palette(page, worker);
    const first = page.locator('#cmdk-list .cmdk-row').first();
    await expect(first).toContainText(worker);
    await expect(first.locator('.cmdk-kind')).toHaveText('Worker');
    await input.press('Enter');
    await expect(page.locator('#cmdk-overlay')).not.toHaveClass(/active/);
    await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
    expect(await page.evaluate(() => (0, eval)('peekSession'))).toBe(worker);
    await page.evaluate(() => (window as any).closePeek());

    // Card: found by the server's name index, opens its detail.
    input = await palette(page, `quokka ${stamp}`);
    const cardRow = page.locator('#cmdk-list .cmdk-row', { hasText: `Quokka migration plan ${stamp}` });
    await expect(cardRow).toBeVisible({ timeout: 10_000 });
    await expect(cardRow.locator('.cmdk-kind')).toHaveText('Card');
    await expect(page.locator('#cmdk-foot')).toContainText('searched');
    await cardRow.click();
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/, { timeout: 30_000 });
    await expect(page.locator('#board-detail-overlay')).toContainText(cardId);
    await page.keyboard.press('Escape');

    // Tab: typed name, arrow keys select it.
    await page.evaluate(() => { const o = document.getElementById('board-detail-overlay'); if (o?.classList.contains('active')) (window as any).closeBoardDetail?.(); });
    input = await palette(page, 'scheduler');
    const tabRow = page.locator('#cmdk-list .cmdk-row', { has: page.locator('.cmdk-kind', { hasText: 'Tab' }) }).first();
    await expect(tabRow).toContainText(/Scheduler/i);
    const idx = Number(await tabRow.getAttribute('data-i'));
    for (let i = 0; i < idx; i++) await input.press('ArrowDown');
    await input.press('Enter');
    await expect.poll(() => page.evaluate(() => (0, eval)('activeView'))).toBe('scheduler');

    // Group: filters the Workers list to it.
    input = await palette(page, group);
    const grpRow = page.locator('#cmdk-list .cmdk-row', { has: page.locator('.cmdk-kind', { hasText: 'Group' }) }).first();
    await expect(grpRow).toContainText(group, { timeout: 10_000 });
    await grpRow.click();
    await expect.poll(() => page.evaluate(() => [(0, eval)('activeView'), (0, eval)('activeTag')])).toEqual(['sessions', group]);

    // Escape closes, and Cmd-K again toggles it shut.
    await palette(page, 'x');
    await page.keyboard.press('Escape');
    await expect(page.locator('#cmdk-overlay')).not.toHaveClass(/active/);
  } finally {
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(worker)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': ui }, data: {} }));
    await cleanup('delete fixture card', () => request.delete(`/api/board/${cardId}`, { headers: auth }));
  }
});

// AF-1042: the server answers from its previous index while it rebuilds, and
// says so (`stale`). A card made a moment ago is then missing from that answer,
// and the nightly desktop leg failed on exactly this with no further keystroke
// to ask again. The first answer here is made stale and empty; the card must
// still appear without typing.
test('a stale palette answer is asked again, so a just-made card appears without another keystroke', async ({ page, request }, info) => {
  test.skip(info.project.name !== 'desktop', 'keyboard flow; one project is enough for the client retry');
  const { auth } = await boot(page);
  const stamp = Date.now();
  const card = await request.post('/api/board', { headers: auth, data: { title: `Wombat rollout note ${stamp}`, status: 'todo' } });
  expect(card.ok()).toBeTruthy();
  const cardId = (await card.json()).id;
  let calls = 0;
  await page.route('**/api/palette?*', async (route) => {
    calls += 1;
    const res = await route.fetch();
    const body = await res.json();
    if (calls === 1) { body.hits = []; body.total = 0; body.stale = true; }
    await route.fulfill({ response: res, json: body });
  });
  try {
    await palette(page, `wombat ${stamp}`);
    await expect(page.locator('#cmdk-list .cmdk-row', { hasText: `Wombat rollout note ${stamp}` })).toBeVisible({ timeout: 10_000 });
    expect(calls, 'the stale answer was followed by another request').toBeGreaterThan(1);
    await page.keyboard.press('Escape');
  } finally {
    await cleanup('remove the palette stub', () => page.unroute('**/api/palette?*'));
    await cleanup('delete fixture card', () => request.delete(`/api/board/${cardId}`, { headers: auth }));
  }
});

// On a busy board every answer reads stale: the fingerprint moves during each
// 1-2 s rebuild (live 2026-10-31, a 70 ms old index read stale). The client
// stops once an answer comes from an index built after the first stale one,
// rather than asking six more times on every search.
test('the stale retry stops once a newer index answers, even if that answer still reads stale', async ({ page }, info) => {
  test.skip(info.project.name !== 'desktop', 'keyboard flow; one project is enough for the client retry');
  await boot(page);
  let calls = 0;
  await page.route('**/api/palette?*', async (route) => {
    calls += 1;
    const res = await route.fetch();
    const body = await res.json();
    body.stale = true;
    body.index_age_ms = calls === 1 ? 60_000 : 0;
    await route.fulfill({ response: res, json: body });
  });
  try {
    await palette(page, `busyboard${Date.now()}`);
    await expect.poll(() => calls, { timeout: 5_000 }).toBe(2);
    await page.waitForTimeout(2_000);
    expect(calls, 'no retry after an answer from a rebuilt index').toBe(2);
    await page.keyboard.press('Escape');
  } finally {
    await cleanup('remove the palette stub', () => page.unroute('**/api/palette?*'));
  }
});

// The header magnifying glass is hidden (96e5b8e36); on a phone search opens
// from the + menu.
test('on a phone the + menu opens the palette and it fits the screen', async ({ page }, info) => {
  test.skip(info.project.name === 'desktop', 'phone layouts');
  await boot(page);
  await page.locator('#add-btn').click();
  await page.locator('#add-menu-search').click();
  await expect(page.locator('#cmdk-overlay')).toHaveClass(/active/);
  await expect(page.locator('#cmdk-input')).toBeFocused();
  await page.locator('#cmdk-input').fill('board');
  await expect(page.locator('#cmdk-list .cmdk-row').first()).toBeVisible();
  const box = await page.locator('#cmdk-overlay .modal').boundingBox();
  const vw = await page.evaluate(() => innerWidth);
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(vw + 1);
});
