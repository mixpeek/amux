// Event subscriptions live in the Scheduler (docs/design/event-subscriptions.md):
// a schedule whose trigger is an event. "+ New" asks "At a time" or "When
// something happens"; the words are compiled and backtested by the server and
// shown before saving; the row fires on a real event and its history says so;
// a worker's Schedules tab holds the ones that wake it. The compile needs the
// helper model, which a test home does not have, so only the dry run is
// answered here; everything else is the real API.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test.setTimeout(90_000);

const COMPILED = {
  pattern: 'browser.blocked', filter: { field: 'reason', op: 'eq', value: 'captcha' },
  summary: 'A browser route lands on a CAPTCHA', judge: false, dedupe_key: '{{url}}',
};

async function boot(page: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const sent: any[] = [];
  await page.route(/\/api\/subscriptions$/, async (route: any) => {
    if (route.request().method() !== 'POST') return route.continue();
    const body = route.request().postDataJSON();
    sent.push(body);
    if (!body.dry_run) return route.continue();
    return route.fulfill({ json: { dry_run: true, compiled: COMPILED, pattern: COMPILED.pattern, filter: COMPILED.filter,
      action: body.action, action_target: body.action_target,
      backtest: { measured: true, matched: 1, n_considered: 40, examples: [
        { id: 1, ts: 1794400000, event: { session: 'amux', url: 'https://www.google.com/recaptcha/api2/demo' } }] } } });
  });
  return { auth, sent };
}

/** The schedules and subscriptions a test gave a fixture worker. */
async function purgeFixtures(request: any, auth: any, name: string) {
  const sched = await (await request.get('/api/schedules', { headers: auth })).json();
  for (const x of sched.filter((x: any) => x.session === name)) await request.delete(`/api/schedules/${x.id}`, { headers: auth });
  const subs = await (await request.get('/api/subscriptions', { headers: auth })).json();
  for (const s of subs.subscriptions.filter((x: any) => x.action_target === name)) {
    await request.delete(`/api/subscriptions/${s.id}`, { headers: auth });
  }
}

async function chooseEvent(page: any) {
  await page.locator('.sx-ev-choice-btn', { hasText: 'When something happens' }).click();
}

test('the owner subscribes from the Scheduler, it fires on a real event, and it can be turned off and removed', async ({ page, request }) => {
  const { auth, sent } = await boot(page);
  await page.evaluate(() => (window as any).switchView('scheduler'));
  const section = page.locator('#scheduler-events section[data-group="events"]');
  await expect(section).toContainText('When something happens');

  await page.locator('#scheduler-view .sx-new').click();
  await expect(page.locator('.sx-ev-choice-btn')).toHaveCount(2);
  await chooseEvent(page);
  await page.locator('#ev-words').fill('a browser hits a captcha');
  await page.locator('#modal-btns .btn', { hasText: 'Preview' }).click();
  const preview = page.locator('#modal-msg');
  await expect(preview).toContainText('A browser route lands on a CAPTCHA');
  await expect(preview).toContainText('Last 7 days: 1 of 40 events matched');
  await expect(preview).toContainText('google.com/recaptcha/api2/demo');
  await expect(preview).toContainText('tells you in #general');
  await page.locator('#modal-btns .btn', { hasText: 'Save' }).click();

  const row = section.locator('.sx-ev-row', { hasText: 'a browser hits a captcha' });
  await expect(row).toContainText('tells you', { timeout: 10_000 });
  expect(sent[1]).toMatchObject({ criteria: 'a browser hits a captcha', pattern: 'browser.blocked', action: 'notify',
    filter: COMPILED.filter, dedupe_key: '{{url}}' });

  // A real matching event fires it; a non-matching one does not.
  const emit = (reason: string) => request.post('/api/subscriptions/emit', { headers: auth,
    // The fields a real browser.blocked carries (browser_routing.rs blocked_event).
    data: { type: 'browser.blocked', session: 'amux', data: { reason, provider: 'recaptcha', worker: 'amux',
      url: 'https://www.google.com/recaptcha/api2/demo' } } });
  expect((await (await emit('captcha')).json()).subscriptions_fired.length).toBe(1);
  expect((await (await emit('login')).json()).subscriptions_fired.length).toBe(0);
  await page.evaluate(() => (window as any)._evRefresh());
  await expect(row).toContainText('fired 1×');
  await row.locator('.sx-ev-main').click();
  await expect(page.locator('#modal-msg')).toContainText('browser.blocked · amux');
  await expect(page.locator('#modal-msg')).toContainText('posted:');

  // The type filter shows only events.
  await page.locator('#modal-btns .btn').first().click();
  await page.locator('#sched-f-type').selectOption('event');
  await expect(page.locator('#scheduler-list')).toBeEmpty();
  await expect(row).toBeVisible();
  await page.locator('#sched-f-type').selectOption('tmux');
  await expect(page.locator('#scheduler-events')).toBeEmpty();
  await page.locator('#sched-f-type').selectOption('');

  // Off, on, remove.
  await row.locator('.sx-ev-toggle').click();
  await expect(row).toHaveClass(/sx-ev-off/);
  await expect(row.locator('.sx-ev-toggle')).toHaveText('Off');
  expect((await (await emit('captcha')).json()).subscriptions_fired.length, 'an off subscription does not fire').toBe(0);
  await row.locator('.sx-ev-toggle').click();
  await expect(row.locator('.sx-ev-toggle')).toHaveText('On');
  await row.locator('.sx-ev-main').click();
  await page.locator('#modal-msg .btn', { hasText: 'Remove' }).click();
  await page.locator('#modal-btns .btn', { hasText: 'Remove' }).click();
  await expect(row).toHaveCount(0, { timeout: 10_000 });
  const left = await (await request.get('/api/subscriptions', { headers: auth })).json();
  expect(left.subscriptions.filter((s: any) => s.criteria === 'a browser hits a captcha')).toEqual([]);
});

