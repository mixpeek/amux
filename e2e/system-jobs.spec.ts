// The System section of the Scheduler tab (AMUX-2703) — amux's OWN background
// jobs, rendered below the user's schedules.
//
// Why this spec exists and what it must actually prove:
//
// On 2026-08-10 three internal loops were dead or had never been spawned, for
// hours, and nothing anywhere said so — a loop that is not running and a loop
// with nothing to do produce byte-identical evidence. So the section is only
// worth having if a BROKEN job is impossible to read as a healthy one. That is
// the assertion this file is built around, and it is checked twice:
//
//   1. against REAL server data (every job the server actually spawned, with
//      the tick timings the spawner recorded), and
//   2. against a stubbed payload, because in a healthy server there is by
//      definition no stalled job to photograph. The staleness VERDICT itself is
//      unit-tested with negative controls in runtime_jobs::registry; what the
//      stub proves is the thing a unit test cannot — that the verdict reaches
//      the screen, and reaches it distinctly.
//
// The stub is deliberately not the only evidence, and the real-data test is
// deliberately not asked to produce a red row it has no honest way to produce.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

// BLOCK THE SERVICE WORKER — this file stubs /api/system-jobs with page.route,
// and a registered SW silently defeats that (AF-46, measured 2026-08-13).
//
// sw.js declines to answer /api/* ("network only"), so it looks irrelevant
// here. It is not: the request still passes THROUGH the worker's fetch handler,
// and page.route does not see requests that originate there. The stall test did
// not error — it rendered the REAL job list and compared it to the stub, so the
// failure read as "the stalled-row styling is broken under WebKit" when the
// truth was that the stub never applied. Received ["accountability-nudge",
// "autofix", ...] against the four stubbed ids is what gave it away.
//
// The SW also reloads the page on `controllerchange` (app.js:24253) the moment
// it takes control, which on a FRESH profile lands mid-test and kills the
// execution context. That is real product behaviour and sw-fail-bar.spec.ts is
// where it belongs; here it is just a reload nobody asked for.
test.use({ serviceWorkers: 'block' });

const SHOT = 'test-results/system-jobs';

async function openScheduler(page: Page) {
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function'", null, { timeout: 20_000 });
  await page.evaluate("switchView('scheduler')");
  await page.waitForSelector('#system-jobs-list .sysjob', { timeout: 20_000 });
}

test('system jobs render with real data, separated from user schedules', async ({ page }, info) => {
  await openScheduler(page);

  // The section is labelled as amux's own machinery, not as more schedules.
  await expect(page.locator('.sysjob-head-title')).toHaveText('System');
  await expect(page.locator('.sysjob-head-sub')).toContainText('background jobs');

  // It sits BELOW the user's schedule list, which is the layout the request
  // named. Compared by document order rather than by pixels so it holds at
  // both viewports.
  const order = await page.evaluate(`(() => {
    const a = document.getElementById('scheduler-list');
    const b = document.querySelector('.sysjob-section');
    return a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING ? 'after' : 'before';
  })()`);
  expect(order).toBe('after');

  // Real data: the payload the page rendered came from the server, and every
  // row carries the fields that make the section useful.
  const jobs = await page.evaluate('JSON.stringify(_systemJobs)').then((s) => JSON.parse(s as string));
  expect(Array.isArray(jobs.jobs)).toBeTruthy();
  expect(jobs.jobs.length).toBeGreaterThanOrEqual(10);

  // The loops this fleet cannot run without must all be present AND ticking.
  // Naming them explicitly (rather than asserting "no job is stalled") is the
  // point: a payload that silently lost a job would pass the vaguer check.
  for (const id of ['scheduler', 'steer-deliver', 'board-drive', 'invariants-monitor',
                    'orchestrator-runtime', 'session-bootstrap', 'terminal-scan']) {
    const j = jobs.jobs.find((x: any) => x.id === id);
    expect(j, `${id} missing from /api/system-jobs`).toBeTruthy();
    expect(j.spawned, `${id} is documented but nothing spawned it`).toBeTruthy();
    expect(j.purpose, `${id} has no purpose text`).toBeTruthy();
  }

  // No mutation affordances: these are machinery, not user data.
  // Asserted on BUTTONS, not on the word: `getByText('Delete')` matched the
  // section's own note ("you cannot edit or delete them"), which would have
  // failed forever on prose that is doing exactly the right thing.
  const sysSection = page.locator('.sysjob-section');
  await expect(sysSection.locator('button')).toHaveCount(0);
  await expect(sysSection.locator('.sched-action-btn')).toHaveCount(0);

  // Exactly one live switch — the autofix pref, which the server re-reads on
  // every tick. Env vars render as readouts; a checkbox over one would claim
  // an effect it cannot have.
  await expect(sysSection.locator('.sysjob-toggle input[type=checkbox]')).toHaveCount(1);
  await expect(sysSection.locator('.sysjob-toggle span')).toHaveText('autofix_enabled');
  expect(await sysSection.locator('.sysjob-env').count()).toBeGreaterThan(0);

  await page.screenshot({ path: `${SHOT}-real-${info.project.name}.png`, fullPage: true });
});

