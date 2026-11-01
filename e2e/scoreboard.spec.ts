// The Scoreboard (Z14). The stubbed payload holds one number of each kind and one
// hole of each kind, so a renderer that turns a missing value into a zero or a
// blank fails here. One test reads the real route, so a drift between server and
// page fails too.
import { test, expect, Page } from './fixtures';

const AT = 1793540000;
const STUB = {
  measured: true, n_considered: 3, generated_at: AT, day_start: AT - 30000, retention_days: 14, excluded: 2,
  workers: [
    { name: 'goal-worker', running: true, status: 'active', isolated: false, spend_today_usd: 12.5, needsyou: 0,
      waiting_on: { measured: true, kind: 'working', detail: 'nothing, it is working' },
      goal: { configured: true, measured: true,
        now: { measured: true, done: 44, total: 56, at: AT }, hour_ago: { measured: true, done: 41, total: 56, at: AT - 3600 },
        today_start: { measured: true, done: 30, total: 56, at: AT - 30000 }, peak: { measured: true, done: 47, total: 56, at: AT - 900 },
        rate_1h: { measured: true, per_h: 3 }, rate_today: { measured: true, per_h: 1.68 } } },
    { name: 'new-checker', running: true, status: 'idle', isolated: true, spend_today_usd: null, needsyou: 2,
      waiting_on: { measured: true, kind: 'owner', detail: 'you: 2 needs-you card(s)' },
      goal: { configured: true, measured: false, now: { measured: false, why_unmeasured: 'checker exited 2' },
        hour_ago: { measured: false, why_unmeasured: 'no reading retained yet' }, today_start: { measured: false, why_unmeasured: 'no reading retained yet' },
        peak: { measured: false, why_unmeasured: 'no reading retained yet' }, rate_1h: { measured: false, why_unmeasured: 'one of the two readings is unmeasured' } } },
    { name: 'plain-worker', running: false, status: 'idle', isolated: false, spend_today_usd: 0, needsyou: 0,
      waiting_on: { measured: false, kind: 'unknown', why_unmeasured: 'only a screen guess exists' },
      goal: { configured: false, measured: false, why_unmeasured: 'no goal checker: this worker has no AMUX_GOAL_CHECK' } },
  ],
  rates: { measured: true, n_considered: 9, cards_done_1h: 0, cards_done_24h: 9, spend_per_h_usd: 1.5 },
  ci: { measured: true, n_considered: 2, tip_state: 'green', behind: 0, summary: 'Up to date', workflows: [{ name: 'rust.yml', last_green: 'abc', commits_since_green: 3 }],
    pass_rate: { measured: false, why_unmeasured: 'no CI run history is retained' } },
  spend: { measured: false, n_considered: 0, why_unmeasured: 'the token ledger has no rows' },
  host: { measured: true, n_considered: 2, level: 'ok', reasons: [], load1: 20, ncpu: 28, mem_percent: 97.9, mem_pressure: 'warn', disk_free_gb: 366.1 },
  needsyou: { measured: true, n_considered: 30, total: 30, overdue: 1, shown: 1, hidden: 29, queue: [{ id: 'AMUX-1', title: 'Approve the thing', session: 'goal-worker', age_days: 3.5 }] },
  unmeasured: [
    { field: 'spend', why: 'the token ledger has no rows', workers: [] },
    { field: 'goal', why: 'no goal checker: this worker has no AMUX_GOAL_CHECK', workers: ['plain-worker'] },
  ],
};

async function open(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function' && typeof _sbLoad === 'function'");
}
async function openStubbed(page: Page, body: unknown = STUB) {
  await page.route('**/api/debug/scoreboard', r => r.fulfill({ json: body }));
  await open(page);
  await page.evaluate("switchView('scoreboard')");
  await expect(page.locator('#sb-cards')).toBeVisible();
}
const cell = (page: Page, worker: string, n: number) => page.locator(`#sb-workers tr[data-worker="${worker}"] td`).nth(n);

test('real route: the page renders what the server answers', async ({ page }) => {
  await open(page);
  const res = await page.evaluate(async () => {
    const r = await fetch('/api/debug/scoreboard');
    return { status: r.status, text: await r.text() };
  });
  expect(res.status, res.text.slice(0, 400)).toBe(200);
  const d = JSON.parse(res.text);
  expect(d.measured, res.text.slice(0, 400)).toBe(true);
  for (const k of ['rates', 'ci', 'spend', 'host', 'needsyou']) {
    expect(typeof d[k].measured, k).toBe('boolean');
    expect(typeof d[k].n_considered, k).toBe('number');
    if (!d[k].measured) expect(d[k].why_unmeasured, k).toBeTruthy();
  }
  await page.evaluate("switchView('scoreboard')");
  await expect(page.locator('#sb-cards .metrics-card').first()).toBeVisible();
  await expect(page.locator('#sb-updated')).toContainText('Updated');
  await expect(page.locator('#sb-workers tbody tr')).toHaveCount(d.workers.length);
});

test('the tab bar reaches the page on every width', async ({ page }) => {
  await page.route('**/api/debug/scoreboard', r => r.fulfill({ json: STUB }));
  await open(page);
  await page.locator('#tab-scoreboard').click();
  await expect(page.locator('#sb-cards')).toBeVisible();
  await expect(page.locator('#tab-scoreboard')).toHaveClass(/active/);
});