test('a worker gets a subscription from its Schedules tab, and the row fits a phone', async ({ page, request }, info) => {
  const { auth, sent } = await boot(page);
  const name = `evsub-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  try {
    await expect.poll(() => page.evaluate(async (n) => {
      await (window as any).fetchSessions();
      return (0, eval)('sessions').some((s: any) => s.name === n);
    }, name), { timeout: 30_000 }).toBe(true);
    // A real schedule too, so the tab has a list the events block must not cover.
    expect((await request.post('/api/schedules', { headers: auth, data: {
      title: 'nightly check', session: name, command: 'echo ok', schedule_expr: 'daily at 3am', kind: 'shell' } })).ok()).toBe(true);
    await page.evaluate((n) => (window as any).openPeek(n), name);
    await page.evaluate(() => (window as any).setPeekTab('schedules'));
    const section = page.locator('#peek-schedules-list [data-group="events"]');
    await expect(section).toContainText('When something happens');
    await expect(section).toContainText('none yet');
    // Inside the schedule list, below the schedules (it sat on top of them:
    // Ethan's screenshot, 2026-11-11).
    const lastGroup = page.locator('#peek-schedules-list .sx-group').last();
    await expect(lastGroup).toContainText('nightly check');
    const g = (await lastGroup.boundingBox())!;
    const ev = (await section.boundingBox())!;
    expect(ev.y).toBeGreaterThanOrEqual(g.y + g.height - 1);

    await section.locator('.sx-ev-new').click();
    await expect(page.locator('#ev-worker')).toHaveValue(name);
    await expect(page.locator('input[name=ev-then][value=send]')).toBeChecked();
    await page.locator('#ev-words').fill('a browser hits a captcha');
    await page.locator('#modal-btns .btn', { hasText: 'Preview' }).click();
    await expect(page.locator('#modal-msg')).toContainText(`wakes ${name}`);
    await page.locator('#modal-btns .btn', { hasText: 'Save' }).click();
    const row = section.locator('.sx-ev-row');
    await expect(row).toContainText(`wakes ${name}`, { timeout: 10_000 });
    expect(sent[sent.length - 1]).toMatchObject({ action: 'send', action_target: name, pattern: 'browser.blocked' });

    await page.setViewportSize({ width: 375, height: 760 });
    const toggle = row.locator('.sx-ev-toggle');
    const box = (await toggle.boundingBox())!;
    expect(box.height).toBeGreaterThanOrEqual(44);
    expect(box.x + box.width).toBeLessThanOrEqual(375);
    const main = (await row.locator('.sx-ev-main').boundingBox())!;
    expect(Math.abs(main.y + main.height / 2 - (box.y + box.height / 2))).toBeLessThan(24);
  } finally {
    await cleanup('delete the fixture schedules and subscriptions', () => purgeFixtures(request, auth, name));
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`, { headers: auth }));
  }
});
