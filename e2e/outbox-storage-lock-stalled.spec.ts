// A message is saved locally the moment Send is pressed, and the button stays
// disabled until it is. The save takes the outbox storage lock, which every
// window of the Mac app shares; WebKit suspends a hidden window, and one
// suspended after the lock was granted held it for good. Send then stayed
// greyed with the message still typed (Ethan, 2026-10-29). A visible window
// now takes that lock over after a short wait. The send route is a fake:
// nothing reaches a real worker.
import { test, expect, Page } from './fixtures';

const W = 'zz-storage-stalled';

async function boot(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mutateQueue === 'function');
}

test('a local save goes through past a window that froze holding the storage lock', async ({ context }) => {
  test.setTimeout(60_000);
  const frozen = await context.newPage();
  await boot(frozen);
  // The suspended window: granted the storage lock, never lets it go.
  await frozen.evaluate(() => { navigator.locks.request('amux-outbox-storage', () => new Promise(() => {})); });
  await expect.poll(() => frozen.evaluate(async () => ((await navigator.locks.query()).held || [])
    .some((l: any) => l.name === 'amux-outbox-storage'))).toBe(true);

  const page = await context.newPage();
  // Record every outbox verdict from the first request on. The page's own
  // boot sync (runSyncBanner -> _mutateQueue) is the first thing to queue
  // behind the frozen window, so the takeover usually happens DURING boot.
  // A recorder installed after boot missed it and saw only the replay-lock
  // verdict, which failed this spec on every engine on main (AF-999).
  const verdicts: string[] = [];
  await page.route(/\/api\/client-debug$/, async route => {
    try { const b = route.request().postDataJSON(); if (b && b.verdict) verdicts.push(b.verdict); } catch (_) {}
    await route.fulfill({ json: { ok: true } });
  });
  await boot(page);
  const t0 = Date.now();
  // The same call the composer's save makes (_queueOp -> _mutateQueue).
  const saved = await page.evaluate(async (w) => {
    const g = window as any;
    await g._mutateQueue((rows: any[]) => rows.push({ id: 'storage-stalled-1', url: '/api/sessions/' + w + '/send',
      options: { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text: 'past the frozen window', msg_id: 'm-storage' }) },
      timestamp: Date.now(), state: 'blocked', not_attempted: true }));
    return g.eval('offlineQueue').some((q: any) => q.id === 'storage-stalled-1');
  }, W);
  const waited = Date.now() - t0;
  expect(saved, 'the save landed in the queue').toBe(true);
  expect(waited, 'within seconds, not after the holder wakes').toBeLessThan(15_000);
  await expect.poll(() => verdicts, { message: 'the takeover is reported' }).toContain('outbox_storage_lock_stolen');
  await page.evaluate(async () => { await (window as any)._mutateQueue((rows: any[]) => {
    const at = rows.findIndex((q: any) => q.id === 'storage-stalled-1'); if (at >= 0) rows.splice(at, 1); }); });
});
