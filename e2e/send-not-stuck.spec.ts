// The Send button sat greyed out (owner, 2026-10-29). A write waited for its
// receipt with no bound, so a stall in the receipt's local bookkeeping held
// the send and its button forever. Now a write returns within 4 s of its
// response, and a send that never settles gives its button back after 20 s
// with the text kept. Both are beaconed for the ui.controls_do_not_stick sweep.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const W = 'send-stuck-w';
async function open(page: Page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: '/tmp' }] }));
  await page.route(/\/api\/sessions\/send-stuck-w\/peek\?/, r => r.fulfill({ json: { name: W, live: 'ready', history: '' } }));
  await page.route(/\/api\/sessions\/send-stuck-w\/subagents$/, r => r.fulfill({ json: { session: W, subagents: [] } }));
  allowUnusedRoute(page, /\/api\/sessions\/send-stuck-w\/subagents$/);
  allowUnusedRoute(page, /\/api\/sessions\/send-stuck-w\/peek\?/);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(n => (window as any).openPeek(n), W);
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
}

test('a send whose receipt bookkeeping never finishes still completes', async ({ page }) => {
  await open(page);
  let sent = 0;
  await page.route(/\/api\/sessions\/send-stuck-w\/send/, r => { sent++; return r.fulfill({ json: { ok: true, delivered: true, msg_id: 'm1' } }); });
  // The stall: acknowledging a receipt never resolves.
  await page.evaluate(() => { (window as any).eval('_interactions').acknowledge = () => new Promise(() => {}); });
  await page.locator('#peek-cmd-input').fill('hello');
  const btn = page.locator('#peek-overlay .send-split-main');
  await btn.click();
  await expect.poll(() => sent).toBe(1);
  // Done within the 4 s bound, not held for ever: the button is back.
  await expect(btn).toBeEnabled({ timeout: 8000 });
  await expect(page.locator('#peek-cmd-input')).toHaveValue('');
});

test('a send the server never answers gives the button back, keeps the text, and says so', async ({ page }) => {
  test.setTimeout(60_000);
  await open(page);
  await page.route(/\/api\/sessions\/send-stuck-w\/(send|steer)/, () => { /* never answers */ });
  // Where the composer holds the message in the outbox at once, nothing
  // reaches this stub; the outcome below is the same either way.
  allowUnusedRoute(page, /\/api\/sessions\/send-stuck-w\/(send|steer)/);
  const beacons: any[] = [];
  await page.route('**/api/client-debug', r => { try { beacons.push(JSON.parse(r.request().postData() || '{}')); } catch {} return r.fulfill({ json: { ok: true } }); });
  await page.locator('#peek-cmd-input').fill('are you there');
  const btn = page.locator('#peek-overlay .send-split-main');
  await btn.click();
  // The composer may hold the message in the outbox at once, so the button
  // need not grey at all; what matters is that it is never held.
  await expect(btn).toBeEnabled({ timeout: 30_000 });
  // Never lost: back in the box, or held in the outbox to retry (Sync shows it).
  const where = await page.evaluate(() => ({
    text: (document.getElementById('peek-cmd-input') as HTMLTextAreaElement).value,
    pending: (window as any).__amuxState.connection().pending,
  }));
  expect(where.text === 'are you there' || where.pending > 0, JSON.stringify(where)).toBe(true);
  // A send held past the bound is said, for the ui.controls_do_not_stick sweep.
  if (where.text === 'are you there') await expect.poll(() => beacons.some(b => b.verdict === 'composer_send_stuck' && b.session === W)).toBe(true);
});

// The cause on the owner's Mac (2026-10-29): IndexedDB stalled 46-68 s
// (outbox_flight_stalled) and a message waits for its outbox write. Past 3 s
// it now goes to the server directly, with the msg_id a later replay shares.
test('a message is sent even when the local outbox write never finishes', async ({ page }) => {
  await open(page);
  const bodies: any[] = [];
  await page.route(/\/api\/sessions\/send-stuck-w\/(send|steer)/, r => {
    try { bodies.push(JSON.parse(r.request().postData() || '{}')); } catch {}
    return r.fulfill({ json: { ok: true, delivered: true, submitted: true, id: 'q1', msg_id: 'm1' } });
  });
  // The stall: the outbox write never resolves.
  await page.evaluate(() => { (window as any)._queueOp = () => new Promise(() => {}); });
  await page.locator('#peek-cmd-input').fill('through the stall');
  const btn = page.locator('#peek-overlay .send-split-main');
  await btn.click();
  await expect.poll(() => bodies.length, { timeout: 10_000 }).toBe(1);
  expect(bodies[0].text).toContain('through the stall');
  expect(typeof bodies[0].msg_id).toBe('string');
  await expect(btn).toBeEnabled({ timeout: 8000 });
});