test('numbers show, and a hole reads unmeasured with its reason', async ({ page }) => {
  await openStubbed(page);
  await expect(cell(page, 'goal-worker', 0)).toHaveText('44/56');
  await expect(cell(page, 'goal-worker', 1)).toHaveText('41/56');
  await expect(cell(page, 'goal-worker', 2)).toHaveText('30/56');
  await expect(cell(page, 'goal-worker', 3)).toHaveText('47/56');
  await expect(cell(page, 'goal-worker', 4)).toHaveText('+3/h');
  await expect(cell(page, 'goal-worker', 5)).toHaveText('active');
  await expect(cell(page, 'goal-worker', 6)).toHaveText('nothing, it is working');
  await expect(cell(page, 'goal-worker', 7)).toHaveText('$12.50');
  for (const n of [0, 1, 2, 3, 4]) await expect(cell(page, 'new-checker', n)).toHaveText('unmeasured');
  await expect(cell(page, 'new-checker', 0).locator('.sb-unmeasured')).toHaveAttribute('title', 'checker exited 2');
  await expect(cell(page, 'new-checker', 7)).toHaveText('unmeasured');
  // A real zero stays a zero, beside the holes.
  await expect(cell(page, 'plain-worker', 3)).toHaveText('$0.00');
  await expect(cell(page, 'plain-worker', 2)).toHaveText('unmeasured');
  const cards = page.locator('#sb-cards .metrics-card');
  await expect(cards.filter({ hasText: 'Cards done' }).locator('.metrics-card-value')).toHaveText('0');
  await expect(cards.filter({ hasText: 'Spend today' }).locator('.metrics-card-value')).toHaveText('unmeasured');
  await expect(cards.filter({ hasText: 'CI pass rate' }).locator('.metrics-card-value')).toHaveText('unmeasured');
  await expect(cards.filter({ hasText: 'Needs you' }).locator('.metrics-card-value')).toHaveText('30');
  await expect(page.locator('#sb-needsyou')).toContainText('Approve the thing');
  await expect(page.locator('.sb-why-list')).toContainText('the token ledger has no rows');
  await expect(page.locator('.sb-why-list')).toContainText('plain-worker');
});

test('a refresh repaints, a failed one keeps the numbers and says so, and leaving stops it', async ({ page }) => {
  await openStubbed(page);
  let body: any = JSON.parse(JSON.stringify(STUB));
  body.workers[0].goal.now.done = 45;
  await page.unroute('**/api/debug/scoreboard');
  let hits = 0;
  let fail = false;
  await page.route('**/api/debug/scoreboard', r => { hits++; return fail ? r.fulfill({ status: 500, json: {} }) : r.fulfill({ json: body }); });
  await page.evaluate("_sbLoad()");
  await expect(cell(page, 'goal-worker', 0)).toHaveText('45/56');
  fail = true;
  await page.evaluate("_sbLoad()");
  await expect(page.locator('#sb-updated')).toContainText('Refresh failed');
  await expect(cell(page, 'goal-worker', 0)).toHaveText('45/56');
  // The render path both SSE and the polling fallback run reaches the page.
  fail = false;
  body = JSON.parse(JSON.stringify(body));
  body.workers[0].goal.now.done = 46;
  await page.evaluate("_sbLastAt = 0; render()");
  await expect(cell(page, 'goal-worker', 0)).toHaveText('46/56');
  await page.evaluate("switchView('board')");
  expect(await page.evaluate('_sbTimer')).toBeNull();
  const before = hits;
  await page.evaluate("_sbLastAt = 0; render(); _sbLoad()");
  await page.waitForTimeout(300);
  expect(hits, 'off the view, nothing is fetched').toBe(before);
});

test('phone: one control row, the table scrolls inside itself, the page does not', async ({ page }) => {
  const vp = page.viewportSize()!;
  test.skip(vp.width > 600, 'phone layout');
  await openStubbed(page);
  const bar = (await page.locator('#sb-controls').boundingBox())!;
  expect(bar.height, 'the control bar is one row').toBeLessThanOrEqual(56);
  for (const sel of ['#sb-refresh', '#sb-updated']) {
    const b = (await page.locator(sel).boundingBox())!;
    const mid = b.y + b.height / 2;
    expect(mid, sel).toBeGreaterThanOrEqual(bar.y);
    expect(mid, sel).toBeLessThanOrEqual(bar.y + bar.height);
  }
  const refresh = (await page.locator('#sb-refresh').boundingBox())!;
  expect(refresh.height).toBeGreaterThanOrEqual(44);
  expect(refresh.width).toBeGreaterThanOrEqual(44);
  const view = page.locator('#scoreboard-view');
  expect(await view.evaluate(e => e.scrollWidth <= e.clientWidth), 'no sideways page').toBe(true);
  const wrap = page.locator('#sb-workers').locator('xpath=..');
  expect(await wrap.evaluate(e => e.scrollWidth > e.clientWidth), 'the table scrolls itself').toBe(true);
  // The worker column stays on screen while the table moves.
  await wrap.evaluate(e => { e.scrollLeft = 200; });
  const name = (await page.locator('#sb-workers tr[data-worker="goal-worker"] th').boundingBox())!;
  expect(name.x).toBeLessThan(40);
  await page.screenshot({ path: test.info().outputPath('scoreboard-375.png'), fullPage: false });
  // A refresh keeps the sideways scroll.
  await page.evaluate("_sbLoad()");
  await expect.poll(() => wrap.evaluate(e => e.scrollLeft)).toBe(200);
});
