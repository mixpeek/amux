// A held outbound email can be scheduled instead of sent now (Ethan,
// 2026-11-08: "when i approve outbound email there should be another button to
// schedule for a date/time in the approval flow test e2e including mobile").
// Real server, real held draft: a worker's send to an outside address is held,
// the banner's Schedule opens a date and time row, Schedule send records it on
// the server, and the card then reads "sends <when>" with Send now and
// Reschedule. Runs in every project, the 375px phone included.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('schedule a held email for a date and time from the approval banner', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amuxApprCollapsed'); });
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const lane = `sched-${info.project.name}-${Date.now()}`;
  const subject = `Schedule check ${lane}`;
  const held = await request.post('/api/email/send', {
    headers: { ...auth, 'X-Amux-Session': lane },
    data: { to: 'buyer@example.org', subject, body: 'Hello from the schedule test.', from: 'info@example.com', force_new_thread: true },
  });
  expect(held.status(), await held.text()).toBe(403);
  const id: string = (await held.json()).approval_id;
  expect(id).toMatch(/^apr_[0-9a-f]{16}$/);
  try {
    await page.waitForFunction(() => typeof (window as any)._approvalsRefresh === 'function');
    await page.evaluate(() => (window as any)._approvalsRefresh());
    const card = page.locator('#email-approvals-banner details', { hasText: lane });
    await expect(card).toBeVisible();
    const schedule = card.locator('button.appr-schedule');
    await expect(schedule).toHaveText('Schedule');

    // Schedule opens a date and time row, defaulting to a future time.
    await schedule.click();
    const at = page.locator(`#appr-sched-at-${id}`);
    await expect(at).toBeVisible();
    expect(new Date(await at.inputValue()).getTime()).toBeGreaterThan(Date.now());
    const confirm = card.locator('button.appr-sched-confirm');
    await expect(confirm).toBeVisible();

    // Phone: the row fits the screen and its controls are touch-sized.
    const vw = page.viewportSize()!.width;
    for (const el of [at, confirm]) {
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

    // Pick tomorrow 10:30 and schedule it.
    const when = new Date(); when.setDate(when.getDate() + 1); when.setHours(10, 30, 0, 0);
    await at.fill(local(when));
    await confirm.click();
    await expect(card).toContainText('sends tomorrow', { timeout: 10_000 });
    await expect(card.locator('button', { hasText: 'Send now' })).toBeVisible();
    await expect(card.locator('button.appr-schedule')).toHaveText('Reschedule');

    // The server holds it, scheduled for that minute, and has sent nothing.
    const list = await (await request.get('/api/email/approvals', { headers: auth })).json();
    const row = list.pending.find((p: any) => p.id === id);
    expect(row, 'still held').toBeTruthy();
    expect(row.scheduled.send_at).toBe(when.getTime() / 1000);
    expect(row.scheduled.approver).toBe('dashboard');
    const log = await (await request.get('/api/email/log?days=1', { headers: auth })).json();
    const rows = (log.log || []).filter((r: any) => r.approval_id === id);
    expect(rows.some((r: any) => r.scheduled === true)).toBe(true);
    expect(rows.some((r: any) => r.approved === true)).toBe(false);

    // Reschedule opens on the scheduled time.
    await card.locator('button.appr-schedule').click();
    await expect(page.locator(`#appr-sched-at-${id}`)).toHaveValue(local(when));
  } finally {
    await cleanup('discard the held test email', () => request.post(`/api/email/reject/${id}`,
      { headers: { ...auth, 'X-Amux-Approver': 'e2e' } }));
  }
});
