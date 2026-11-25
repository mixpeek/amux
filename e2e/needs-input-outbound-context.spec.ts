// Owner, 2026-11-25: every direct outbound message waits for approval, and
// the approval is "rich enough for full context". A held X reply's Needs-you
// card shows where it goes, why, the message it answers, the recent
// conversation and the whole text (api/outbound.rs), and Approve decides it
// through its own approval. The queue and the decide call are fakes.
import { test, expect, allowUnusedRoute } from './fixtures';

const NOW = () => Math.floor(Date.now() / 1000);

test('a held outbound message shows its full context and is decided by its approval', async ({ page }) => {
  const decided: any[] = [];
  const w = { name: 'ob-worker', running: true, status: 'waiting', waiting_reason: 'owner', lifecycle: 'active',
    tags: [], dir: '/tmp', flags: '', provider: 'claude', model: 'claude-opus-5-5' };
  const long = 'We index video frame by frame, so a query finds the moment. ' + 'More detail follows. '.repeat(60) + 'THE-LAST-LINE';
  let queue: any[] = [{
    key: 'card:OB-1', kind: 'card', card: 'OB-1', title: 'ob-worker: x reply to @jane', worker: w.name, status: 'needsyou',
    ask_type: 'customer_outbound', question: 'Let ob-worker send this x reply to https://x.com/jane/status/9?',
    unblocks: 'ob-worker may post exactly this text on x.com for 30 minutes.', category: 'outbound', rank: 0, since: NOW() - 60,
    context: 'the clipped card description', chips: [],
    approval: { ref: 'outbound:OUT-1', kind: 'outbound', id: 'OUT-1', remember: false, expires_at: null },
    outbound: {
      channel: 'x', action: 'reply', account: '@ethansteininger', to: '@jane', target_url: 'https://x.com/jane/status/9',
      text: long, worker: w.name, reason: 'she asked how we index video',
      parent: { author: '@jane', text: 'How do you index video?', url: 'https://x.com/jane/status/9', ts: '2026-11-25' },
      thread: [{ author: '@jane', text: 'first message' }, { author: '@ethansteininger', text: 'second message' }],
    },
  }];
  await page.route('**/api/needs-input', r => r.fulfill({ json: { items: queue, count: queue.length, snoozed: [], measured: true, n_considered: queue.length } }));
  await page.route('**/api/approvals/decide', async r => {
    const b = JSON.parse(r.request().postData() || '{}');
    decided.push(b);
    queue = [];
    return r.fulfill({ json: { ok: true, approval_ref: b.approval_ref, card: null } });
  });
  await page.route('**/api/needs-input/log', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/needs-input/log');
  const sessionsList = /\/api\/sessions(?:\?.*)?$/;
  await page.route(sessionsList, r => r.request().method() === 'GET' ? r.fulfill({ json: [w] }) : r.fallback());
  allowUnusedRoute(page, sessionsList);
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openNeedsInput === 'function');
  await page.evaluate(async () => { await (window as any)._niFetch(); await (window as any).openNeedsInput(); });

  const card = page.locator('.ni-outbound');
  await expect(card).toContainText('x reply, as @ethansteininger');
  await expect(card).toContainText('@jane');
  await expect(card).toContainText('she asked how we index video');
  const contexts = page.locator('.ni-context');
  await expect(contexts.filter({ hasText: 'Replying to @jane' })).toContainText('How do you index video?');
  await expect(contexts.filter({ hasText: 'Recent conversation (2)' })).toHaveCount(1);
  const full = contexts.filter({ hasText: 'Full text' }).locator('pre');
  await expect(full).toContainText('THE-LAST-LINE');   // the whole text, not the 600-character clip
  await expect(page.locator('.ni-context', { hasText: 'the clipped card description' })).toHaveCount(0);

  // Phone width: the message stays inside the screen.
  await page.setViewportSize({ width: 375, height: 800 });
  const box = await full.boundingBox();
  expect(box && box.x + box.width).toBeLessThanOrEqual(376);

  await page.locator('.ni-approve').click();
  await expect.poll(() => decided.length).toBe(1);
  expect(decided[0]).toEqual({ approval_ref: 'outbound:OUT-1', decision: 'approve' });
});
