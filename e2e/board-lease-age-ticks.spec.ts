// A card's heartbeat age keeps counting on an open board, and a Working strip
// over a worker that stopped clears without a reload (VID-147).
import { test, expect } from './fixtures';

test('the lease age moves on its own, and the live tick re-reads the workers', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._boardLiveTick === 'function');
  const ages = await page.evaluate(() => {
    const g = window as any;
    const now = Math.floor(Date.now() / 1000);
    const host = document.createElement('div');
    host.innerHTML = g._leaseChip({ lease: { holder: 'rewrite', heartbeat_at: now, expires_at: now + 1800 } });
    document.body.appendChild(host);
    const el = host.querySelector('.lease-beat') as HTMLElement;
    const first = el.textContent;
    el.dataset.at = String(now - 600);
    g._boardLiveTick();
    return [first, el.textContent];
  });
  expect(ages).toEqual(['just now', '10m ago']);

  await page.evaluate(() => (window as any).switchView('board'));
  await page.waitForFunction(() => (window as any).eval('activeView') === 'board');
  const reads = await page.evaluate(() => {
    const g = window as any;
    g.eval("sessions = [{ name: 'w', running: true, status: 'active', lifecycle: 'active', runtime_board: { measured: true, status: 'unlinked' } }];");
    let n = 0;
    const real = g.fetchSessions;
    g.fetchSessions = () => { n++; return Promise.resolve(); };
    g._boardLiveTick();
    g.eval("sessions = [];");
    g._boardLiveTick();
    g.fetchSessions = real;
    return n;
  });
  expect(reads, 'one read while the strip shows, none without it').toBe(1);
});
