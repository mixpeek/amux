// Performance pass, 2026-10-19: the board's change feed patched each changed
// card with its own GET /api/board/<id>, one after another, so a burst of N
// changes cost N round trips before the board repainted. It now reads all of
// them in one list request (?ids=), and an archived card drops out.
import { test, expect } from './fixtures';

test('a burst of card changes reaches the board in one request', async ({ page, request }) => {
  test.setTimeout(90_000);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  const ids: string[] = [];
  for (let i = 0; i < 6; i++) {
    const r = await request.post('/api/board', { headers, data: { title: `cdc batch ${i}`, status: 'todo' } });
    expect(r.ok()).toBeTruthy();
    ids.push((await r.json()).id);
  }
  await page.evaluate(() => (window as any).switchView('board'));
  for (const id of ids) await expect(page.locator(`.board-card[data-id="${id}"]`).first()).toBeAttached({ timeout: 30_000 });
  // The change feed needs its cursor. A fresh server's cursor trails the
  // change log by one 200ms tail, so re-read until the board has seeded it.
  await expect.poll(() => page.evaluate(async () => {
    const g = globalThis as any;
    if (!(g.eval('_cdcSeq') > 0)) await g.fetchBoard();
    return g.eval('_cdcSeq');
  }), { timeout: 20_000 }).toBeGreaterThan(0);
  // Let the initial reads settle, then count only what the burst causes.
  await page.waitForTimeout(1500);
  const perCard: string[] = [], batched: string[] = [], full: string[] = [];
  page.on('request', r => {
    const u = new URL(r.url());
    if (r.method() !== 'GET') return;
    if (/^\/api\/board\/[A-Z]+-\d+$/.test(u.pathname)) perCard.push(u.pathname);
    else if (u.pathname === '/api/board' && u.searchParams.has('ids')) batched.push(u.search);
    else if (u.pathname === '/api/board') full.push(u.search);
  });
  for (let i = 0; i < 5; i++) {
    const r = await request.patch(`/api/board/${ids[i]}`, { headers, data: { title: `cdc batch ${i} renamed` } });
    expect(r.ok()).toBeTruthy();
  }
  const arch = await request.patch(`/api/board/${ids[5]}`, { headers, data: { archived: 1,
    archive_outcome: 'Retiring this e2e fixture; no production work is hidden', authorized_by: 'e2e fixture owner' } });
  expect(arch.ok()).toBeTruthy();

  for (let i = 0; i < 5; i++) await expect(page.locator(`.board-card[data-id="${ids[i]}"]`).first()).toContainText('renamed', { timeout: 30_000 });
  await expect(page.locator(`.board-card[data-id="${ids[5]}"]`)).toHaveCount(0, { timeout: 30_000 });
  expect(perCard).toEqual([]);
  expect(batched.length).toBeGreaterThanOrEqual(1);
  expect(batched.length).toBeLessThanOrEqual(3);
  expect(full).toEqual([]);
});
