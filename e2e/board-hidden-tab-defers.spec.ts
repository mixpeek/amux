// 2026-10-22: the Mac app runs a whole dashboard per tab, and every tab re-read
// the board and the worker list on every change, hidden or not, so ten tabs made
// ten reads of a board that takes seconds to serve. A hidden tab now waits and
// catches up once when it is shown.
import { test, expect } from './fixtures';

test('a hidden tab reads no board changes until it is shown, then catches up', async ({ page, request }) => {
  test.setTimeout(90_000);
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    const w = window as any;
    w.__hidden = false;
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => w.__hidden });
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (w.__hidden ? 'hidden' : 'visible') });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  const made = await request.post('/api/board', { headers, data: { title: 'hidden tab seed', status: 'todo' } });
  expect(made.ok()).toBeTruthy();
  const id = (await made.json()).id;
  await page.evaluate(() => (window as any).switchView('board'));
  await expect(page.locator(`.board-card[data-id="${id}"]`).first()).toBeAttached({ timeout: 30_000 });
  await expect.poll(() => page.evaluate(async () => {
    const g = globalThis as any;
    if (!(g.eval('_cdcSeq') > 0)) await g.fetchBoard();
    return g.eval('_cdcSeq');
  }), { timeout: 20_000 }).toBeGreaterThan(0);
  await page.waitForTimeout(1500);

  const reads: string[] = [];
  const beacons: any[] = [];
  page.on('request', r => {
    const u = new URL(r.url());
    if (r.method() === 'GET' && (u.pathname.startsWith('/api/board') || u.pathname === '/api/sessions')) reads.push(u.pathname + u.search);
    if (r.method() === 'POST' && u.pathname === '/api/client-debug') {
      try { const b = JSON.parse(r.postData() || '{}'); if (b.kind === 'hidden-tab-catchup') beacons.push(b); } catch {}
    }
  });
  await page.evaluate(() => { (window as any).__hidden = true; document.dispatchEvent(new Event('visibilitychange')); });
  for (let i = 0; i < 3; i++) {
    const r = await request.patch(`/api/board/${id}`, { headers, data: { title: `hidden tab renamed ${i}` } });
    expect(r.ok()).toBeTruthy();
  }
  await expect.poll(() => page.evaluate(() => (globalThis as any).eval('_boardChangesWhileHidden')), { timeout: 20_000 }).toBeGreaterThan(0);
  // A worker change while hidden waits too (the SSE handler's own scheduler).
  await page.evaluate(() => (globalThis as any).eval('_scheduleSessionInvalidation()'));
  await page.waitForTimeout(1500);
  expect(reads).toEqual([]);

  await page.evaluate(() => { (window as any).__hidden = false; document.dispatchEvent(new Event('visibilitychange')); });
  await expect(page.locator(`.board-card[data-id="${id}"]`).first()).toContainText('hidden tab renamed 2', { timeout: 30_000 });
  expect(reads.filter(u => u.startsWith('/api/board/changes')).length).toBeGreaterThanOrEqual(1);
  await expect.poll(() => reads.filter(u => u === '/api/sessions').length).toBeGreaterThanOrEqual(1);
  await expect.poll(() => beacons.length).toBe(1);
  expect(beacons[0].verdict).toBe('reads_deferred_while_hidden');
  expect(beacons[0].board_changes).toBeGreaterThan(0);
  expect(beacons[0].session_changes).toBe(1);
});
