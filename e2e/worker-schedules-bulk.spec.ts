// A worker's Schedules tab selects rows and acts on them in bulk, like the
// Scheduler page (Ethan, 2026-10-30: "make it so i can do multiselect on a
// workers' schedules like bulk delete"). The tab hid the checkboxes and had no
// bulk bar. Workers and schedules are fakes; DELETE is answered here and
// recorded, so nothing real is deleted.
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const A = 'zz-sbulk-a', B = 'zz-sbulk-b';
const SCHEDS = [
  { id: 'SCHED-SB1', session: A, title: 'a one', enabled: 1, schedule_expr: 'every 15m', command: 'x', kind: 'shell' },
  { id: 'SCHED-SB2', session: A, title: 'a two', enabled: 1, schedule_expr: 'daily at 9am', command: 'y', kind: 'tmux' },
  { id: 'SCHED-SB3', session: A, title: 'a three', enabled: 1, schedule_expr: 'every 1h', command: 'z', kind: 'shell' },
  { id: 'SCHED-SB4', session: B, title: 'b one', enabled: 1, schedule_expr: 'every 15m', command: 'w', kind: 'shell' },
];

async function boot(page: Page) {
  const deleted: string[] = [];
  let live = SCHEDS.slice();
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: A, dir: '/tmp/sbulk-a', running: true, status: 'idle', flags: '', steering: [] },
    { name: B, dir: '/tmp/sbulk-b', running: true, status: 'idle', flags: '', steering: [] }] }));
  await page.route(/\/api\/sessions\/zz-sbulk-[ab]\/peek\?.*/, r => r.fulfill({ json: { output: 'idle\n', history: '' } }));
  await page.route(/\/api\/schedules\/runs(?:\?.*)?$/, r => r.fulfill({ json: [] }));
  const one = /\/api\/schedules\/SCHED-SB\d(?:\?.*)?$/;
  // Clear sends no request, so the test that only clears never reaches this
  // stub; the delete test does and checks what it recorded.
  allowUnusedRoute(page, one);
  await page.route(one, r => {
    const id = r.request().url().match(/SCHED-SB\d/)![0];
    if (r.request().method() !== 'DELETE') return r.fulfill({ json: { ok: true } });
    deleted.push(id);
    live = live.filter(s => s.id !== id);
    return r.fulfill({ json: { ok: true, deleted: id } });
  });
  await page.route(/\/api\/schedules(?:\?.*)?$/, r => r.fulfill({ json: live }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('sessions').filter((s: any) => s.name.startsWith('zz-sbulk-')).length === 2);
  return deleted;
}

test('a worker\'s schedules can be selected and deleted together, and only that worker\'s', async ({ page }) => {
  const deleted = await boot(page);
  // A row of another worker picked on the Scheduler page must not ride along.
  await page.evaluate((b) => { (globalThis as any).eval('_sxSelected').add('SCHED-SB4'); (window as any).openPeek(b); }, B);
  await page.evaluate((a) => { const g = window as any; g.openPeek(a); g._stopPeekPoll?.(); g.setPeekTab('schedules'); }, A);
  const list = page.locator('#peek-schedules-list');
  await expect(list.locator('.sx-row.sched-item')).toHaveCount(3);
  const bar = page.locator('#peek-sched-bulk');
  await expect(bar, 'nothing of this worker is selected yet').toBeHidden();

  for (const id of ['SCHED-SB1', 'SCHED-SB3']) {
    const box = list.locator(`.sx-row[data-sched-id="${id}"] .sx-sel input`);
    await expect(box, `${id} has a visible checkbox`).toBeVisible();
    await box.check();
  }
  await expect(bar).toBeVisible();
  await expect(page.locator('#peek-sched-bulk-n')).toHaveText('2 selected');
  await expect(list.locator('.sx-row.selected')).toHaveCount(2);
  // Checking a box selects the row; it does not open the schedule's details.
  await expect(page.locator('.sx-detail.open, #sched-detail.open')).toHaveCount(0);

  await bar.getByRole('button', { name: 'Delete' }).click();
  await expect(page.locator('#modal-msg')).toContainText('Delete 2 schedules');
  await page.locator('#modal-btns').getByRole('button', { name: 'Delete' }).click();
  await expect.poll(() => deleted.slice().sort()).toEqual(['SCHED-SB1', 'SCHED-SB3']);
  await expect(list.locator('.sx-row.sched-item')).toHaveCount(1);
  await expect(bar, 'the bar goes away once the selection is spent').toBeHidden();
  expect(await page.evaluate(() => [...(globalThis as any).eval('_sxSelected')]), 'the other worker\'s pick is kept, not deleted').toEqual(['SCHED-SB4']);
});

test('Clear empties only this worker\'s selection', async ({ page }) => {
  await boot(page);
  await page.evaluate((a) => { const g = window as any; g.openPeek(a); g._stopPeekPoll?.(); g.setPeekTab('schedules'); }, A);
  const list = page.locator('#peek-schedules-list');
  await list.locator('.sx-row[data-sched-id="SCHED-SB2"] .sx-sel input').check();
  await page.locator('#peek-sched-bulk').getByRole('button', { name: 'Clear' }).click();
  await expect(page.locator('#peek-sched-bulk')).toBeHidden();
  await expect(list.locator('.sx-row.selected')).toHaveCount(0);
  await expect(list.locator('.sx-row[data-sched-id="SCHED-SB2"] .sx-sel input')).not.toBeChecked();
});
