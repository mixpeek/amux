// A goal check is a schedule (primitive integrity audit 2026-11-26, step 5):
// AMUX_GOAL_CHECK creates a kind=shell schedule with managed_by=goal-check
// (runtime_jobs/goal_check.rs). The Schedules tab names the key that keeps it
// in step and why it is paused, and the goal tile says which schedule runs the
// check. The harness server runs no goal keeper, so the list is stubbed in the
// server's own shape.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

async function settle(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openSchedDetail === 'function'
    && typeof (window as any)._goalTile === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
}

const base = { kind: 'shell', sched_type: 'recurring', schedule_expr: 'every 10m', exit_actions: '{"1":"log"}',
  managed_by: 'goal-check', created_by: 'policy:AMUX_GOAL_CHECK' };
const running = { ...base, id: 'SCHED-gc1', title: 'Goal check: lane-a', session: 'lane-a', enabled: 1,
  command: 'export AMUX_GOAL_CHECK_RUN=1\n./check.sh' };
const stopped = { ...base, id: 'SCHED-gc2', title: 'Goal check: lane-b', session: 'lane-b', enabled: 0,
  disabled_reason: 'goal_check_worker_stopped', command: 'export AMUX_GOAL_CHECK_RUN=1\n./check.sh' };
const plain = { id: 'SCHED-plain', title: 'nightly report', kind: 'shell', sched_type: 'recurring',
  schedule_expr: 'daily at 09:00', session: '', enabled: 1, command: 'make report' };

test('a goal-check schedule names its key, says why it is paused, and the goal tile names the schedule', async ({ page }) => {
  await page.route(/\/api\/schedules(\?.*)?$/, (route) => route.request().method() === 'GET'
    ? route.fulfill({ json: [running, stopped, plain] }) : route.continue());
  await settle(page);
  await page.evaluate("switchView('scheduler')");
  await expect.poll(() => page.evaluate("(schedules || []).filter(s => s.managed_by).length")).toBe(2);

  // The list: the key on each managed row, nothing on an ordinary one.
  const row = (id: string) => page.locator(`.sx-row[data-sched-id="${id}"]`);
  await expect(row('SCHED-gc1').locator('.sx-managed')).toHaveText('AMUX_GOAL_CHECK');
  await expect(row('SCHED-gc2').locator('.sx-managed')).toHaveText('AMUX_GOAL_CHECK');
  await expect(row('SCHED-plain').locator('.sx-managed')).toHaveCount(0);
  await expect(row('SCHED-gc2')).toHaveAttribute('data-status', 'paused');
  await expect(row('SCHED-gc2').locator('.sx-pill')).toHaveAttribute('title', /while its worker is stopped/);

  // The detail: where to change it.
  await page.evaluate("openSchedDetail('SCHED-gc1')");
  const body = page.locator('#sched-detail-body');
  await expect(body).toContainText('Managed by');
  await expect(body).toContainText("Change the check or AMUX_GOAL_CHECK_INTERVAL_S in the worker's Scope tab");

  // Re-render: the badge survives a refresh of the list.
  await page.evaluate("closeSchedDetail(); typeof renderScheduler === 'function' && renderScheduler()");
  await expect(row('SCHED-gc1').locator('.sx-managed')).toHaveText('AMUX_GOAL_CHECK');

  // Searching by the key finds the managed rows only.
  const hay = await page.evaluate(([a, b]) => (globalThis as any).eval(`[_sxSearchHay(${JSON.stringify(a)}), _sxSearchHay(${JSON.stringify(b)})]`), [running, plain]);
  expect(hay[0]).toContain('amux_goal_check');
  expect(hay[1]).not.toContain('amux_goal_check');

  // The goal tile names the schedule, and a stopped worker's pause.
  const tip = await page.evaluate(() => {
    const t = (globalThis as any)._goalTile({ active: true, state: 'unmet', checker: { configured: true, measured: true, met: false,
      done: 3, total: 9, schedule: { id: 'SCHED-gc2', enabled: false, disabled_reason: 'goal_check_worker_stopped' } } });
    const el = document.createElement('div');
    el.innerHTML = t;
    return el.firstElementChild?.getAttribute('title') || '';
  });
  expect(tip).toContain('Runs as schedule SCHED-gc2 (paused while the worker is stopped)');
  const tipLive = await page.evaluate(() => {
    const t = (globalThis as any)._goalTile({ active: true, state: 'unmet', checker: { configured: true, measured: true, met: false,
      schedule: { id: 'SCHED-gc1', enabled: true } } });
    const el = document.createElement('div');
    el.innerHTML = t;
    return el.firstElementChild?.getAttribute('title') || '';
  });
  expect(tipLive).toContain('Runs as schedule SCHED-gc1; its run history is in the Schedules tab');

  // Phone width: the badge does not push the row past the screen.
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow).toBeLessThanOrEqual(1);
});
