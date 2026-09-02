import { test, expect } from '@playwright/test';

// `_sseFallback` was a ONE-WAY LATCH. enablePollingFallback() set it; the only
// thing that cleared it was setOnline()'s false→true edge, which requires the
// client to lose the server ENTIRELY first.
//
// The commonest way to lose SSE does not lose the server. The auto-builder
// swaps the binary on every commit and every stream dies with it (see
// /api/debug/sse's own note), while plain HTTP answers again seconds later.
// Three failed retries latch the fallback, the 5s poll keeps succeeding,
// `online` never goes false — so nothing ever clears the latch, and for the
// life of the page that tab has no SSE, no staleness watchdog and no
// resume-reconnect (both guarded on !_sseFallback). On a phone that page lives
// for days, which is how "amux is offline on my phone" recurs against a server
// that was up the whole time.
//
// Every case carries its control, because the retry is only correct against the
// one it must NOT make: retrying while the SERVER is gone is noise, and the
// setOnline edge already covers that case.

test('a client stuck on polling can get back onto SSE', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');

  const r = await page.evaluate(() => {
    const w = window as any;
    const now = 1_000_000_000;
    const s = (o: any) => w._shouldRetrySse(o, now);
    const MIN = 60_000;
    const base = { fallback: true, hidden: false, online: true, lastTry: now - 10 * MIN, resume: false };

    return {
      // THE BUG: latched, server reachable, never tried. This must be retryable.
      stuckAndReachable: s(base),
      // CONTROL: not in fallback. connectSSE()/the watchdog own that path; a
      // retry here would double-connect.
      notInFallback: s({ ...base, fallback: false }),
      // CONTROL: server unreachable. Polling is failing too, so this is an
      // outage, not an SSE-only fault — setOnline()'s edge already reconnects
      // when the server returns, and retrying the stream meanwhile is noise.
      serverGone: s({ ...base, online: false }),
      // CONTROL: hidden tab cannot verify the result; resume retries instead.
      hidden: s({ ...base, hidden: true }),
      // Pacing: a background re-attempt 10s after the last one must not fire.
      tooSoonBackground: s({ ...base, lastTry: now - 10_000 }),
      // ...but a RESUME is a strong signal and uses the shorter floor.
      resumeAfter10s: s({ ...base, lastTry: now - 10_000, resume: true }),
      // One iOS resume fires visibility + pageshow + focus. Only the first may
      // connect, or a single wake opens three EventSources.
      resumeStorm: s({ ...base, lastTry: now - 1_000, resume: true }),
      // A client that has never re-attempted (lastTry 0) is the freshly-latched
      // one, and is exactly who this exists for.
      neverTried: s({ ...base, lastTry: 0 }),
    };
  });

  expect(r.stuckAndReachable, 'latched on polling against a reachable server is the whole bug — it must retry').toBe(true);
  expect(r.notInFallback, 'not in fallback: connectSSE owns this, retrying would double-connect').toBe(false);
  expect(r.serverGone, 'server unreachable is an outage, not an SSE fault; setOnline covers it').toBe(false);
  expect(r.hidden, 'a hidden tab cannot verify a reconnect').toBe(false);
  expect(r.tooSoonBackground, 'background re-attempts must stay paced').toBe(false);
  expect(r.resumeAfter10s, 'a user-visible resume must not wait out the background cadence').toBe(true);
  expect(r.resumeStorm, 'one wake fires three events; only the first may connect').toBe(false);
  expect(r.neverTried, 'a freshly-latched client is who this is for').toBe(true);
});

test('the resume refetch does not need prior data to fire', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');

  // AC-275 fixed _sseLooksStale() to fall back to page load, because a client
  // that never received a first datum could not declare itself stale — the one
  // state where the refetch is the only thing that helps. _onClientResume's
  // refetch guard had the identical `_lastDataTime &&` shape and kept the hole:
  // a PWA cold-started while the server was down has a null _lastDataTime, so
  // every resume refetched nothing, forever. That is the phone that opens to a
  // dead dashboard and stays dead until it is force-quit.
  const r = await page.evaluate(() => {
    const w = window as any;
    const now = 1_000_000_000;
    const f = (ld: number | null, pl: number) => w._resumeNeedsRefetch(ld, pl, now);
    return {
      // THE BUG: never got a datum, page loaded a minute ago. Must refetch.
      neverHadData: f(null, now - 60_000),
      // CONTROL: never got a datum, but the page loaded 100ms ago — the initial
      // load is still in flight and a second resync would just double it.
      neverHadDataFreshLoad: f(null, now - 100),
      // Ordinary case: data, but stale. Must refetch.
      staleData: f(now - 60_000, now - 3_600_000),
      // CONTROL: data from 1s ago is fresh; resuming must not refetch on every
      // visibilitychange or a tabbing user hammers the server.
      freshData: f(now - 1_000, now - 3_600_000),
      // Data present but older than page load must still be judged on the DATA,
      // not silently rescued by a recent load.
      dataOutranksPageLoad: f(now - 3_600_000, now - 100),
    };
  });

  expect(r.neverHadData, 'a client that never received data is exactly who needs the refetch').toBe(true);
  expect(r.neverHadDataFreshLoad, 'the initial load is still in flight; do not double it').toBe(false);
  expect(r.staleData, 'stale data on resume must refetch').toBe(true);
  expect(r.freshData, 'fresh data must not refetch on every visibilitychange').toBe(false);
  expect(r.dataOutranksPageLoad, 'a recent page load must not excuse stale data').toBe(true);
});
