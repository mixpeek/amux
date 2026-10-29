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
  await boot(page);
  // Every engine: record the diagnostic where it is raised, not off the wire.
  await page.evaluate(() => { const g = window as any, real = g._outboxDiagnostic; g.__verdicts = [];
    g._outboxDiagnostic = (v: string, f: any) => { g.__verdicts.push(v); return real(v, f); }; });
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
  expect(await page.evaluate(() => (window as any).__verdicts), 'the takeover is reported').toContain('outbox_storage_lock_stolen');
  await page.evaluate(async () => { await (window as any)._mutateQueue((rows: any[]) => {
    const at = rows.findIndex((q: any) => q.id === 'storage-stalled-1'); if (at >= 0) rows.splice(at, 1); }); });
});
