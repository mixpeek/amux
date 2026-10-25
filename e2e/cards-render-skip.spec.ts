// Performance pass, 2026-10-19: render() rebuilt every worker card on every
// worker-list change, and 7 of 8 measured rebuilds wrote byte-identical HTML
// (~40ms each on a throttled phone). An identical write is now skipped. The
// card list must still change when its content does, including coming back
// after an empty state that wrote something else in between.
import { test, expect } from './fixtures';

const row = (name: string, status: string) => ({ name, dir: '/tmp/', provider: 'claude', status, running: true,
  tags: ['e2e'], desc: '', pinned: false, isolated: false });

test('an unchanged worker list keeps its card nodes; a changed one repaints', async ({ page }) => {
  test.setTimeout(60_000);
  let list = [row('e2e-skip-a', 'idle'), row('e2e-skip-b', 'idle')];
  await page.route('**/api/sessions', route => route.request().method() === 'GET'
    ? route.fulfill({ json: list }) : route.fallback());
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).render === 'function');
  const card = page.locator('#cards .card[data-session="e2e-skip-a"]').first();
  await expect(card).toBeAttached({ timeout: 30_000 });

  const kept = await page.evaluate(() => {
    const w = window as any; const g: any = globalThis;
    const before = document.querySelector('#cards .card[data-session="e2e-skip-a"]');
    const stats = g.eval('_cardsRenderStats'); const s0 = stats.skipped;
    for (let i = 0; i < 5; i++) w.render();
    return { same: before === document.querySelector('#cards .card[data-session="e2e-skip-a"]'), skipped: stats.skipped - s0 };
  });
  expect(kept.same).toBe(true);
  expect(kept.skipped).toBe(5);

  // A real change repaints.
  list = [row('e2e-skip-a', 'waiting'), row('e2e-skip-b', 'idle')];
  await page.evaluate(() => (window as any).fetchSessions());
  await expect(card).toHaveAttribute('data-worker-status', /waiting/);

  // An empty state in between, then the same cards: they must come back.
  const back = await page.evaluate(() => {
    const g: any = globalThis; const w = window as any;
    const saved = g.eval('sessions');
    g.eval('sessions = []'); w.render();
    const empty = !document.querySelector('#cards .card[data-session="e2e-skip-a"]');
    return { empty, saved };
  });
  expect(back.empty).toBe(true);
  await page.evaluate(s => { (globalThis as any).eval('(s) => { sessions = s; }')(s); (window as any).render(); }, back.saved);
  await expect(card).toBeAttached();
  await expect(card).toHaveAttribute('data-worker-status', /waiting/);
});
