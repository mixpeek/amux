// Rebuildable IndexedDB caches are coalesced (Ethan, 2026-10-23: the Mac app
// was "so much slower than browser"). Every tab wrote the whole sessions list
// on each change, and in WebKit those writes queued ahead of the outbox's read
// of pending uploads. A cache key is written at most once per 30s, its latest
// value is what get() returns meanwhile, and pending work is never delayed.
import { test, expect } from './fixtures';

test('cache keys coalesce, pending work does not, and reads see the latest value', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => (globalThis as any).eval('typeof _idb') === 'object');
  const r = await page.evaluate(async () => {
    const g = globalThis as any, idb = g.eval('_idb'), st = g._lsWrites;
    const before = { w: st.idb_written, c: st.idb_coalesced };
    for (let i = 1; i <= 5; i++) await idb.set('sessions_cache', [{ name: 'v' + i }]);
    const latest = await idb.get('sessions_cache');
    await idb.set('zz_not_a_cache', { n: 1 });
    const strict = await idb.get('zz_not_a_cache');
    await new Promise(r => setTimeout(r, 50));
    return { latest, strict, written: st.idb_written - before.w, coalesced: st.idb_coalesced - before.c };
  });
  expect(r.latest).toEqual([{ name: 'v5' }]);
  expect(r.strict).toEqual({ n: 1 });
  // The app may have written sessions_cache itself just before; at most one
  // write of the five lands now, and the rest are counted as coalesced.
  expect(r.written).toBeLessThanOrEqual(1);
  expect(r.coalesced).toBeGreaterThanOrEqual(4);
});
