import { test, expect } from './fixtures';

test('message loaders preserve recorded delivery and failed submission on every surface', async ({ page }) => {
  const rows = [
    { id: 88001, text: 'queued metadata fixture', type: 'user', session: 'receipt-fixture', ts: Date.now(), delivery: 'queued', queued_at: 1000, delivered_at: null, submit_verdict: null },
    { id: 88002, text: 'failed metadata fixture', type: 'user', session: 'receipt-fixture', ts: Date.now(), delivery: 'direct', delivered_at: null, submit_verdict: 'stuck' },
  ];
  await page.route('**/api/history?*', async route => {
    const url = new URL(route.request().url());
    await route.fulfill({ json: url.searchParams.has('counts') ? { all: 2, human: 2 } : rows });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekMsgFetch === 'function');
  const rendered = await page.evaluate(async () => {
    const w = window as any;
    const scoped = await w._peekMsgFetch({ level: 'worker', name: 'receipt-fixture' });
    await w._loadCmdHistoryFromServer();
    const global = (0, eval)('_cmdHistory').filter((r: any) => r.session === 'receipt-fixture');
    const render = (items: any[], ctx: any) => items.map(r => w._cmdHistItemHTML(r, ctx)).join('');
    return { scoped, global, surfaces: [w._msgCtxMessages(), w._msgCtxHistory(), w._msgCtxPeek()].map(ctx => render(scoped, ctx)) };
  });
  for (const items of [rendered.scoped, rendered.global]) {
    expect(items.find((r: any) => r.id === 88001).delivery).toBe('queued');
    expect(items.find((r: any) => r.id === 88002).submit_verdict).toBe('stuck');
  }
  for (const html of rendered.surfaces) {
    expect(html).toContain('queued');
    expect(html).toContain('NOT SENT');
    expect(html).not.toContain('direct?');
  }
});
