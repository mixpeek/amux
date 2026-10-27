// The localStorage write guard (disk RCA 2026-10-14, 2026-10-15). WebKit
// appends every localStorage write to a SQLite WAL it does not shrink while the
// app runs; the macOS app's reached 13.7G, then 11G again in 26h after the
// first guard, because it throttled only `*_cache` keys and amux_cmd_history
// (1.1MB) was rewritten whole on every change. Identical values are not
// rewritten; a cache key or any value over 16KB is written at most once per
// 5 min, read back from the pending copy meanwhile, and flushed when the page
// hides. The beacon names the top keys by bytes.
import { test, expect } from './fixtures';

test('cache keys and large values are throttled, reads see the latest, hiding flushes it', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => !!(window as any)._lsWrites);
  const r = await page.evaluate(() => {
    const st = (window as any)._lsWrites;
    const before = { ...st };
    localStorage.setItem('zz_probe_cache', 'v1');          // first write: immediate
    localStorage.setItem('zz_probe_cache', 'v2');          // within 5 min: deferred
    localStorage.setItem('zz_probe_cache', 'v3');          // still deferred, latest kept
    const big = (c: string) => c.repeat(20000);            // over 16KB, a name the old guard ignored
    localStorage.setItem('zz_probe_history', big('a'));
    localStorage.setItem('zz_probe_history', big('b'));    // deferred by size
    localStorage.setItem('zz_probe_small', 'x1');
    localStorage.setItem('zz_probe_small', 'x2');          // small, uncached: written at once
    localStorage.setItem('zz_probe_plain', 'same');
    localStorage.setItem('zz_probe_plain', 'same');        // identical: skipped
    return {
      readCache: localStorage.getItem('zz_probe_cache'),
      readBig: (localStorage.getItem('zz_probe_history') || '')[0],
      small: localStorage.getItem('zz_probe_small'),
      deferred: st.deferred - before.deferred,
      skipped: st.skipped_identical - before.skipped_identical,
      topHasHistory: 'zz_probe_history' in st.by_key,
    };
  });
  expect(r.readCache, 'a read sees the deferred value, not the stored one').toBe('v3');
  expect(r.readBig, 'a value over 16KB is throttled whatever its key').toBe('b');
  expect(r.small).toBe('x2');
  expect(r.deferred).toBe(2);
  expect(r.skipped).toBeGreaterThanOrEqual(1);
  expect(r.topHasHistory, 'bytes are counted per key for the beacon').toBe(true);
  // What actually reached storage, read from a second page (its own guard has
  // nothing pending): still the first value until the first page hides.
  const other = await page.context().newPage();
  await other.goto('/');
  const stored = (k: string) => other.evaluate((k) => (localStorage.getItem(k) || '').slice(0, 2), k);
  expect(await stored('zz_probe_cache')).toBe('v1');
  expect(await stored('zz_probe_history')).toBe('aa');
  await page.evaluate(() => window.dispatchEvent(new Event('pagehide')));
  // The flush is synchronous in the first page, but a second page sees
  // another page's localStorage writes asynchronously. One immediate read
  // raced that and read the old 'v1' (CI run 40551723369, mobile and desktop).
  await expect.poll(() => stored('zz_probe_cache'), { timeout: 5000 }).toBe('v3');
  await expect.poll(() => stored('zz_probe_history'), { timeout: 5000 }).toBe('bb');
});
