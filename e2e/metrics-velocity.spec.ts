// Metrics > Velocity (docs/design/worker-velocity.md step 7): the fleet ranked
// by wasted hours, a worker's waste by who can fix it, and when its open cards
// land. The fixtures below are the API's shapes (api/velocity.rs); the last
// test runs against the real server's empty history.
import { test, expect, Page } from './fixtures';

const fleet = {
  window_days: 30, measured: true, n_considered: 120,
  workers: [
    { worker: 'tubescience-parity', attempts: 80, finished_cards: 12, finished_per_day: 0.4, finished_share_of_time: 31, attempts_per_card: 3.1, wasted_hours: 41.2, top_bottleneck: 'parked_waiting' },
    { worker: 'mxp-gs12', attempts: 40, finished_cards: 20, finished_per_day: 0.67, finished_share_of_time: 58, attempts_per_card: 1.6, wasted_hours: 25.3, top_bottleneck: 'review' },
  ],
};
const detail = {
  worker: 'tubescience-parity', window_days: 30, attempts: 80, cards: 26, finished_cards: 12, finished_per_day: 0.4,
  attempts_per_card: 3.1, finished_share_of_time: 31, hours: { total: 60, finished: 18.8 },
  per_finished_card: { minutes: { p25: 20, p50: 45, p75: 90, n: 12 }, tokens: { p50: 180000 } },
  waste: [
    { kind: 'review', owner: 'owner', attempts: 4, hours: 3.1, share_of_time: 5, what_it_means: 'Waiting for the owner to review.' },
    { kind: 'parked_waiting', owner: 'dependency', attempts: 50, hours: 23.7, share_of_time: 40, what_it_means: 'Parked on another worker.' },
  ],
  waited_on: [{ waiting: 'after the b13b roll serves on the TS shard', attempts: 50, hours: 23.7 }],
};
const projection = {
  worker: 'tubescience-parity', measured: true, n_considered: 2, include_backlog: true,
  cards: [
    { card: 'TP-201', title: 'move 4a', status: 'doing', minutes_p50: 45, minutes_p75: 90, blocked_by: [], over_estimate: { elapsed_minutes: 140, expected_minutes_p75: 90 } },
    { card: 'TP-202', title: 'move 1', status: 'todo', minutes_p50: 45, minutes_p75: 90, blocked_by: ['MG-2292'], over_estimate: null },
  ],
  work_minutes: { p50: 120, p75: 240 }, calibration: { actual_over_estimate: 1.3, cards: 12 },
  pace: { days_per_finished_card: 2.5 }, finish_by: { p50: '2026-11-04T10:00:00Z', p75: '2026-11-06T10:00:00Z' },
  blocked_by_outside: ['MG-2292'],
};

async function openVelocity(page: Page, mock = true) {
  if (mock) {
    await page.route(/\/api\/velocity\/[^/?]+\/projection/, r => r.fulfill({ json: projection }));
    await page.route(/\/api\/velocity\/[^/?]+(\?|$)/, r => r.fulfill({ json: detail }));
    await page.route(/\/api\/velocity(\?|$)/, r => r.fulfill({ json: fleet }));
  }
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function' && typeof _metricsSetMode === 'function'");
  await page.evaluate("switchView('metrics')");
  // A phone opens Metrics with the worker sidebar over the content; close it as a user would.
  await page.evaluate("_metricsSidebarOpen = false; _metricsApplySidebarState()");
  await page.locator('#metricsmode-velocity').click();
  await expect(page.locator('#metricsmode-velocity')).toHaveClass(/active/);
}

test('the fleet ranks by wasted hours and a worker opens its waste and projection', async ({ page }) => {
  await openVelocity(page);
  const rows = page.locator('.velocity-row');
  await expect(rows).toHaveCount(2);
  await expect(rows.first()).toContainText('tubescience-parity');
  await expect(rows.first()).toContainText('41.2 h wasted');
  await rows.first().click();
  await expect(page.locator('#velocity-summary')).toContainText('Finished 12 of 26 cards');
  // Waste is listed biggest first, each with who can fix it.
  await expect(page.locator('.velocity-waste').first()).toHaveAttribute('data-kind', 'parked_waiting');
  await expect(page.locator('.velocity-waste').first()).toContainText('(dependency)');
  await expect(page.getByText('after the b13b roll serves on the TS shard')).toBeVisible();
  await expect(page.locator('#velocity-finish')).toContainText('by 2026-11-04');
  await expect(page.locator('#velocity-finish')).toContainText('Waits on MG-2292');
  await expect(page.locator('.velocity-card[data-card="TP-201"]')).toContainText('over estimate: 140 of 90 min');
  await expect(page.locator('.velocity-card[data-card="TP-202"]')).not.toContainText('over estimate');
  // Back to the fleet; a re-entry into the mode keeps the open worker.
  await page.evaluate("_metricsSetMode('system'); _metricsSetMode('velocity')");
  await expect(page.locator('#velocity-summary')).toBeVisible();
  await page.locator('#velocity-back').click();
  await expect(page.locator('.velocity-row')).toHaveCount(2);
});

test('phone: the Metrics modes stay one row and scroll sideways', async ({ page }) => {
  test.skip(page.viewportSize()!.width > 600, 'phone layout');
  await openVelocity(page);
  const bar = (await page.locator('.metrics-modebar').boundingBox())!;
  expect(bar.height, 'one row').toBeLessThanOrEqual(56);
  for (const id of ['system', 'host', 'resources', 'velocity', 'disk']) {
    const b = (await page.locator('#metricsmode-' + id).boundingBox())!;
    const mid = b.y + b.height / 2;
    expect(mid, id + ' in the one row').toBeGreaterThanOrEqual(bar.y);
    expect(mid, id + ' in the one row').toBeLessThanOrEqual(bar.y + bar.height);
    expect(b.height, id + ' touch target').toBeGreaterThanOrEqual(44);
  }
  await page.locator('.velocity-row').first().click();
  const doc = await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1);
  expect(doc, 'the worker view does not overflow the phone').toBe(true);
});

test('the real server with no history says so', async ({ page }) => {
  await openVelocity(page, false);
  const res = await page.evaluate(() => fetch(API + '/api/velocity?days=30').then(async r => [r.status, await r.text()]));
  expect(res[0], String(res[1])).toBe(200);
  await expect(page.locator('#velocity-content')).toContainText(/No worker has enough history yet|most wasted hours first/);
  await expect(page.locator('#velocity-content')).not.toContainText('did not load');
});
