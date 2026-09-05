import { test, expect } from '@playwright/test';

// AMUX-3917. Ethan's Connection panel showed "Disconnected (offline) 4h 16m,
// Aug 29 9:38 PM → 1:55 AM" as its largest incident. That was an iPhone asleep
// overnight. Six of the other ten rows were 0s or 1s polling fallbacks that had
// recovered on their own, and the header called all of it "Disconnections".
//
// The panel's job is to tell the user whether AMUX is reachable. A row it cannot
// attribute is worse than no row: it is a wrong answer that looks measured.
//
// Every case below carries its control, because each classification is only
// meaningful against the one it must NOT make. Turning every offline into
// "asleep" would empty the panel of real outages, which is the worse bug.

test('connection episodes are classified by what they actually were', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');

  const r = await page.evaluate(() => {
    const w = window as any;
    const now = 1_000_000_000;
    const k = (ep: any) => w._connEpisodeKind(ep, now);
    const HOUR = 3600_000;
    return {
      // Ethan's row: offline, began while the page was hidden.
      sleep: k({ start: now - 4 * HOUR, end: now - 10_000, worst: 'offline', hid: true }),
      // CONTROL: same shape, page VISIBLE. This is a real outage and must stay one.
      realOutage: k({ start: now - 4 * HOUR, end: now - 10_000, worst: 'offline', hid: false }),
      // The 0s/1s/4s rows: brief polling fallback that recovered itself.
      blip: k({ start: now - 1200, end: now - 200, worst: 'polling', hid: false }),
      // CONTROL: the same fallback lasting 15 minutes is NOT a blip. Ethan's
      // panel had one of these (1:57 AM → 2:13 AM) and it must survive folding.
      longPolling: k({ start: now - 15 * 60_000, end: now - 10_000, worst: 'polling', hid: false }),
      // CONTROL: an episode still happening cannot be folded as "recovered".
      ongoingShort: k({ start: now - 1200, worst: 'polling', hid: false }),
      // Stored before this build: no visibility flag. Must NOT be guessed into
      // 'sleep' — unknown attribution stays an outage and says so in the row.
      legacyOffline: k({ start: now - 4 * HOUR, end: now - 10_000, worst: 'offline', hid: null }),
      // A hidden page that only degraded to polling is not the sleep case.
      hiddenPolling: k({ start: now - 15 * 60_000, end: now - 10_000, worst: 'polling', hid: true }),
    };
  });

  expect(r.sleep, 'offline that began while hidden is the device sleeping, not an amux outage').toBe('sleep');
  expect(r.realOutage, 'offline while VISIBLE is a real outage and must never be excused as sleep').toBe('outage');
  expect(r.blip, 'a sub-5s polling fallback that recovered is not an incident').toBe('blip');
  expect(r.longPolling, 'a 15-minute polling degradation must stay visible').toBe('outage');
  expect(r.ongoingShort, 'an episode with no end has not recovered and cannot be folded').toBe('outage');
  expect(r.legacyOffline, 'no visibility flag means unknown; unknown must not be relabelled as sleep').toBe('outage');
  expect(r.hiddenPolling, 'hidden + polling is not the offline-while-asleep case').toBe('outage');
});

test('a recorded transition carries the flag that makes the above decidable', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');

  // Goes through the SHIPPED persistence path (localStorage), not an internal
  // binding: top-level `let` is not on `window` in a classic script, and a test
  // that reached for one would be testing a paraphrase of the code.
  const rec = await page.evaluate(() => {
    const w = window as any;
    const saved = localStorage.getItem('amux_conn_events');
    localStorage.removeItem('amux_conn_events');
    w._connState = 'live';
    w._recordConnState('offline');
    let out: any = null;
    try { const a = JSON.parse(localStorage.getItem('amux_conn_events') || '[]'); out = a[a.length - 1] || null; } catch (e) {}
    if (saved === null) localStorage.removeItem('amux_conn_events');
    else localStorage.setItem('amux_conn_events', saved);
    return out;
  });

  expect(rec, 'a live -> offline transition must be recorded').toBeTruthy();
  // The KEY must exist, not merely be falsy. A missing key is the legacy case
  // above, and it is the whole difference between "we looked and the page was
  // visible" and "we never looked".
  expect('hid' in rec, 'every new event must carry the visibility flag').toBe(true);
  expect(rec.hid, 'the page is visible under test, so hid records 0').toBe(0);
});
