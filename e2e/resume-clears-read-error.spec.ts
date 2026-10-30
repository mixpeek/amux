// A read that fails while the device is offline sets "Sync error" on the
// connection pill, and only a later read that succeeds clears it. Coming back
// online within a few seconds of the last data skipped the resync, so the pill
// kept saying "Sync error" over a sync that was running (LC-SYNC-PROGRESS on
// ios-safari, run 41010141601). Reconnecting now always re-reads when a read
// had failed.
import { test, expect } from './fixtures';

test('coming back online re-reads at once when a read had failed, and the pill leaves Sync error', async ({ page }) => {
  const resyncs: string[] = [];
  page.on('console', m => { if (m.text().includes('resume_resync_read_error')) resyncs.push(m.text()); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).updateConnectionStatus === 'function');
  // Wait out the boot read: a reconnect read that joins it can be discarded
  // when a newer snapshot lands, which is not what this test is about.
  await expect(page.locator('#conn-status')).toHaveText(/^(Live|Polling)$/, { timeout: 15000 });
  // The state an offline read leaves behind, with data that arrived a moment
  // ago, then the reconnect: ONE synchronous step, taken while no read is in
  // flight. A background read that lands between steps clears the error on its
  // own, and on CI's slower WebKit it did, before the pill was even checked
  // (nightly run 41018265890: "Live" for the whole 5s).
  const pill = await page.waitForFunction(() => {
    if ((0, eval)('_sessionFetchInFlight')) return false;
    (0, eval)("_sessionLoadError = {status: 0, reason: 'network_error'}; _lastDataTime = Date.now();");
    (window as any).updateConnectionStatus();
    const text = document.getElementById('conn-status')!.textContent;
    // A hidden tab, as on CI's WebKit, where the resume path (which does
    // nothing while hidden) never re-read: no resync line (run 41026111631).
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    window.dispatchEvent(new Event('online'));
    return text;
  }, null, { timeout: 15000 });
  expect(await pill.jsonValue(), 'the failed read shows Sync error').toBe('Sync error');
  // The resync line is what separates the reconnect's own re-read from a
  // background poll that happens to land (without the fix the pill can still
  // clear on its own). Only the fix writes it, so the wait can be generous: a
  // 1.5 s window failed on a loaded CI WebKit (run 41026111631).
  await expect.poll(() => resyncs.length, { message: 'reconnecting resyncs because a read had failed', timeout: 10000 }).toBeGreaterThan(0);
  await expect(page.locator('#conn-status'), 'the re-read clears Sync error').not.toHaveText('Sync error', { timeout: 5000 });
});
