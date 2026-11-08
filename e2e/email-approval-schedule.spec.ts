// A held outbound email can be scheduled instead of sent now (Ethan,
// 2026-11-08: "when i approve outbound email there should be another button to
// schedule for a date/time in the approval flow test e2e including mobile").
// The banner's Schedule opens a date and time row, Schedule send posts the time
// with the approver marker, and the card then reads "sends <when>" with Send
// now and Reschedule. Runs in every project, the 375px phone included.
//
// THE QUEUE IS MOCKED IN THIS PAGE, NOT FILLED ON THE SERVER. A real held email
// shows its banner on every page served by the same test server, and the first
// version of this spec created one: draw-realtime, running beside it on the
// mobile server, drew its rectangle under the banner and failed on two CI runs
// (2026-11-08). The server side (held, sent once at its time, recovery) is
// covered by the email tests in crates/amux-server/src/api/email.rs.
import { test, expect } from './fixtures';

const ID = 'apr_5c4ed01e5c4ed01e';

function held(scheduled?: { send_at: number }) {
  return {
    id: ID, session: 'gtm-engine', age_s: 120, expires: false, expires_in_s: null, requests: 1,
    ...(scheduled ? { scheduled: { ...scheduled, approver: 'dashboard', edited: false } } : {}),
    preview: { endpoint: 'send', from: 'info@example.com', to: 'buyer@example.org', subject: 'Pricing follow-up' },
    review: { body: 'Hello from the schedule test.', body_complete: true, attachment_count: 0, complete: true },
  };
}

test('schedule a held email for a date and time from the approval banner', async ({ page }) => {
  let scheduledAt: number | null = null;
  const posts: { body: any; approver: string | undefined }[] = [];
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amuxApprCollapsed'); });
  await page.route(/\/api\/email\/approvals(?:\?.*)?$/, (r) =>
    r.fulfill({ json: { pending: [held(scheduledAt ? { send_at: scheduledAt } : undefined)] } }));
  await page.route(/\/api\/grants(?:\?.*)?$/, (r) => r.fulfill({ json: { pending: [] } }));
  await page.route(new RegExp(`/api/email/approve/${ID}$`), async (r) => {
    const body = r.request().postDataJSON();
    posts.push({ body, approver: r.request().headers()['x-amux-approver'] });
    scheduledAt = body.send_at;
    await r.fulfill({ json: { ok: true, scheduled: true, delivered: false, approval_id: ID, send_at: body.send_at, sent_for_session: 'gtm-engine' } });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._approvalsRefresh === 'function');
  await page.evaluate(() => (window as any)._approvalsRefresh());
  const card = page.locator('#email-approvals-banner details', { hasText: 'gtm-engine' });
  await expect(card).toBeVisible();
  const schedule = card.locator('button.appr-schedule');
  await expect(schedule).toHaveText('Schedule');
  await expect(card.locator('button', { hasText: 'Approve' })).toBeVisible();

  // Schedule opens a date and time row, defaulting to a future time.
  await schedule.click();
  const at = page.locator(`#appr-sched-at-${ID}`);
  await expect(at).toBeVisible();
  expect(new Date(await at.inputValue()).getTime()).toBeGreaterThan(Date.now());
  const confirm = card.locator('button.appr-sched-confirm');
  await expect(confirm).toBeVisible();

  // Phone: the row fits the screen and its controls are touch-sized.
  const vw = page.viewportSize()!.width;
  for (const el of [at, confirm, card.locator('button.appr-sched-cancel')]) {
    const b = (await el.boundingBox())!;
    expect(b.x).toBeGreaterThanOrEqual(0);
    expect(b.x + b.width).toBeLessThanOrEqual(vw + 1);
    expect(b.height).toBeGreaterThanOrEqual(44);
  }
  const sb = (await schedule.boundingBox())!;
  expect(sb.x + sb.width).toBeLessThanOrEqual(vw + 1);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true);

  // A time in the past is refused in the page, before any request.
  const pad = (n: number) => String(n).padStart(2, '0');
  const local = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  await at.fill(local(new Date(Date.now() - 3600_000)));
  await confirm.click();
  await expect(page.locator('#toast')).toContainText('future');
  expect(posts).toHaveLength(0);

  // Pick tomorrow 10:30 and schedule it: one request, naming the approver.
  const when = new Date(); when.setDate(when.getDate() + 1); when.setHours(10, 30, 0, 0);
  await at.fill(local(when));
  await confirm.click();
  await expect(card).toContainText('sends tomorrow', { timeout: 10_000 });
  expect(posts).toHaveLength(1);
  expect(posts[0].body.send_at).toBe(when.getTime() / 1000);
  expect(posts[0].approver).toBe('dashboard');
  await expect(card.locator('button', { hasText: 'Send now' })).toBeVisible();
  await expect(card.locator('button.appr-schedule')).toHaveText('Reschedule');

  // Reschedule opens on the scheduled time.
  await card.locator('button.appr-schedule').click();
  await expect(page.locator(`#appr-sched-at-${ID}`)).toHaveValue(local(when));
});
