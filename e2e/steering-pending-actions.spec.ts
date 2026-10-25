// A worker's Queued list showed messages saved on this device as "Needs review"
// or "Awaiting server" with Send now and ✕ both disabled, so a stuck message
// could be neither retried nor removed (Ethan, 2026-10-23: "buggy", one row
// queued 4h). They now carry the Messages tab's wording and its two actions.
// The worker and its steer route are fakes: nothing reaches a real worker.
import { test, expect, Page } from './fixtures';

const W = 'zz-steer-pending';

async function open(page: Page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/steer-pending', running: true, status: 'active', flags: '', steering: [] }] }));
  await page.route(`**/api/sessions/${W}/peek?*`, r => r.fulfill({ json: { name: W, output: 'working\n', history: '' } }));
  await page.goto('/');
  await page.waitForFunction((w) => typeof (window as any).openPeek === 'function' && typeof (window as any)._mutateQueue === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  // Hold the outbox: this is about the rows, not background delivery.
  await page.evaluate(() => { (window as any).eval('online = false; _syncFlight = new Promise(() => {}); runSyncBanner = async () => {}; setOnline = () => {}'); });
  await page.evaluate(async (w) => {
    const g = window as any;
    await g._mutateQueue((rows: any[]) => {
      rows.push({ id: 'refused-s', url: '/api/sessions/' + w + '/steer', options: { method: 'POST', body: JSON.stringify({ text: 'refused steer', msg_id: 'm-rs' }) },
        timestamp: Date.now() - 4 * 3600000, attempted_at: Date.now() - 4 * 3600000, attempts: 1, state: 'blocked',
        error: 'Needs review before retry: expired or unsupported operation', needs_decision: 'refused' });
      rows.push({ id: 'fresh-s', url: '/api/sessions/' + w + '/steer', options: { method: 'POST', body: JSON.stringify({ text: 'fresh steer', msg_id: 'm-fs' }) },
        timestamp: Date.now(), state: 'pending', not_attempted: true });
    });
    g.openPeek(w); g._stopPeekPoll?.(); g.setPeekTab('steering'); g._steeringRender();
  }, W);
}

test('Queued rows saved on this device say what happened and can be sent again or removed', async ({ page }) => {
  const beacons: string[] = [];
  page.on('request', r => { const b = r.postData() || ''; if (b.includes('steer_pending_stuck')) beacons.push(b); });
  await open(page);
  const list = page.locator('#peek-steering-list');
  // The old badges said nothing about what to do; the stored error still shows as the reason.
  await expect(list.locator('.steer-pending-state')).toHaveText(['Not sent', 'Saved on this device']);
  const refused = list.locator('div', { has: page.getByText('refused steer', { exact: true }) }).filter({ has: page.locator('.steer-pending-retry') }).last();
  await expect(refused.locator('.steer-pending-state')).toHaveText('Not sent');
  await expect(refused.locator('.steer-pending-note')).toContainText('the worker did not accept it: Needs review before retry');
  await expect(refused.locator('.steer-pending-retry')).toHaveText('Send again');
  await expect(refused.locator('.steer-pending-retry')).toBeEnabled();
  await expect(refused.locator('.steer-pending-remove')).toBeEnabled();
  const fresh = list.locator('div', { has: page.getByText('fresh steer', { exact: true }) }).filter({ has: page.locator('.steer-pending-retry') }).last();
  await expect(fresh.locator('.steer-pending-state')).toHaveText('Saved on this device');
  await expect(fresh.locator('.steer-pending-retry')).toHaveText('Send now');
  // Stuck for 4h is reported once, so a sweep sees it.
  await expect.poll(() => beacons.length).toBe(1);
  await page.evaluate(() => (window as any)._steeringRender());
  await page.waitForTimeout(300);
  expect(beacons.length).toBe(1);

  // Remove works on an unattempted row without asking.
  await fresh.locator('.steer-pending-remove').click();
  await expect.poll(() => page.evaluate(() => (window as any).eval('offlineQueue').some((q: any) => q.id === 'fresh-s'))).toBe(false);
  await page.evaluate(() => (window as any)._steeringRender());
  await expect(list).not.toContainText('fresh steer');

  // Send again clears the refusal and hands it back to the outbox.
  await refused.locator('.steer-pending-retry').click();
  await expect.poll(() => page.evaluate(() => {
    const q = (window as any).eval('offlineQueue').find((e: any) => e.id === 'refused-s');
    return q ? `${q.state}|${q.needs_decision}|${q.attempts}` : 'gone';
  })).toBe('pending||0');
});
