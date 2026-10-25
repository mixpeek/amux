// A hidden tab never starts an automatic offline save (2026-10-23). The copies
// live in IndexedDB, shared by every tab, so the shown tab's pass serves them
// all; ten Mac app tabs each fetched every running worker's scrollback. The
// cross-window 10-minute limit is covered by offline-prefetch-throttle.spec.ts.
import { test, expect } from './fixtures';

test('a hidden tab skips the automatic offline save, a manual one still runs', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    Object.defineProperty(Document.prototype, 'hidden', { get: () => true, configurable: true });
    Object.defineProperty(Document.prototype, 'visibilityState', { get: () => 'hidden', configurable: true });
  });
  // The worker list itself serves zz-pf: a sessions refresh landing between the
  // setup below and the manual save used to replace it with the real (empty)
  // list, so the save found nothing to fetch (main CI run 40108120656).
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: 'zz-pf', dir: '/tmp', running: true, status: 'active', flags: '' }] }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._offlinePrefetch === 'function');
  const peeks: string[] = [];
  await page.route(/\/api\/sessions\/[^/]+\/peek\?lines=200/, r => { peeks.push(r.request().url()); return r.fulfill({ json: { output: 'x', history: 'x' } }); });
  const g = (code: string) => page.evaluate(c => (globalThis as any).eval(c), code);
  await g(`localStorage.removeItem(_PREFETCH_LAST_KEY); _prefetchRunning = false; sessions = [{name:'zz-pf', running:true, status:'active'}];`);
  await g(`_offlinePrefetch(false, 'interval')`);
  expect(peeks.length).toBe(0);
  await page.evaluate(() => (window as any).fetchSessions());   // a refresh lands first, as it can in CI
  await g(`_offlinePrefetch(true)`);
  await expect.poll(() => peeks.length).toBe(1);
});
