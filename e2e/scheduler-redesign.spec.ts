// Scheduler page redesign (2026-10-11): list, details, create.
//
// Runs against the throwaway-AMUX_HOME server from playwright.config.ts, never
// the live one. Every schedule here is created inside that harness server and
// carries a per-test unique prefix, so parallel spec files cannot collide.
// All three projects (desktop, 375px mobile, iOS Safari) run every test.
import { test, expect } from './fixtures';
import type { Page, APIRequestContext } from '@playwright/test';

// A window.confirm anywhere in these flows is a failure: deletes must use the
// shared confirmation dialog. Recorded here and asserted empty in afterEach.
const nativeDialogs = new WeakMap<Page, string[]>();
test.afterEach(async ({ page }) => {
  expect(nativeDialogs.get(page) || [], 'no native confirm/alert dialogs').toEqual([]);
});
async function settle(page: Page): Promise<void> {
  const seen: string[] = [];
  nativeDialogs.set(page, seen);
  page.on('dialog', async (d) => { seen.push(d.type() + ': ' + d.message()); await d.dismiss(); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).apiCall === 'function'
    && typeof (window as any).openSchedDetail === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
}
async function token(page: Page): Promise<string> {
  return page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
}
async function create(request: APIRequestContext, tok: string, data: Record<string, unknown>): Promise<string> {
  const r = await request.post('/api/schedules', {
    headers: { Authorization: `Bearer ${tok}`, 'Content-Type': 'application/json', 'X-Amux-Session': 'e2e-scheduler' },
    data,
  });
  expect(r.status(), await r.text()).toBe(201);
  return (await r.json()).id;
}
async function getOne(request: APIRequestContext, tok: string, id: string) {
  const r = await request.get('/api/schedules/' + id, { headers: { Authorization: `Bearer ${tok}` } });
  return { status: r.status(), body: r.status() === 200 ? await r.json() : null };
}
async function listAll(request: APIRequestContext, tok: string): Promise<any[]> {
  const r = await request.get('/api/schedules', { headers: { Authorization: `Bearer ${tok}` } });
  return r.json();
}
async function openScheduler(page: Page, search?: string): Promise<void> {
  await page.evaluate("switchView('scheduler')");
  await expect(page.locator('#scheduler-view')).toBeVisible();
  if (search != null) await page.fill('#sched-search', search);
}
async function expectNoOverflow(page: Page): Promise<void> {
  const o = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(o, 'page must not overflow horizontally').toBeLessThanOrEqual(1);
}
// A live session name for the worker picker. The harness has no workers, so
// one is added to the client's session list (the picker reads it), exactly as
// a fetched session would appear.
async function addWorker(page: Page, name: string): Promise<void> {
  await page.evaluate(`sessions.some(s => s.name === ${JSON.stringify(name)}) || sessions.push({ name: ${JSON.stringify(name)}, status: 'idle' })`);
}

test('list: groups come from the server cadence class, pills and filters narrow it, numbers are measured', async ({ page, request }, info) => {
  await settle(page);
  const tok = await token(page);
  const p = `sx-list-${Date.now()}-`;
  const ids = {
    loop: await create(request, tok, { title: p + 'loop', session: 'e2e-w1', command: 'check', schedule_expr: 'every 10m' }),
    routine: await create(request, tok, { title: p + 'routine', kind: 'shell', command: 'true', schedule_expr: 'daily at 09:00' }),
    weekly: await create(request, tok, { title: p + 'weekly', session: 'e2e-w2', command: 'audit', schedule_expr: 'weekly on monday at 06:00' }),
    inactive: await create(request, tok, { title: p + 'inactive', session: 'e2e-w1', command: 'x', schedule_expr: 'every 2h', enabled: 0 }),
  };
  // The server publishes the class and the measured counts this page renders.
  const rows = await listAll(request, tok);
  const loopRow = rows.find((r: any) => r.id === ids.loop);
  expect(loopRow.cadence_class).toBe('loop');
  expect(loopRow.run_stats_24h).toMatchObject({ runs: 0, failures: 0, measured: true });
  expect(typeof loopRow.run_stats_24h.n_considered).toBe('number');

  await openScheduler(page, p);
  for (const [group, id] of Object.entries(ids)) {
    await expect(page.locator(`.sx-group[data-group="${group}"] [data-sched-id="${id}"]`), `${group} row in its group`).toBeVisible();
  }
  // Type icon tells shell from a worker prompt.
  await expect(page.locator(`[data-sched-id="${ids.routine}"] .sx-ico-shell`)).toBeVisible();
  await expect(page.locator(`[data-sched-id="${ids.loop}"] .sx-ico-tmux`)).toBeVisible();
  // Frequency labels.
  await expect(page.locator(`[data-sched-id="${ids.loop}"] .sx-freq`)).toContainText('Every 10m');
  await expect(page.locator(`[data-sched-id="${ids.weekly}"] .sx-freq`)).toContainText('Mon');
  // Status pills.
  await expect(page.locator(`[data-sched-id="${ids.loop}"] .sx-pill`)).toHaveText('Scheduled');
  await expect(page.locator(`[data-sched-id="${ids.inactive}"] .sx-pill`)).toHaveText('Paused');
  // Pill counts agree with the rows shown under the search.
  await expect(page.locator('.sx-pill-btn[data-group="all"] .sx-pill-n')).toHaveText('4');
  await expect(page.locator('.sx-pill-btn[data-group="inactive"] .sx-pill-n')).toHaveText('1');
  // Pill narrows to one group.
  await page.locator('.sx-pill-btn[data-group="inactive"]').click();
  await expect(page.locator('.sx-group')).toHaveCount(1);
  await expect(page.locator(`[data-sched-id="${ids.inactive}"]`)).toBeVisible();
  await expect(page.locator(`[data-sched-id="${ids.loop}"]`)).toHaveCount(0);
  // System pill shows amux's own jobs and hides the user list.
  await page.locator('.sx-pill-btn[data-group="system"]').click();
  await expect(page.locator('#scheduler-view .sysjob-section')).toBeVisible();
  await expect(page.locator('#scheduler-list .sx-group')).toHaveCount(0);
  await page.locator('.sx-pill-btn[data-group="all"]').click();
  // Type and status filters.
  await page.selectOption('#sched-f-type', 'shell');
  await expect(page.locator('#scheduler-list [data-sched-id]')).toHaveCount(1);
  await expect(page.locator(`[data-sched-id="${ids.routine}"]`)).toBeVisible();
  await page.selectOption('#sched-f-type', '');
  await page.selectOption('#sched-f-status', 'paused');
  await expect(page.locator('#scheduler-list [data-sched-id]')).toHaveCount(1);
  await page.selectOption('#sched-f-status', '');
  await page.selectOption('#sched-f-worker', 'e2e-w2');
  await expect(page.locator('#scheduler-list [data-sched-id]')).toHaveCount(1);
  await expect(page.locator(`[data-sched-id="${ids.weekly}"]`)).toBeVisible();
  await page.selectOption('#sched-f-worker', '');
  // Mobile: stacked rows, no header row, no overflow.
  const vw = page.viewportSize()!.width;
  if (vw <= 600) {
    await expect(page.locator('.sx-row-head').first()).toBeHidden();
  } else {
    await expect(page.locator('.sx-row-head').first()).toBeVisible();
  }
  await expectNoOverflow(page);
  await page.screenshot({ path: info.outputPath('scheduler-list.png'), fullPage: false });
});

test('details: tabs, pause/resume, run now, edit keeps the stored expression, duplicate, delete with the shared confirm', async ({ page, request }, info) => {
  await settle(page);
  const tok = await token(page);
  const p = `sx-det-${Date.now()}-`;
  const id = await create(request, tok, { title: p + 'nightly', kind: 'shell', session: 'e2e-owner', command: 'true', schedule_expr: 'daily at 9am' });
  await openScheduler(page, p);
  await page.locator(`[data-sched-id="${id}"] .sx-title`).click();
  const panel = page.locator('#sched-detail');
  await expect(panel).toBeVisible();
  await expect(page.locator('#sched-detail-id')).toHaveText(id);
  await expect(page.locator('#sched-detail-title')).toHaveText(p + 'nightly');
  const body = page.locator('#sched-detail-body');
  await expect(body).toContainText('Run a command');
  await expect(body).toContainText('Runs every day at');
  await expect(body).toContainText('Recent Runs');
  // Full-screen sheet on phones.
  const vw = page.viewportSize()!.width;
  if (vw <= 600) {
    await expect.poll(async () => Math.round((await page.locator('.sx-detail').boundingBox())!.width)).toBe(vw);
  }
  // Every Related action has real height (they once shrank to 2px in the
  // scrolling flex column and could not be tapped).
  for (const label of ['Edit schedule', 'Duplicate', 'Delete schedule']) {
    // Wait for the row before measuring it: boundingBox() reads null while the
    // details sheet is still rendering (CI, 2026-10-13: "reading 'height' of null").
    const row = page.locator('#sched-detail .sx-rel', { hasText: label });
    await expect(row).toBeVisible();
    // Polled: a live re-render between the visibility check and the read
    // detaches the row, and boundingBox() then reads null (CI, 2026-10-15).
    await expect.poll(async () => (await row.boundingBox())?.height ?? 0, { message: label + ' height' }).toBeGreaterThanOrEqual(40);
  }
  await page.waitForTimeout(400);   // let the sheet finish sliding in
  await page.screenshot({ path: info.outputPath('scheduler-details.png') });

  // History tab shows the attributed create.
  await page.locator('#sched-detail .sx-tab[data-tab="history"]').click();
  await expect(body).toContainText('created');
  await expect(body).toContainText('e2e-scheduler');
  await page.locator('#sched-detail .sx-tab[data-tab="runs"]').click();
  await expect(body).toContainText(/No runs recorded yet|most recent run/);
  await page.locator('#sched-detail .sx-tab[data-tab="logs"]').click();
  await expect(body.locator('.sx-log')).toBeVisible();
  await page.locator('#sched-detail .sx-tab[data-tab="details"]').click();

  // Pause, then resume: the server agrees each time, and the page says so.
  await body.getByRole('button', { name: /Pause/ }).click();
  await expect.poll(async () => (await getOne(request, tok, id)).body.enabled).toBe(0);
  await expect(page.locator('#sched-detail-status .sx-pill')).toHaveText('Paused');
  await expect(page.locator('#toast')).toContainText('Paused ' + id);
  await body.getByRole('button', { name: /Resume/ }).click();
  await expect.poll(async () => (await getOne(request, tok, id)).body.enabled).toBe(1);

  // Run now: a request goes out and an outcome is shown, whatever it is.
  const runResp = page.waitForResponse(r => r.url().includes(`/api/schedules/${id}/run`) && r.request().method() === 'POST');
  await body.getByRole('button', { name: /Run now/ }).click();
  await runResp;
  await expect(page.locator('#toast')).not.toHaveText('', { timeout: 5000 });

  // Edit: rename only. The stored expression "daily at 9am" must survive
  // verbatim (an untouched schedule is never rewritten as "daily at 09:00").
  await page.locator('#sched-detail .sx-rel', { hasText: 'Edit schedule' }).click();
  await expect(page.locator('#sched-overlay')).toHaveClass(/active/);
  await expect(page.locator('#sched-modal-title')).toHaveText('Edit ' + id);
  await expect(page.locator('#sched-session')).toHaveValue('e2e-owner');
  await page.fill('#sched-title', p + 'renamed');
  await page.locator('#sched-save-btn').click();
  await expect.poll(async () => (await getOne(request, tok, id)).body.title).toBe(p + 'renamed');
  const after = (await getOne(request, tok, id)).body;
  expect(after.schedule_expr).toBe('daily at 9am');
  expect(after.session).toBe('e2e-owner');
  expect(after.kind).toBe('shell');

  // Duplicate opens the create form prefilled; creating it adds a new row.
  await expect(panel).toBeVisible();
  await page.locator('#sched-detail .sx-rel', { hasText: 'Duplicate' }).click();
  await expect(page.locator('#sched-modal-title')).toHaveText('Duplicate schedule');
  await expect(page.locator('#sched-title')).toHaveValue('Copy of ' + p + 'renamed');
  await page.locator('#sched-save-btn').click();
  await expect.poll(async () => (await listAll(request, tok)).filter((s: any) => s.title === 'Copy of ' + p + 'renamed').length).toBe(1);
  const dup = (await listAll(request, tok)).find((s: any) => s.title === 'Copy of ' + p + 'renamed');
  expect(dup.schedule_expr).toBe('daily at 09:00');
  expect(dup.kind).toBe('shell');

  // Delete: the shared confirm dialog, Cancel keeps it, Delete removes it.
  await page.locator('.sx-detail-close').click();
  await expect(panel).toBeHidden();
  await openScheduler(page, p);
  await page.locator(`[data-sched-id="${id}"] .sx-title`).click();
  await page.locator('#sched-detail .sx-rel', { hasText: 'Delete schedule' }).click();
  await expect(page.locator('#modal-backdrop')).toHaveClass(/open/);
  await page.locator('#modal-btns .btn', { hasText: 'Cancel' }).click();
  expect((await getOne(request, tok, id)).body.deleted ?? null).toBeNull();
  await page.locator('#sched-detail .sx-rel', { hasText: 'Delete schedule' }).click();
  await page.locator('#modal-btns .btn.danger').click();
  await expect.poll(async () => (await listAll(request, tok)).some((s: any) => s.id === id)).toBe(false);
  await expect(page.locator('#toast')).toContainText('Deleted ' + id);
  await expect(panel).toBeHidden();
});

// An Edit save refetches the list and then closes the form. Duplicate tapped
// during that refetch opened a new form, which the late close hid behind
// display:none with its fields filled (iOS e2e, 2026-10-13). The refetch is
// held here so the tap always lands inside it.
test('a form opened while an edit save refetches stays open', async ({ page, request }) => {
  await settle(page);
  const tok = await token(page);
  const p = `sx-race-${Date.now()}-`;
  const id = await create(request, tok, { title: p + 'one', kind: 'shell', command: 'true', schedule_expr: 'daily at 9am' });
  await openScheduler(page, p);
  await page.locator(`[data-sched-id="${id}"] .sx-title`).click();
  await expect(page.locator('#sched-detail')).toBeVisible();
  await page.locator('#sched-detail .sx-rel', { hasText: 'Edit schedule' }).click();
  await expect(page.locator('#sched-overlay')).toHaveClass(/active/);
  await page.fill('#sched-title', p + 'renamed');

  let release!: () => void;
  const held = new Promise<void>(r => { release = r; });
  let sawRefetch!: () => void;
  const refetching = new Promise<void>(r => { sawRefetch = r; });
  // Hold every list GET after the save's PATCH until released: the save's own
  // refetch is among them (an SSE-driven refetch can arrive first).
  let armed = false;
  await page.route(/\/api\/schedules(\/[^/?]+)?(\?.*)?$/, async (route) => {
    const req = route.request();
    const isList = /\/api\/schedules(\?.*)?$/.test(req.url());
    if (req.method() === 'PATCH') { armed = true; return route.continue(); }
    if (req.method() !== 'GET' || !isList || !armed) return route.continue();
    sawRefetch();
    await held;
    await route.continue();
  });
  await page.locator('#sched-save-btn').click();
  await refetching;
  await page.evaluate(`duplicateSchedule(${JSON.stringify(id)})`);
  await expect(page.locator('#sched-modal-title')).toHaveText('Duplicate schedule');
  // The save ends by reopening the edited schedule's details, which fetches
  // its runs; that request marks the point the old close would have run.
  const saveDone = page.waitForRequest(r => r.url().includes('/api/schedules/runs?schedule_id=' + id));
  armed = false;
  release();
  await saveDone;
  await page.waitForTimeout(400);   // past closeSchedModal's 250ms hide timer
  await expect(page.locator('#sched-overlay')).toHaveClass(/active/);
  await expect(page.locator('#sched-save-btn')).toBeVisible();
  await page.locator('#sched-save-btn').click();
  await expect.poll(async () => (await listAll(request, tok)).filter((s: any) => s.title === 'Copy of ' + p + 'one').length).toBe(1);
});

// Opening the form adds `active` one frame after display:flex, and closing
// hides it 250ms after removing `active`. On a slow WebKit frame (CI iOS,
// 2026-10-13) Create right after a save let the stale close timer run between
// the two halves of the open: the form ended `active` but display:none, and
// every field read "not visible" until the test timed out. The frame is
// delayed here so the race is certain rather than lucky.
async function withSlowFrames(page: Page, script: string): Promise<void> {
  await page.evaluate(`(() => {
    const raf = window.requestAnimationFrame;
    window.requestAnimationFrame = (cb) => setTimeout(() => raf(cb), 400);
    try { ${script} } finally { window.requestAnimationFrame = raf; }
  })()`);
}
test('a form reopened inside the close fade stays visible when the frame is slow', async ({ page }) => {
  await settle(page);
  await openScheduler(page);
  await page.locator('.sx-new').click();
  await expect(page.locator('#sched-overlay')).toHaveClass(/active/);
  await withSlowFrames(page, 'closeSchedModal(); openSchedModal();');
  await page.waitForTimeout(600);   // past both the 250ms hide and the 400ms frame
  await expect(page.locator('#sched-overlay')).toHaveClass(/active/);
  await expect(page.locator('#sched-overlay')).toHaveCSS('display', 'flex');
  await expect(page.locator('#sched-title')).toBeVisible();
  await page.fill('#sched-title', 'reopened');
  await page.locator('#sched-overlay .sched-modal-foot .btn', { hasText: 'Cancel' }).click();
  await expect(page.locator('#sched-overlay')).toBeHidden();
});
test('a form closed before its opening frame stays closed', async ({ page }) => {
  await settle(page);
  await openScheduler(page);
  await withSlowFrames(page, 'openSchedModal(); closeSchedModal();');
  await page.waitForTimeout(600);
  await expect(page.locator('#sched-overlay')).not.toHaveClass(/active/);
  await expect(page.locator('#sched-overlay')).toBeHidden();
});

test('row menu: run, pause, edit and delete are reachable from the list', async ({ page, request }) => {
  await settle(page);
  const tok = await token(page);
  const p = `sx-menu-${Date.now()}-`;
  const id = await create(request, tok, { title: p + 'one', kind: 'shell', command: 'true', schedule_expr: 'every 30m' });
  await openScheduler(page, p);
  await page.locator(`[data-sched-id="${id}"] .sx-more-btn`).click();
  const menu = page.locator('#sched-menu');
  await expect(menu).toBeVisible();
  for (const label of ['Details', 'Run now', 'Pause', 'Skip next run', 'Edit schedule', 'Duplicate', 'Copy ID', 'Delete']) {
    await expect(menu.getByRole('menuitem', { name: label, exact: true })).toBeVisible();
  }
  const mb = await menu.boundingBox();
  const vw = page.viewportSize()!.width;
  expect(mb!.x).toBeGreaterThanOrEqual(0);
  expect(mb!.x + mb!.width).toBeLessThanOrEqual(vw);
  await menu.getByRole('menuitem', { name: 'Pause', exact: true }).click();
  await expect.poll(async () => (await getOne(request, tok, id)).body.enabled).toBe(0);
  // The row switch shows the pause at once (AMUX-5865: it used to show "on"
  // until a refetch landed, so this tap sent a second pause), then resumes it.
  const sw = page.locator(`[data-sched-id="${id}"] .sx-toggle input`);
  await expect(sw).not.toBeChecked();
  await sw.click();
  await expect(sw).toBeChecked();
  await expect.poll(async () => (await getOne(request, tok, id)).body.enabled).toBe(1);
  await page.locator(`[data-sched-id="${id}"] .sx-more-btn`).click();
  await menu.getByRole('menuitem', { name: 'Delete', exact: true }).click();
  await page.locator('#modal-btns .btn.danger').click();
  await expect.poll(async () => (await listAll(request, tok)).some((s: any) => s.id === id)).toBe(false);
});

// Typing that starts right after the sheet opens stays in the field it began
// in. The open used to focus Name 50ms later no matter what, so on a fast CI
// runner the Message text landed in Name, Message stayed empty and the save was
// refused (the create test below, desktop run 38814804812, mobile 38867620848).
test('create: a field entered right after open keeps focus', async ({ page }) => {
  await settle(page);
  await openScheduler(page);
  await page.evaluate(() => {
    (window as any).openSchedModal();
    document.getElementById('sched-command')!.focus();
  });
  await page.waitForTimeout(200);   // past the open's delayed focus of Name
  expect(await page.evaluate(() => document.activeElement && document.activeElement.id)).toBe('sched-command');
  await page.keyboard.insertText('typed early');
  await expect(page.locator('#sched-command')).toHaveValue('typed early');
  await expect(page.locator('#sched-title')).toHaveValue('');
  // An untouched open still lands on Name.
  await page.locator('#sched-overlay .sched-modal-foot .btn', { hasText: 'Cancel' }).click();
  await page.locator('.sx-new').click();
  await expect.poll(() => page.evaluate(() => document.activeElement && document.activeElement.id)).toBe('sched-title');
  await page.locator('#sched-overlay .sched-modal-foot .btn', { hasText: 'Cancel' }).click();
});

// Every preset, alternating the two types, must write an expression the server
// accepts, and the exact expression is checked.
const PRESETS: Array<{ preset: string; fill?: (page: Page) => Promise<void>; expr: string | null; stype?: string }> = [
  { preset: 'minute', expr: 'every 1m' },
  { preset: 'everyN', fill: async (pg) => { await pg.fill('#sched-every-n', '45'); }, expr: 'every 45m' },
  { preset: 'hour', expr: 'every 1h' },
  { preset: 'daily', fill: async (pg) => { await pg.fill('#sched-time', '07:30'); }, expr: 'daily at 07:30' },
  { preset: 'weekdays', fill: async (pg) => { await pg.fill('#sched-time', '08:15'); }, expr: 'every weekday at 08:15' },
  { preset: 'weekly', fill: async (pg) => { await pg.selectOption('#sched-weekday', 'friday'); await pg.fill('#sched-time', '06:00'); }, expr: 'weekly on friday at 06:00' },
  { preset: 'monthly', fill: async (pg) => { await pg.fill('#sched-monthday', '15'); await pg.fill('#sched-time', '09:00'); }, expr: 'monthly on 15 at 09:00' },
  { preset: 'once', expr: null, stype: 'once' },
  { preset: 'custom', fill: async (pg) => { await pg.fill('#sched-expr', '0 9 * * 1-5'); }, expr: '0 9 * * 1-5' },
];

test('create: every preset and both types save an expression the server parses', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  await settle(page);
  const tok = await token(page);
  const p = `sx-new-${Date.now()}-`;
  await addWorker(page, 'e2e-target');
  await openScheduler(page);
  for (let i = 0; i < PRESETS.length; i++) {
    const c = PRESETS[i];
    const shell = i % 2 === 0;
    // Re-added each time: a background session refresh replaces the list.
    await addWorker(page, 'e2e-target');
    await page.locator('.sx-new').click();
    await expect(page.locator('#sched-overlay')).toHaveClass(/active/);
    await expect(page.locator('#sched-modal-title')).toHaveText('Create Schedule');
    if (i === 0) {
      // Full-screen form on phones.
      const vw = page.viewportSize()!.width;
      // Polled: the dialog opens with a short scale-in transition.
      if (vw <= 600) {
        await expect.poll(async () => Math.round((await page.locator('#sched-overlay .sx-create').boundingBox())!.width)).toBe(vw);
        // Full-screen on phones: the sheet reaches the bottom (the shared
        // dialog guard once held it 40px short, with the list showing below).
        const vh = page.viewportSize()!.height;
        await expect.poll(async () => {
          const b = (await page.locator('#sched-overlay .sx-create').boundingBox())!;
          return Math.round(b.y + b.height);
        }).toBeGreaterThanOrEqual(vh - 2);
      }
      await page.waitForTimeout(300);
      await page.screenshot({ path: info.outputPath('scheduler-create.png') });
    }
    await page.fill('#sched-title', p + c.preset);
    await page.check(shell ? '#sched-kind-shell' : '#sched-kind-tmux');
    await expect(page.locator('#sched-cmd-prefix')).toBeVisible({ visible: shell });
    await page.fill('#sched-command', shell ? 'echo ' + c.preset : 'Report status for ' + c.preset);
    if (!shell) {
      // A session refresh can replace the client's list between opening the
      // form and picking; re-add and refill the picker right before choosing.
      await addWorker(page, 'e2e-target');
      await page.evaluate("_sxFillSessions('', false)");
      await page.selectOption('#sched-session', 'e2e-target');
    }
    await page.locator(`.sx-preset[data-preset="${c.preset}"]`).click();
    if (c.fill) await c.fill(page);
    // The preview describes the schedule and, for a worker, the cost.
    const preview = page.locator('#sched-preview');
    await expect(preview).not.toHaveText('');
    if (!shell && c.preset !== 'once') await expect(preview).toContainText('model turn');
    if (shell) await expect(preview).toContainText('no model turn');
    await page.locator('#sched-save-btn').click();
    await expect(page.locator('#sched-overlay')).not.toHaveClass(/active/, { timeout: 8000 });
    const row = (await listAll(request, tok)).find((s: any) => s.title === p + c.preset);
    expect(row, `${c.preset} was created`).toBeTruthy();
    expect(row.kind).toBe(shell ? 'shell' : 'tmux');
    if (c.expr) expect(row.schedule_expr).toBe(c.expr);
    if (c.stype) { expect(row.sched_type).toBe(c.stype); expect(row.run_at).toBeTruthy(); }
    if (!shell) expect(row.session).toBe('e2e-target');
  }
  // A worker trigger without a worker is refused in the form, not sent.
  await page.locator('.sx-new').click();
  await page.fill('#sched-title', p + 'no-worker');
  await page.check('#sched-kind-tmux');
  await page.fill('#sched-command', 'hello');
  await page.selectOption('#sched-session', '');
  await page.locator('#sched-save-btn').click();
  await expect(page.locator('#sched-session')).toHaveAttribute('aria-invalid', 'true');
  await expect(page.locator('#toast')).toContainText('Pick the worker');
  // Templates prefill without saving.
  await page.locator('.sx-nav-btn[data-sec="template"]').click();
  await page.locator('.sx-template', { hasText: 'Nightly script' }).click();
  await expect(page.locator('#sched-title')).toHaveValue('Nightly script');
  await expect(page.locator('#sched-kind-shell')).toBeChecked();
  await expect(page.locator('.sx-preset[data-preset="daily"]')).toHaveClass(/active/);
  await page.locator('#sched-overlay .sched-modal-foot .btn', { hasText: 'Cancel' }).click();
  expect((await listAll(request, tok)).some((s: any) => s.title === 'Nightly script')).toBe(false);
  await expectNoOverflow(page);
});
