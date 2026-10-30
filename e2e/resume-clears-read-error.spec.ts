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
  await page.waitForFunction(() => !(0, eval)('_sessionFetchInFlight'));
  // The state an offline read leaves behind, with data that arrived a moment ago.
  await page.evaluate(() => {
    (0, eval)("_sessionLoadError = {status: 0, reason: 'network_error'}; _lastDataTime = Date.now();");
    (window as any).updateConnectionStatus();
  });
  await expect(page.locator('#conn-status')).toHaveText('Sync error');
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  // The reconnect itself re-reads, inside a window shorter than any background
  // poll: the resync line is what separates this from a poll that happened to
  // land (without the fix the pill can still clear on its own a few seconds later).
  await expect.poll(() => resyncs.length, { message: 'reconnecting resyncs because a read had failed', timeout: 1500 }).toBeGreaterThan(0);
  await expect(page.locator('#conn-status'), 'the re-read clears Sync error').not.toHaveText('Sync error', { timeout: 5000 });
});
