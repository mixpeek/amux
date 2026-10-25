// A message queued in one window must still go when ANOTHER window of the same
// app holds the outbox locks and never lets go. The Mac app's windows share one
// data store and so one set of web locks, and WebKit suspends a hidden window's
// script mid-delivery; the visible window's message then read "Awaiting server"
// for hours (Ethan, 2026-10-23: "specific to mac app - i try in browser and it
// works"). Two tabs in one context share locks the same way. The steer route is
// a fake: nothing reaches a real worker.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const W = 'zz-outbox-stalled';
const ID = 'stalled-op-1';

async function boot(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mutateQueue === 'function' && typeof (window as any).runSyncBanner === 'function');
}

test('a visible window delivers past a window that froze holding the outbox locks', async ({ context }) => {
  test.setTimeout(150_000);
  const posts: string[] = [];
  const beacons: string[] = [];
  const STEER = `**/api/sessions/${W}/steer`;
  // Both windows get the stub and the beacon watch: the emulated "frozen" window
  // still runs script, so after its replay lock is taken either window may be
  // the one that delivers, and an unstubbed send would reach the real server.
  const open = async () => {
    const p = await context.newPage();
    await p.route(STEER, async r => {
      if (r.request().method() !== 'POST') return r.fulfill({ json: [] });
      posts.push(JSON.parse(r.request().postData() || '{}').msg_id);
      await r.fulfill({ json: { ok: true, submitted: true, id: 'steer-' + posts.length } });
    });
    p.on('request', r => { const b = r.postData() || ''; if (/outbox_(delivery_lock_stolen|replay_lock_stalled)/.test(b)) beacons.push(b); });
    await boot(p);
    return p;
  };
  const frozen = await open();
  allowUnusedRoute(frozen, STEER);   // the visible window usually delivers; the frozen one only may
  // The suspended window: it holds the replay lock and this message's delivery lock forever.
  await frozen.evaluate((id) => {
    navigator.locks.request('amux-outbox-replay', () => new Promise(() => {}));
    navigator.locks.request('amux-outbox-delivery:' + id, () => new Promise(() => {}));
  }, ID);
  await expect.poll(() => frozen.evaluate(async () => ((await navigator.locks.query()).held || [])
    .filter((l: any) => /^amux-outbox-(replay|delivery:)/.test(l.name)).length)).toBe(2);

  const page = await open();
  allowUnusedRoute(page, STEER);     // whichever window delivers, the count below is the assertion
  await page.evaluate(async ({ w, id }) => {
    const g = window as any;
    await g._mutateQueue((rows: any[]) => rows.push({ id, url: '/api/sessions/' + w + '/steer',
      options: { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text: 'past the frozen window', msg_id: 'm-stalled' }) },
      timestamp: Date.now(), state: 'pending', not_attempted: true }));
    g.eval('online = true');
    g.runSyncBanner(true);
  }, { w: W, id: ID });
  // 20s for the replay lock, then 30s for the delivery lock, then the send.
  await expect.poll(() => posts.filter(m => m === 'm-stalled').length, { timeout: 90_000, message: 'the message is sent despite the frozen holder' }).toBe(1);
  await expect.poll(() => page.evaluate((id) => (window as any).eval('offlineQueue').some((q: any) => q.id === id), ID)).toBe(false);
  await expect.poll(() => beacons.some(b => b.includes('outbox_delivery_lock_stolen')), { message: 'the takeover is reported' }).toBe(true);
  expect(beacons.some(b => b.includes('"action":"stole"')), 'the replay lock is taken over, not run beside').toBe(true);
});
