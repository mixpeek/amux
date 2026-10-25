// MSG-92727 (2026-10-22): a message sent with two screenshots showed twice in
// Messages: the delivered server row, and a ghost "queued" row with no id. The
// server records attachments appended as @/path mentions; the local row keeps
// only the typed text, so the echo check never matched them.
import { test, expect } from './fixtures';

test('a sent message with attachments is one row once the server echoes it', async ({ page }) => {
  const now = Date.now();
  const typed = 'still buggy mac os, and like it wont send';
  const rows = [{ id: 92727, text: typed + ' @/Users/x/.amux/uploads/16ad-image.png @/Users/x/.amux/uploads/a569-image.png',
    type: 'user', kind: 'human', session: 'echo-fixture', ts: now, delivery: 'queued', queued_at: now, delivered_at: now + 12000 }];
  await page.route('**/api/history?*', async route => {
    const url = new URL(route.request().url());
    await route.fulfill({ json: url.searchParams.has('counts') ? { all: 1, human: 1 } : rows });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekMsgFetch === 'function' && typeof (window as any).cmdHistoryAdd === 'function');
  const out = await page.evaluate(async ({ typed }) => {
    const w = window as any;
    // The local row the composer writes at send: the typed text, no id yet.
    w.cmdHistoryAdd(typed, { type: 'steering', session: 'echo-fixture' });
    w.cmdHistoryAdd('a different message still in flight', { type: 'steering', session: 'echo-fixture' });
    const scoped = await w._peekMsgFetch({ level: 'worker', name: 'echo-fixture' });
    const merged = w._mergeUnechoed(scoped, 'echo-fixture');
    await w._loadCmdHistoryFromServer();
    const global = (0, eval)('_cmdHistory').filter((r: any) => r.session === 'echo-fixture');
    const count = (list: any[], t: string) => list.filter((r: any) => String(r.text).startsWith(t)).length;
    return { scoped: count(merged, typed), global: count(global, typed), ids: merged.map((r: any) => r.id || null),
             other: count(merged, 'a different message') };
  }, { typed });
  expect(out.scoped, 'worker Messages: one row').toBe(1);
  expect(out.global, 'global history: one row').toBe(1);
  expect(out.ids).toContain(92727);
  expect(out.other, 'control: a message the server has not echoed stays').toBe(1);
});
