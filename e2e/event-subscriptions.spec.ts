// A worker's Configurations tab lists what wakes it (event subscriptions,
// docs/design/event-subscriptions.md) and adds one in plain words: the words
// go to the server, which compiles them into an event filter and backtests
// it; the confirm shows both before anything is saved. The compile needs the
// helper model, which a test home does not have, so the two POSTs are
// answered here; the list, toggle and delete go to the real API.
import { test, expect } from './fixtures';

test.setTimeout(60_000);

test('a worker subscribes in plain words from its Configurations tab', async ({ page, request }, testInfo) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const walkthrough = page.locator('#wt-overlay.open');
  await walkthrough.waitFor({ state: 'visible', timeout: 2_000 }).catch(() => {});
  if (await walkthrough.isVisible()) await page.locator('#wt-tooltip .wt-skip').click();

  const name = `subs-${testInfo.project.name}-${Date.now()}`;
  const created = await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp' } });
  expect(created.status()).toBe(201);
  const sent: any[] = [];
  await page.route(/\/api\/subscriptions$/, async (route) => {
    if (route.request().method() !== 'POST') return route.continue();
    const body = route.request().postDataJSON();
    sent.push(body);
    const compiled = { pattern: 'browser.blocked', filter: { field: 'reason', op: 'eq', value: 'captcha' },
      summary: 'A browser route lands on a CAPTCHA', judge: false, dedupe_key: '{{url}}' };
    if (body.dry_run) {
      return route.fulfill({ json: { dry_run: true, compiled, pattern: compiled.pattern, filter: compiled.filter,
        action: 'send', action_target: name, backtest: { measured: true, matched: 2, n_considered: 51 } } });
    }
    // The real create gets exactly what the dashboard sent: the confirmed
    // compile, so the server does not compile again.
    const real = await request.post('/api/subscriptions', { headers: auth, data: body });
    return route.fulfill({ status: real.status(), json: await real.json() });
  });

  await page.reload();
  const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
  await expect(card).toBeVisible({ timeout: 10_000 });
  await card.locator('.card-menu-btn').click();
  await page.locator('.card-menu.open .card-menu-item', { hasText: 'Peek terminal' }).click();
  await page.getByRole('button', { name: /Configurations$/ }).click();

  const row = page.locator('[data-worker-config="subscriptions"]');
  await expect(row).toContainText('None. Add one in plain words');

  await row.getByRole('button', { name: 'Add' }).click();
  await page.locator('#modal-prompt-input').fill('a browser hits a captcha');
  await page.locator('#modal-prompt-input').press('Enter');
  const confirm = page.locator('#modal-msg');
  await expect(confirm).toContainText('A browser route lands on a CAPTCHA');
  await expect(confirm).toContainText('Last 7 days: 2 of 51 events would have matched.');
  await expect(confirm).toContainText(`${name} gets a message`);
  await page.locator('#modal-btns .btn', { hasText: 'Subscribe' }).click();

  await expect(row).toContainText('1 active: a browser hits a captcha', { timeout: 10_000 });
  expect(sent.map((b) => [b.criteria, b.action_target, !!b.dry_run, b.pattern || null])).toEqual([
    ['a browser hits a captcha', name, true, null],
    ['a browser hits a captcha', name, false, 'browser.blocked'],
  ]);
  expect(sent[1].filter).toEqual({ field: 'reason', op: 'eq', value: 'captcha' });
  expect(sent[1].dedupe_key).toBe('{{url}}');

  // Manage: turn it off, then remove it, through the real API.
  await row.getByRole('button', { name: 'Manage' }).click();
  await page.locator('#modal-msg .btn', { hasText: 'Turn off' }).click();
  await expect(page.locator('#modal-msg')).toContainText('· off');
  await expect(row).toContainText('None.', { timeout: 10_000 });
  await page.locator('#modal-msg .btn', { hasText: 'Remove' }).click();
  await expect(page.locator('#modal-msg')).toContainText('No subscriptions yet.');
  const left = await (await request.get('/api/subscriptions', { headers: auth })).json();
  expect(left.subscriptions.filter((s: any) => s.action_target === name)).toEqual([]);

  // On a phone the row's controls stay on one row, inside the screen, at the touch floor.
  await page.locator('#modal-btns .btn').first().click();
  await page.setViewportSize({ width: 375, height: 760 });
  const add = row.getByRole('button', { name: 'Add' });
  const manage = row.getByRole('button', { name: 'Manage' });
  const [a, m] = [await add.boundingBox(), await manage.boundingBox()];
  expect(a!.height).toBeGreaterThanOrEqual(44);
  expect(Math.abs(a!.y - m!.y)).toBeLessThan(4);
  expect(m!.x + m!.width).toBeLessThanOrEqual(375);
});