test('a stalled job is visually distinct from a healthy one', async ({ page }, info) => {
  // A healthy server has no stalled job to photograph, and manufacturing one
  // by breaking a real loop would be a destructive test. So the SHAPE of the
  // payload is stubbed while the rendering path stays entirely real: same
  // renderer, same CSS, same page.
  await page.route('**/api/system-jobs', async (route) => {
    const now = Date.now() / 1000;
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        now,
        count: 4,
        unhealthy: 3,
        stall_rule: 'stub',
        jobs: [
          { id: 'board-drive', name: 'Board drive', purpose: 'Assigns cards to idle lanes.',
            documented: true, kind: 'periodic', interval_s: 60, stale_after_s: 165,
            spawned: true, ticks: 812, last_tick_at: now - 4, last_tick_age_s: 4,
            last_tick_ms: 31.2, in_flight: false, instrumented: true, status: 'ok',
            env: [], pref: null, detail: '/api/debug/board-drive',
            outcome: '1 assigned, 0 nudged across 12 lane(s)' },
          { id: 'pipe-reconcile', name: 'Session log piping', purpose: 'Re-attaches tmux pipe-pane.',
            documented: true, kind: 'loop', interval_s: 60, stale_after_s: 165,
            spawned: true, ticks: 3, last_tick_at: now - 4210, last_tick_age_s: 4210,
            in_flight: false, instrumented: true, status: 'stalled',
            env: [], pref: null, detail: '/api/debug/logs', outcome: null },
          { id: 'scheduler', name: 'Schedule firing', purpose: 'Fires every due user schedule.',
            documented: true, kind: null, interval_s: null, stale_after_s: null,
            spawned: false, ticks: 0, last_tick_at: null, instrumented: false,
            status: 'not_spawned',
            env: [{ kind: 'env', var: 'AMUX_RS_SCHEDULER', value: '1', effect: 'fire for real',
                    off_value: null, off_now: false, editable: false }],
            pref: null, detail: null, outcome: null },
          { id: 'autofix', name: 'Autofix', purpose: 'Files a board card per distinct fault.',
            documented: true, kind: 'periodic', interval_s: 120, stale_after_s: 315,
            spawned: true, ticks: 40, last_tick_at: now - 900, last_tick_age_s: 900,
            in_flight: false, instrumented: true, status: 'dead',
            env: [], pref: { kind: 'pref', key: 'autofix_enabled', value: '1',
                             effect: 'off skips only the card', editable: true, on: true },
            detail: '/api/debug/autofix', outcome: '0 filed, 2 suppressed' },
        ],
      }),
    });
  });

  await openScheduler(page);

  // BROKEN FIRST: the three unhealthy jobs sort above the healthy one, so a
  // dead loop is never something you have to scroll for.
  const ids = await page.$$eval('#system-jobs-list .sysjob .sysjob-id', (els) =>
    els.map((e) => e.textContent));
  expect(ids.slice(0, 3).sort()).toEqual(['autofix', 'pipe-reconcile', 'scheduler']);
  expect(ids[3]).toBe('board-drive');

  // The words are unambiguous and the ages are real numbers, not "<1m".
  const stalled = page.locator('.sysjob', { has: page.locator('.sysjob-id', { hasText: 'pipe-reconcile' }) });
  await expect(stalled.locator('.sysjob-status')).toHaveText('STALLED');
  await expect(stalled).toContainText('1h 10m ago');
  await expect(stalled).toContainText('budget');
  await expect(page.locator('.sysjob', { has: page.locator('.sysjob-id', { hasText: 'scheduler' }) })
    .locator('.sysjob-status')).toHaveText('NOT RUNNING');
  await expect(page.locator('.sysjob', { has: page.locator('.sysjob-id', { hasText: 'autofix' }) })
    .locator('.sysjob-status')).toHaveText('DEAD');
  await expect(page.locator('#system-jobs-count')).toContainText('3 need');

  // COLOUR IS NOT THE ONLY CHANNEL. A greyscale screenshot, a colour-blind
  // reader and a phone in sunlight all have to distinguish these, so the
  // difference is asserted on class + text, and the background tint is
  // asserted as an ADDITIONAL signal rather than the only one.
  await expect(stalled).toHaveClass(/\bbad\b/);
  const bg = await stalled.evaluate((e) => getComputedStyle(e).backgroundColor);
  const okBg = await page.locator('.sysjob', { has: page.locator('.sysjob-id', { hasText: 'board-drive' }) })
    .evaluate((e) => getComputedStyle(e).backgroundColor);
  expect(bg).not.toBe(okBg);

  await page.screenshot({ path: `${SHOT}-stalled-${info.project.name}.png`, fullPage: true });
});

test('the section fits a phone without horizontal overflow', async ({ page }, info) => {
  // amux is mobile-first; 390px is the iPhone the PWA actually runs on.
  await page.setViewportSize({ width: 390, height: 844 });
  await openScheduler(page);
  const overflow = await page.evaluate(`(() => {
    const bad = [];
    for (const el of document.querySelectorAll('#system-jobs-list .sysjob, .sysjob-section')) {
      const r = el.getBoundingClientRect();
      if (r.right > window.innerWidth + 1 || r.left < -1) bad.push(el.className + ' ' + r.left + '..' + r.right);
    }
    return bad;
  })()`);
  expect(overflow).toEqual([]);
  expect(await page.evaluate('document.documentElement.scrollWidth <= window.innerWidth + 1')).toBeTruthy();
  await page.screenshot({ path: `${SHOT}-390-${info.project.name}.png`, fullPage: true });
});
