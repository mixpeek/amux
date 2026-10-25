// A sync pass that never settles must not freeze the outbox (Ethan, 2026-10-21,
// the Mac app: a message sat "Saved on this device · waiting to sync" for 9 to
// 67 minutes and went out only when he sent another). Each await a pass can
// hang on is made to hang here: the cross-window replay lock held by another
// window, the IndexedDB upload list never answering, and an arbitrary phase.
// The message must still be delivered, and the stall must be beaconed with
// its cause. The worker and its send route are fakes.
import { test, expect, Page } from './fixtures';

const W = 'zz-outbox-stall';

async function boot(page: Page, beacons: any[]) {
  await page.route('**/api/client-debug', r => { try { beacons.push(JSON.parse(r.request().postData() || '{}')); } catch (e) {} return r.fulfill({ json: { ok: true } }); });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._mutateQueue === 'function' && typeof (window as any).runSyncBanner === 'function');
}

async function seedAndSync(page: Page) {
  await page.evaluate(async (w) => {
    const g = window as any;
    await g._mutateQueue((rows: any[]) => {
      rows.push({ id: 'stall-1', url: '/api/sessions/' + w + '/send', options: { method: 'POST', body: JSON.stringify({ text: 'test it e2e', msg_id: 'm-stall' }) },
        timestamp: Date.now(), state: 'pending', not_attempted: true });
    });
    g.runSyncBanner(true);
  }, W);
}

function sendRoute(page: Page, posts: string[]) {
  return page.route(`**/api/sessions/${W}/send`, async r => {
    if (r.request().method() !== 'POST') return r.fulfill({ json: { ok: true } });
    posts.push(JSON.parse(r.request().postData() || '{}').msg_id);
    await r.fulfill({ json: { ok: true, submitted: true, id: 'msg-' + posts.length } });
  });
}

test('another window holding the replay lock does not hold the message, and a shown window takes it at once', async ({ page, context }) => {
  test.setTimeout(90_000);
  const posts: string[] = [], beacons: any[] = [];
  await sendRoute(page, posts);
  // A second window of the app that took the lock and never finishes, as a
  // suspended WebKit window does.
  // A same-origin document is enough to hold it; a full app here could deliver
  // the message itself and hide the stall.
  const other = await context.newPage();
  await other.goto('/health');
  await other.evaluate(() => { (window as any).__held = navigator.locks.request('amux-outbox-replay', () => new Promise(() => {})); });
  await boot(page, beacons);
  await page.evaluate(() => (window as any).eval('_syncFlight = null'));
  await seedAndSync(page);
  // A shown window steals the lock after a short wait instead of waiting out
  // the 20s stall bound (Ethan, 2026-10-23: the Mac app was slower than the
  // browser because a frozen background tab held this lock).
  const t0 = Date.now();
  await expect.poll(() => posts.includes('m-stall'), { timeout: 15_000 }).toBe(true);
  expect(Date.now() - t0).toBeLessThan(10_000);
  await expect.poll(() => beacons.some(b => b.verdict === 'outbox_replay_lock_stalled' && b.action === 'stole' && b.held === 1)).toBe(true);
  await other.close();
});

test('an IndexedDB upload list that never answers does not hold the message', async ({ page }) => {
  test.setTimeout(60_000);
  const posts: string[] = [], beacons: any[] = [];
  await sendRoute(page, posts);
  await boot(page, beacons);
  await page.evaluate(() => (window as any).eval('_upqList = () => new Promise(() => {})'));
  await seedAndSync(page);
  await expect.poll(() => posts.includes('m-stall'), { timeout: 20_000 }).toBe(true);
  expect(beacons.some(b => b.verdict === 'outbox_upload_list_timeout')).toBe(true);
});

test('a pass stuck in any phase is abandoned, named, and replaced', async ({ page }) => {
  test.setTimeout(120_000);
  const posts: string[] = [], beacons: any[] = [];
  await sendRoute(page, posts);
  await boot(page, beacons);
  // The first pass hangs forever in a phase nothing bounds; later passes work.
  await page.evaluate(() => (window as any).eval(
    "{ const orig = _runSyncBanner; let n = 0; _runSyncBanner = async q => { if (n++ === 0) { _syncSetPhase('test_hang'); return new Promise(() => {}); } return orig(q); }; }"));
  await seedAndSync(page);
  await expect.poll(() => beacons.find(b => b.verdict === 'outbox_flight_stalled')?.phase, { timeout: 70_000 }).toBe('test_hang');
  await expect.poll(() => posts.includes('m-stall'), { timeout: 30_000 }).toBe(true);
});
