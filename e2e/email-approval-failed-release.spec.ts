// A release that fails says so (sandbox, 2026-11-12). The server answered 502
// for an approval it could not send; the offline outbox queued the POST and
// handed back its synthetic 202, so the banner said "Approved — sent for
// worker" while nothing was sent, and the queue replayed the approval seven
// times. Approve and Discard go to the server and report its answer.
import { test, expect } from './fixtures';

const ID = 'apr_failedrelease1';

test('an approval the server could not send reads as failed and is not queued', async ({ page }) => {
  const approves: string[] = [];
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/email\/approvals(?:\?.*)?$/, (r: any) => r.fulfill({ json: { pending: [{
    id: ID, session: 'wexus', age_s: 60, expires: false, expires_in_s: null, requests: 1,
    preview: { endpoint: 'send', from: '', to: 'buyer@example.org', subject: 'Agreement', body: 'Attached.',
      external_recipients: ['buyer@example.org'] },
  }], scheduled: [] } }));
  await page.route(/\/api\/email\/approve\//, (r: any) => {
    approves.push(r.request().method());
    return r.fulfill({ status: 502, json: { error: 'invalid connector account or family', approval_id: ID } });
  });
  await page.route(/\/api\/grants(?:\?.*)?$/, (r: any) => r.fulfill({ json: { grants: [] } }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._approvalsRefresh === 'function');
  await page.evaluate(() => (window as any)._approvalsRefresh());

  await page.evaluate((id) => (window as any)._apprApprove(id, null), ID);
  await expect(page.locator('#toast')).toContainText('Approve failed: invalid connector account or family');
  await expect(page.locator('#toast')).not.toContainText('sent for');
  expect(approves).toEqual(['POST']);
  // Nothing waits to replay it later.
  const queued = await page.evaluate(() => JSON.stringify((window as any)._readQueue?.() || []));
  expect(queued).not.toContain('/api/email/approve/');
});
