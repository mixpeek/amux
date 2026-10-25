// golden_realtime_convergence, 2026-10-20: fetchBoard read the board and THEN
// seeded the change-feed cursor from the log head. A card created between the
// two reads was missing from the data and already behind the cursor, so no
// later change-feed update brought it back. The head is now read first.
import { test, expect } from './fixtures';

test('a card created while the board read is in flight still reaches the board', async ({ page, request }) => {
  test.setTimeout(60_000);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).fetchBoard === 'function');
  await page.evaluate(() => (window as any).switchView('board'));
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  // A fresh server's change log is empty, and an empty log seeds no cursor
  // (the board then re-reads in full). One write gives the log a head.
  expect((await request.post('/api/board', { headers, data: { title: `seed race warmup ${Date.now()}`, status: 'todo' } })).ok()).toBeTruthy();
  await expect.poll(() => page.evaluate(async () => {
    const g = globalThis as any;
    if (!(g.eval('_cdcSeq') > 0)) await g.fetchBoard();
    return g.eval('_cdcSeq');
  }), { timeout: 20_000 }).toBeGreaterThan(0);
  // No stream for the rest of the test, so only the two reads below can deliver the card.
  await page.evaluate(() => { const g = globalThis as any; g.eval('_sse && _sse.close()'); });

  let title = '';
  let held = false;
  await page.route(/\/api\/board\?archived=0&slim=1&quota=1/, async route => {
    if (held) return route.continue();
    held = true;
    // The server answers first; the card is created after its snapshot.
    const response = await route.fetch({ headers: { ...route.request().headers(), 'if-none-match': '' } });
    title = `seed race ${Date.now()}`;
    const r = await request.post('/api/board', { headers, data: { title, status: 'todo' } });
    expect(r.ok()).toBeTruthy();
    await route.fulfill({ response });
  });
  await page.evaluate(() => (window as any).fetchBoard());
  expect(held).toBe(true);
  expect(await page.evaluate(t => (globalThis as any).eval('boardItems').some((i: any) => i.title === t), title)).toBe(false);

  await page.evaluate(() => (globalThis as any).eval('_cdcBoardUpdate')());
  await expect.poll(() => page.evaluate(t => (globalThis as any).eval('boardItems').some((i: any) => i.title === t), title),
    { timeout: 10_000 }).toBe(true);
});
