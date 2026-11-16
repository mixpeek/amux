// Governance PR 6: the owner's approval from the dashboard is recorded as the
// proven owner (`owner-token`, approver_verified: true), whatever name the
// banner sends in X-Amux-Approver. Runs against the suite's real server, so the
// held draft is real: a worker queues an external send (the page's own fetch
// carries the owner bearer, plus the worker header that makes the send a
// worker's), the banner shows it, and the owner schedules it for a date far
// ahead. Scheduling records the decision without sending, so nothing here can
// reach a mail provider. The draft is withdrawn in `finally`, and named per
// project because projects share one server.
//
// The send needs a sender. A worker's send that no account could make is
// refused before the hold (de0fc8719), and the suite's server has no mail
// account, so the spec adds a file mailbox (read-only, it can never send) for
// the length of the test. Scheduling records the decision before the
// draft-only check that a real send would meet.
//
// Who may list, withdraw and approve (workers, apps, unproven callers) is
// covered end to end by e2e/chaos/email-approval-scope.mjs and by the handler
// test `held_email_is_scoped_to_the_proven_caller`.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import fs from 'fs';
import path from 'path';

test('the owner releases a held email from the banner, recorded as the proven owner', async ({ page }, info) => {
  const lane = `pw-scope-${info.project.name}`.replace(/[^a-z0-9-]/gi, '-').toLowerCase();
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amuxApprCollapsed'); });
  const home = String(info.project.metadata?.home || '');
  expect(home, 'premise: the config names this project\'s AMUX_HOME').not.toBe('');
  const acct = `${lane}@example.com`;
  const box = path.join(home, 'mailboxes', `${acct}.json`);
  fs.mkdirSync(path.dirname(box), { recursive: true });
  fs.writeFileSync(box, JSON.stringify({ account: acct, messages: [] }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._authHeaders === 'function' && typeof (window as any)._approvalsRefresh === 'function');
  const id: string = await page.evaluate(async ({ lane, acct }) => {
    const h = (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': lane });
    const r = await fetch('/api/email/send', {
      method: 'POST', headers: h,
      body: JSON.stringify({ from: acct, to: 'buyer@customer.example', subject: 'Scope spec ' + lane, body: 'held by the scope spec', force_new_thread: true }),
    });
    return (await r.json()).approval_id;
  }, { lane, acct });
  expect(id, 'premise: a worker\'s external send is held').toMatch(/^apr_/);
  try {
    await page.evaluate(() => (window as any)._approvalsRefresh());
    const card = page.locator('#email-approvals-banner details', { hasText: lane });
    await expect(card).toBeVisible();
    await card.locator('button.appr-schedule').click();
    const at = page.locator(`#appr-sched-at-${id}`);
    await expect(at).toBeVisible();
    const pad = (n: number) => String(n).padStart(2, '0');
    const when = new Date(); when.setDate(when.getDate() + 30); when.setHours(10, 0, 0, 0);
    await at.fill(`${when.getFullYear()}-${pad(when.getMonth() + 1)}-${pad(when.getDate())}T10:00`);
    const confirm = card.locator('button.appr-sched-confirm');
    const vw = page.viewportSize()!.width;
    const b = (await confirm.boundingBox())!;
    expect(b.x + b.width).toBeLessThanOrEqual(vw + 1);
    await confirm.click();
    await expect(page.locator('#toast')).toContainText('Scheduled', { timeout: 10_000 });

    const row = await page.evaluate(async (id) => {
      const h = (window as any)._authHeaders({});
      const log = await fetch('/api/email/log?limit=200', { headers: h }).then((x) => x.json());
      return (log.log || []).find((x: any) => x.approval_id === id && x.scheduled === true) || null;
    }, id);
    expect(row, 'the decision is in the ledger').not.toBeNull();
    expect(row.approved_by, 'the proven owner, not the name the banner sent').toBe('owner-token');
    expect(row.approver_verified).toBe(true);
  } finally {
    await cleanup('withdraw the held draft', () => page.evaluate((id) => {
      const h = (window as any)._authHeaders({ 'Content-Type': 'application/json' });
      return fetch('/api/email/reject/' + id, { method: 'POST', headers: h, body: '{}' }).catch(() => {});
    }, id), info);
    await cleanup('remove the fixture mailbox', async () => fs.rmSync(box, { force: true }), info);
  }
});
