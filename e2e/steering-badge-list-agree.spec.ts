// The Steering tab's badge and its list must say the same thing. Ethan's
// screenshot (2026-10-24): the badge read 1 while the list read "No queued
// messages". The message left the outbox (it was sent) while another tab was
// showing; nothing repainted the badge, and opening Steering drew the list
// fresh. A change from another window of the same app (the Mac app shares one
// store) repainted neither. The worker and its routes are fakes.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const W = 'zz-steer-agree';
const row = (id: string, w: string) => ({ id, url: '/api/sessions/' + w + '/steer',
  options: { method: 'POST', body: JSON.stringify({ text: 'queued ' + id, msg_id: 'm-' + id }) },
  timestamp: Date.now(), state: 'pending', not_attempted: true });

async function boot(page: Page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/steer-agree', running: true, status: 'active', flags: '', steering: [] }] }));
  const PEEK = `**/api/sessions/${W}/peek?*`;
  await page.route(PEEK, r => r.fulfill({ json: { name: W, output: 'working\n', history: '' } }));
  allowUnusedRoute(page, PEEK);   // a window that never opens the worker does not peek it
  await page.goto('/');
  await page.waitForFunction((w) => typeof (window as any).openPeek === 'function' && typeof (window as any)._mutateQueue === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  // Hold delivery: the outbox changes only when the test changes it.
  await page.evaluate(() => { (window as any).eval('online = false; _syncFlight = new Promise(() => {}); runSyncBanner = async () => {}; setOnline = () => {}'); });
}

const badge = (page: Page) => page.locator('#peek-tab-steering-count');

test('the Steering badge follows the outbox while another tab shows, and agrees with the list when opened', async ({ page }) => {
  const drift: string[] = [];
  page.on('request', r => { const b = r.postData() || ''; if (b.includes('steer_list_stale')) drift.push(b); });
  await boot(page);
  await page.evaluate(async ({ w, r }) => {
    const g = window as any;
    g.openPeek(w); g._stopPeekPoll?.(); g.setPeekTab('terminal');
    await g._mutateQueue((rows: any[]) => rows.push(r));
  }, { w: W, r: row('a', W) });
  await expect(badge(page), 'a queued message counts while Terminal shows').toHaveText('1');

  // Sent while Terminal shows: it leaves the outbox.
  await page.evaluate(async () => { await (window as any)._mutateQueue((rows: any[]) => { rows.splice(0, rows.length); }); });
  await expect(badge(page), 'the badge clears with no Steering render').toHaveText('');

  await page.evaluate(() => (window as any).setPeekTab('steering'));
  await expect(page.locator('#peek-steering-count')).toHaveText('No queued messages');
  await expect(badge(page)).toHaveText('');

  // Queued and removed again with Steering open: both move together.
  await page.evaluate(async (r) => { await (window as any)._mutateQueue((rows: any[]) => rows.push(r)); }, row('b', W));
  await expect(page.locator('#peek-steering-count')).toHaveText('1 queued');
  await expect(badge(page)).toHaveText('1');
  expect(drift, 'no stale list once every path repaints both').toEqual([]);
});

test('a message sent from another window clears this window\'s Steering badge and list', async ({ context }) => {
  const a = await context.newPage();
  await boot(a);
  await a.evaluate(async ({ w, r }) => {
    const g = window as any;
    await g._mutateQueue((rows: any[]) => rows.push(r));
    g.openPeek(w); g._stopPeekPoll?.(); g.setPeekTab('steering');
  }, { w: W, r: row('c', W) });
  await expect(a.locator('#peek-steering-count')).toHaveText('1 queued');
  await expect(badge(a)).toHaveText('1');

  const b = await context.newPage();
  await boot(b);
  await b.evaluate(async () => { await (window as any)._mutateQueue((rows: any[]) => { rows.splice(0, rows.length); }); });
  await expect(a.locator('#peek-steering-count'), 'the other window\'s send reaches this list').toHaveText('No queued messages');
  await expect(badge(a), 'and this badge').toHaveText('');
});

test('a Steering list left stale is reported and redrawn when the badge updates', async ({ page }) => {
  const stale: any[] = [];
  page.on('request', r => { const b = r.postData() || ''; if (b.includes('steer_list_stale')) { try { stale.push(JSON.parse(b)); } catch { /* not JSON */ } } });
  await boot(page);
  await page.evaluate((w) => { const g = window as any; g.openPeek(w); g._stopPeekPoll?.(); g.setPeekTab('steering'); }, W);
  await expect(page.locator('#peek-steering-count')).toHaveText('No queued messages');
  // A path that changes the queue and repaints only the badge, as the
  // steering_delivered alert did before this fix.
  await page.evaluate(() => { (window as any).eval('offlineQueue').push({ id: 'd', url: '/api/sessions/zz-steer-agree/steer',
    options: { method: 'POST', body: JSON.stringify({ text: 'late', msg_id: 'm-d' }) }, timestamp: Date.now(), state: 'pending', not_attempted: true });
    (window as any)._steeringUpdateBadge(); });
  await expect.poll(() => stale.length).toBe(1);
  expect(stale[0]).toMatchObject({ list: 0, badge: 1, session: W });
  await expect(page.locator('#peek-steering-count'), 'and the list is redrawn').toHaveText('1 queued');
});
