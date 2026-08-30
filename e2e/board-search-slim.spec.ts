import { test, expect } from '@playwright/test';

// AMUX-2840 step 2. Free-text board search reads desc and log, which slim=1
// does not ship. Rather than switching engines to /api/search — measured to
// return materially different results in both directions, and paginated — the
// matcher stays authoritative and the client pays for desc/log only while a
// text query is active.

test('the full-text hydrator is INERT while the payload already carries desc', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const r = await page.evaluate(async () => {
    const w = window as any;
    let fetches = 0;
    const orig = w.fetch;
    // Count the HYDRATOR's fetch shape only (full=1 — the prose escape it
    // asks for), which is the claim in this test's name. The counter used to
    // count EVERY fetch, which worked while nothing else fetched in the
    // window — but since AMUX-3503 replaced SSE full-pushes with
    // invalidate-driven conditional fetches, a PARALLEL spec's board write
    // makes this page legitimately fetch its slim poll mid-window, and an
    // unscoped counter reads that as the hydrator firing.
    w.fetch = (...a: any[]) => {
      const u = String(a[0]);
      if (u.includes('/api/board') && u.includes('full=1')) fetches++;
      return orig(...a);
    };
    // Today's shape: items carry desc. Must not fetch.
    w._bqEnsureFullText([{ id: 'A-1', desc: 'present', log: '' }]);
    await new Promise(res => setTimeout(res, 200));
    w.fetch = orig;
    return fetches;
  });
  expect(r, 'a full payload must not trigger a hydration fetch').toBe(0);
});

test('it hydrates once when the payload is slim, and not again inside the cache window', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const r = await page.evaluate(async () => {
    const w = window as any;
    let fetches = 0;
    const orig = w.fetch;
    w.fetch = (...a: any[]) => {
      // The hydrator's own shape only (full=1) — the slim poll an
      // AMUX-3503 invalidate triggers also matches archived=0 and would
      // count as a phantom second hydration.
      const u = String(a[0]);
      if (u.includes('/api/board') && u.includes('full=1')) fetches++;
      return orig(...a);
    };
    // Slim shape: desc absent. `desc_len` present, as slim actually serves.
    const slim = [{ id: 'A-1', desc_len: 5 }];
    w._bqEnsureFullText(slim);
    await new Promise(res => setTimeout(res, 800));
    const afterFirst = fetches;
    // Second call inside the 60s window must not refetch.
    w._bqEnsureFullText(slim);
    await new Promise(res => setTimeout(res, 300));
    w.fetch = orig;
    return { afterFirst, afterSecond: fetches };
  });
  expect(r.afterFirst, 'a slim payload must trigger exactly one hydration fetch').toBe(1);
  expect(r.afterSecond, 'the 60s cache must suppress the second call').toBe(1);
});
