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
