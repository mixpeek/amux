// Pass/fail schedules (Ethan, 2026-10-26): turning pass/fail on requires a
// check, a command whose exit code says whether the work is done. The server
// re-runs a schedule whose check fails until it passes
// (runtime_jobs/schedule_check.rs); this spec covers the form and how the
// check's state reads. The harness server runs the scheduler in shadow mode,
// so no check runs here: a saved check stays in `probe`.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

async function settle(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openSchedModal === 'function'
    && typeof (window as any).apiCall === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
}

test('pass/fail cannot be saved without a check, and a saved check shows on the schedule', async ({ page, request }) => {
  await settle(page);
  const tok = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const title = `pf-${Date.now()}`;
  const mine = async () => ((await (await request.get('/api/schedules', { headers: { Authorization: `Bearer ${tok}` } })).json()) as any[])
    .filter(s => s.title === title);

  await page.evaluate("switchView('scheduler')");
  await page.evaluate('openSchedModal()');
  await expect(page.locator('#sched-overlay')).toBeVisible();
  await page.fill('#sched-title', title);
  await page.locator('label[for="sched-kind-shell"]').click();
  await page.fill('#sched-command', 'make-report');
  await expect(page.locator('#sched-pass-check-field')).toBeHidden();
  await page.locator('#sched-pass-fail').check();
  await expect(page.locator('#sched-pass-check')).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow, 'the form fits the screen').toBeLessThanOrEqual(1);

  // On with no check: refused in the form, nothing reaches the server.
  await page.locator('#sched-save-btn').click();
  await expect(page.locator('#sched-pass-check')).toHaveAttribute('aria-invalid', 'true');
  expect(await mine()).toHaveLength(0);

  await page.fill('#sched-pass-check', 'test -s /tmp/pf-report-that-is-not-there.md');
  await page.locator('#sched-save-btn').click();
  await expect(page.locator('#sched-overlay')).toBeHidden();
  const [row] = await mine();
  expect(row.pass_check).toBe('test -s /tmp/pf-report-that-is-not-there.md');
  expect(row.pass_state).toBe('probe');

  await page.evaluate(`openSchedDetail(${JSON.stringify(row.id)})`);
  const body = page.locator('#sched-detail-body');
  await expect(body).toContainText('Pass check');
  await expect(body).toContainText('test -s /tmp/pf-report-that-is-not-there.md');
  await expect(body).toContainText('Checking now');

  // Editing shows the switch on with the saved check, and turning it off clears it.
  await page.evaluate(`closeSchedDetail(); openSchedModal(${JSON.stringify(row.id)})`);
  await expect(page.locator('#sched-pass-fail')).toBeChecked();
  await expect(page.locator('#sched-pass-check')).toHaveValue('test -s /tmp/pf-report-that-is-not-there.md');
  await page.locator('#sched-pass-fail').uncheck();
  await page.locator('#sched-save-btn').click();
  await expect(page.locator('#sched-overlay')).toBeHidden();
  const [cleared] = await mine();
  expect(cleared.pass_check ?? null).toBeNull();
});

test('a failing check reads as failing, with the check output, even after a delivered run', async ({ page }) => {
  // The harness runs no checks (shadow mode), so the list the page fetches
  // carries a schedule whose check has failed.
  const row = { id: 'SCHED-pf', title: 'pf report', kind: 'shell', command: 'make-report', enabled: 1,
    schedule_expr: 'daily at 09:00', sched_type: 'recurring', session: '',
    pass_check: 'test -s /tmp/report.md', pass_state: 'fail', pass_attempts: 3,
    pass_checked_at: Math.floor(Date.now() / 1000), pass_detail: 'exit 1\nno report yet' };
  await page.route(/\/api\/schedules(\?.*)?$/, (route) => route.request().method() === 'GET'
    ? route.fulfill({ json: [row] }) : route.continue());
  await settle(page);
  const r = await page.evaluate((s) => (globalThis as any).eval(`(() => {
    const s = ${JSON.stringify(s)};
    const delivered = [{ schedule_id: s.id, status: 'ok', ran_at: Math.floor(Date.now() / 1000) }];
    return { fail: _sxStatus(s, delivered),
      broken: _sxStatus(Object.assign({}, s, { pass_state: 'unrunnable' }), delivered),
      passing: _sxStatus(Object.assign({}, s, { pass_state: 'pass' }), delivered) };
  })()`), row);
  expect(r.fail).toMatchObject({ key: 'failing', label: 'Check failing' });
  expect(r.fail.why).toContain('exit 1');
  expect(r.broken.label).toBe('Check broken');
  expect(r.passing).toMatchObject({ key: 'passing', label: 'Passing' });

  await page.evaluate("switchView('scheduler')");
  await expect.poll(() => page.evaluate("(schedules || []).some(s => s.id === 'SCHED-pf')")).toBe(true);
  await page.evaluate("openSchedDetail('SCHED-pf')");
  await expect(page.locator('#sched-detail-status')).toContainText('Check failing');
  const body = page.locator('#sched-detail-body');
  await expect(body).toContainText('no report yet');
  await expect(body).toContainText('test -s /tmp/report.md');
});

// The Mac health check's pass check (scripts/mac-health-pass.sh) writes its
// first line for people; a passing check shows it and a green Passing pill.
test('a passing check reads as Passing, leads with what it found, and keeps the command one tap away', async ({ page }) => {
  const row = { id: 'SCHED-pp', title: 'Mac system health check', kind: 'shell', command: 'tick', enabled: 1,
    schedule_expr: 'every 1h', sched_type: 'recurring', session: '',
    pass_check: 'bash scripts/mac-health-pass.sh', pass_state: 'pass', pass_attempts: 0,
    pass_checked_at: Math.floor(Date.now() / 1000),
    pass_detail: 'Ran 12:57 and cleaned up. Still over a floor, already escalated to its owner: disk 125G free (floor 150G)\nmac-cleanup: done elapsed=398s' };
  await page.route(/\/api\/schedules(\?.*)?$/, (route) => route.request().method() === 'GET'
    ? route.fulfill({ json: [row] }) : route.continue());
  await settle(page);
  await page.evaluate("switchView('scheduler')");
  await expect.poll(() => page.evaluate("(schedules || []).some(s => s.id === 'SCHED-pp')")).toBe(true);
  await expect(page.locator('.sx-pill-passing').first()).toHaveText('Passing');
  await page.evaluate("openSchedDetail('SCHED-pp')");
  await expect(page.locator('#sched-detail-status')).toContainText('Passing');
  const body = page.locator('#sched-detail-body');
  await expect(body.locator('.sx-pass-found')).toHaveText(/Ran 12:57 and cleaned up\. Still over a floor/);
  await expect(body.locator('.sx-pass-how code')).toBeHidden();
  await body.locator('.sx-pass-how summary').click();
  await expect(body.locator('.sx-pass-how code')).toHaveText('bash scripts/mac-health-pass.sh');
});
