// Governance PR 12: an approval (a spend grant, a publish, a held email, an
// owner-gated land, a capability ask) is a Needs-you card, and the triage
// buttons decide it through POST /api/approvals/decide, which runs the item's
// own approve or reject. Approve & remember records a standing approval for
// the exact same request; a land approval cannot be remembered. The card is
// never moved by hand. The queue and every write are fakes.
import { test, expect, allowUnusedRoute } from './fixtures';

const NOW = () => Math.floor(Date.now() / 1000);

test('the triage buttons decide approval cards through their own path', async ({ page }) => {
  const decided: any[] = [];
  const patches: any[] = [];
  const w = { name: 'ac-worker', running: true, status: 'waiting', waiting_reason: 'owner', lifecycle: 'active',
    tags: [], dir: '/tmp', flags: '', provider: 'claude', model: 'claude-opus-5-5' };
  const item = (card: string, kind: string, id: string, q: string, remember: boolean) => ({
    key: 'card:' + card, kind: 'card', card, title: q, worker: w.name, status: 'needsyou', ask_type: 'budget',
    question: q, unblocks: 'Approving lets the worker continue.', category: 'money', rank: 0, since: NOW() - 60,
    chips: [], approval: { ref: kind + ':' + id, kind, id, remember, expires_at: null },
  });
  let queue = [
    item('AC-1', 'grant', 'g_one', 'Let ac-worker charge USD 250.00 at Test Shop?', true),
    item('AC-2', 'grant', 'g_two', 'Let ac-worker charge USD 80.00 at Test Shop?', true),
    item('AC-3', 'land', '88f8f5b067f2794864ffc76bbc61bff2f850aafc', 'Approve ac-worker\'s change 88f8f5b067f2?', false),
  ];
  await page.route('**/api/needs-input', r => r.fulfill({ json: { items: queue, count: queue.length, snoozed: [], measured: true, n_considered: queue.length } }));
  await page.route('**/api/approvals/decide', async r => {
    const b = JSON.parse(r.request().postData() || '{}');
    decided.push(b);
    queue = queue.filter(i => i.approval.ref !== b.approval_ref);
    return r.fulfill({ json: { ok: true, approval_ref: b.approval_ref, card: queue.length ? 'AC' : null,
      ...(b.remember ? { standing_approval: 7 } : {}) } });
  });
  await page.route('**/api/board/AC-*', async r => {
    if (r.request().method() === 'PATCH') patches.push(JSON.parse(r.request().postData() || '{}'));
    return r.fulfill({ json: { id: 'AC-1', status: 'needsyou', desc: '' } });
  });
  allowUnusedRoute(page, '**/api/board/AC-*');   // the point: an approval card is never moved by hand
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

  const question = page.locator('#ni-question');
  const approve = page.locator('.ni-approve');
  const remember = page.locator('.ni-always');

  // 1. Approve: one decide call, the item's own approval ref.
  await expect(question).toContainText('USD 250.00');
  await expect(remember).toHaveText(/Approve & remember/);
  await expect(remember).toBeEnabled();
  await approve.click();
  await expect.poll(() => decided.length).toBe(1);
  expect(decided[0]).toEqual({ approval_ref: 'grant:g_one', decision: 'approve' });

  // 2. Approve & remember for 30 days.
  await expect(question).toContainText('USD 80.00');
  await remember.click();
  const until = page.locator('#ni-remember-until');
  await expect(until).toBeVisible();
  await until.selectOption('30');
  await page.locator('.ni-remember-go').click();
  await expect.poll(() => decided.length).toBe(2);
  expect(decided[1].approval_ref).toBe('grant:g_two');
  expect(decided[1].decision).toBe('approve');
  const days = (decided[1].remember.until - NOW()) / 86400;
  expect(days).toBeGreaterThan(29);
  expect(days).toBeLessThan(31);

  // 3. A land approval cannot be remembered; Decline rejects it.
  await expect(question).toContainText('88f8f5b067f2');
  await expect(remember).toBeDisabled();
  await page.locator('.ni-decline').click();
  await expect.poll(() => decided.length).toBe(3);
  expect(decided[2]).toEqual({ approval_ref: 'land:88f8f5b067f2794864ffc76bbc61bff2f850aafc', decision: 'reject' });

  expect(patches).toEqual([]);
  await expect(page.locator('.ni-empty')).toBeVisible();
});
