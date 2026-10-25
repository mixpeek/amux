// A worker tab's answer must land on the worker it asked about. Switching
// workers while a request was out painted worker A's memory into worker B's
// editor (and Save then wrote it into B), sent "Send now" cleanup to the wrong
// worker, and let an older Messages answer replace a newer filter's rows
// (Ethan, 2026-10-24: "theres race conditions all over the app"). Workers and
// their routes are fakes: nothing reaches a real worker.
import { test, expect, Page } from './fixtures';

const A = 'zz-race-a', B = 'zz-race-b';

async function boot(page: Page, steeringA: any[] = []) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: A, dir: '/tmp/race-a', running: true, status: 'active', flags: '', steering: steeringA },
    { name: B, dir: '/tmp/race-b', running: true, status: 'active', flags: '', steering: [] }] }));
  await page.route(/\/api\/sessions\/zz-race-[ab]\/peek\?.*/, r => r.fulfill({ json: { output: 'working\n', history: '' } }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('sessions').filter((s: any) => s.name.startsWith('zz-race-')).length === 2);
}
const beaconsOf = (page: Page, verdict: string) => {
  const got: any[] = [];
  page.on('request', r => { const b = r.postData() || ''; if (b.includes(verdict)) { try { got.push(JSON.parse(b)); } catch { /* not JSON */ } } });
  return got;
};

test("A's slow memory answer does not land in B's editor, and Save will not write another worker's text", async ({ page }) => {
  const posts: string[] = [];
  await page.route(/\/api\/sessions\/zz-race-a\/memory$/, async r => {
    await new Promise(res => setTimeout(res, 1500));
    await r.fulfill({ json: { content: 'A memory' } });
  });
  await page.route(/\/api\/sessions\/zz-race-b\/memory$/, async r => {
    if (r.request().method() === 'POST') { posts.push(r.request().postData() || ''); return r.fulfill({ json: { ok: true } }); }
    await r.fulfill({ json: { content: 'B memory' } });
  });
  const dropped = beaconsOf(page, 'peek_stale_response_dropped');
  const refused = beaconsOf(page, 'peek_save_refused_other_worker');
  await boot(page);
  await page.evaluate(({ a, b }) => {
    const g = window as any;
    g.openPeek(a); g._stopPeekPoll?.(); g.setPeekTab('memory');
    g.openPeek(b); g._stopPeekPoll?.(); g.setPeekTab('memory');
  }, { a: A, b: B });
  await expect(page.locator('#peek-memory-input')).toHaveValue('B memory');
  await page.waitForTimeout(1800);   // A's answer has arrived by now
  await expect(page.locator('#peek-memory-input'), "A's late answer is dropped").toHaveValue('B memory');
  await expect.poll(() => dropped.some(d => d.where === 'memory' && d.asked === A && d.now === B)).toBe(true);

  // An editor that still holds A's text (as it could before) is not saved into B.
  await page.evaluate(() => { const el = document.getElementById('peek-memory-input') as HTMLTextAreaElement; el.value = 'A memory'; el.dataset.sess = 'zz-race-a'; (window as any).savePeekMemory(); });
  await expect.poll(() => refused.length).toBe(1);
  await page.waitForTimeout(300);
  expect(posts, "nothing was written to B's memory").toEqual([]);
  await expect(page.locator('#peek-memory-input'), 'and the editor reloads B').toHaveValue('B memory');
});

test('Send now keeps to the worker the message was queued on when you switch mid-send', async ({ page }) => {
  const deletes: string[] = [];
  await page.route(/\/api\/sessions\/zz-race-a\/send$/, async r => {
    await new Promise(res => setTimeout(res, 1200));
    await r.fulfill({ json: { ok: true, message: 'delivered' } });
  });
  await page.route(/\/api\/sessions\/zz-race-[ab]\/steer$/, r => {
    if (r.request().method() === 'DELETE') deletes.push(r.request().url());
    return r.fulfill({ json: { ok: true } });
  });
  await boot(page, [{ id: 'q1', text: 'queued on A', queued_at: Date.now() / 1000 }]);
  await page.evaluate(({ a, b }) => {
    const g = window as any;
    g.openPeek(a); g._stopPeekPoll?.(); g.setPeekTab('steering');
    g._steeringSendNow('q1');
    g.openPeek(b); g._stopPeekPoll?.();
  }, { a: A, b: B });
  await expect.poll(() => deletes.length, { timeout: 5000 }).toBe(1);
  expect(deletes[0], "the queued row is removed from A, not B").toContain('/api/sessions/' + A + '/steer');
});

test("two quick filter taps: the older, slower answer does not replace the newer filter's rows", async ({ page }) => {
  await page.route(/\/api\/history\?.*/, async r => {
    const u = new URL(r.request().url());
    if (u.searchParams.get('counts')) return r.fulfill({ json: { all: 2, human: 1, session: 1, schedule: 0 } });
    const human = u.searchParams.get('kind') === 'human';
    if (human) await new Promise(res => setTimeout(res, 1500));
    const text = human ? 'only-human-row' : 'all-filter-row';
    return r.fulfill({ json: [{ id: human ? 11 : 12, text, type: 'user', session: B, ts: Date.now(), kind: human ? 'human' : 'session' }],
      headers: { 'x-amux-total': '1' } });
  });
  const older = beaconsOf(page, 'older_answer_dropped');
  await boot(page);
  await page.evaluate((b) => {
    const g = window as any;
    g.openPeek(b); g._stopPeekPoll?.();
    g.eval("_peekMsgFilter = 'session'");
    g.setPeekTab('messages');
    g._peekMsgSetFilter('human');
    g._peekMsgSetFilter('all');
  }, B);
  await expect(page.locator('#peek-messages-list')).toContainText('all-filter-row');
  await page.waitForTimeout(1800);   // the human answer has arrived
  await expect(page.locator('#peek-messages-list'), "the older human answer is dropped").not.toContainText('only-human-row');
  await expect(page.locator('#peek-messages-list')).toContainText('all-filter-row');
  await expect.poll(() => older.length).toBeGreaterThan(0);
  // The badge reads the same server count as the lit chip.
  await expect(page.locator('#peek-tab-messages-count')).toHaveText('2');
});

test('pausing a schedule from the Schedules tab updates its tab badge with the list', async ({ page }) => {
  await boot(page);
  await page.evaluate((a) => {
    const g = window as any;
    g.eval(`schedules = [{id:'SCHED-R1', session:'${a}', title:'race', enabled:1, schedule_expr:'every 15m', command:'x', kind:'shell'}]`);
    g.openPeek(a); g._stopPeekPoll?.();
    g._peekColorSchedBadge(g.eval('schedules').filter((x: any) => x.session === a));
  }, A);
  const badge = page.locator('#peek-tab-schedules-count');
  const before = await badge.textContent();
  await page.evaluate(() => { const g = window as any; g.eval('schedules')[0].enabled = 0; g._peekRefreshSchedIfActive(); });
  await expect(badge, 'the badge follows the paused schedule').not.toHaveText(before || '');
  await expect(badge).toHaveAttribute('title', '0 active, 1 inactive schedule');
});
