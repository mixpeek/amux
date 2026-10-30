// A window that falls back to polling gets its live stream back. Three failed
// reconnects (one deploy restart can cause them) used to leave a window on the
// 5 s poll for good: nothing retried the stream or stopped the timer, and the
// Mac app's windows fetched the whole board about 2,360 times an hour (hog RCA
// 20261030-044026). Now the poll retries the stream and its first message ends
// the poll.
import { test, expect } from './fixtures';

test('a window on the polling fallback returns to the live stream and stops polling', async ({ page }) => {
  test.setTimeout(90_000);
  const recovered: string[] = [];
  page.on('console', m => { if (m.text().includes('sse_recovered_from_polling')) recovered.push(m.text()); });
  await page.goto('/');
  await page.waitForFunction(() => (0, eval)('typeof _liveSSE !== "undefined" && _liveSSE'), null, { timeout: 30_000 });
  // The state three failed reconnects leave behind.
  await page.evaluate(() => (0, eval)('_liveSSE = false; enablePollingFallback();'));
  expect(await page.evaluate(() => (0, eval)('_pollTimer != null && _sseFallback'))).toBe(true);
  // Within one retry period plus a poll tick, the stream is back and the poll is gone.
  await page.waitForFunction(() => (0, eval)('_liveSSE && _pollTimer == null && !_sseFallback'), null, { timeout: 50_000 });
  expect(recovered.length, 'the recovery is logged').toBeGreaterThan(0);
  // And it stays gone. The reconnect itself refetches once (its catch-up, and a
  // poll tick that fired just before the stream came back), so count from a
  // second later: a 5 s poll would show twice in the next 12 s.
  await page.waitForTimeout(1000);
  const polls: number[] = [];
  const t0 = Date.now();
  page.on('request', r => { if (r.url().includes('/api/board?archived=0&slim=1&quota=1')) polls.push(Date.now() - t0); });
  await page.waitForTimeout(12_000);
  expect(polls.length, 'no 5 s board poll after the stream came back (ms: ' + polls.join(', ') + ')').toBe(0);
});
