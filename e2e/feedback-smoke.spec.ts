// No-silent-actions smoke (Ethan, 2026-08-09: "make sure every action has some
// kind of response in the ui"). Five representative user actions, each asserted
// to produce a VISIBLE response — the exact property whose absence shipped
// twice in one week (schedule delete, then worker delete: an undefined name in
// the async handler meant the click did literally nothing).
//
// Runs against the throwaway-AMUX_HOME Rust server from playwright.config.ts —
// never the live server. Both projects run it, so every action here is also
// exercised at 375px (mobile-first); each test ends with an overflow check.
import { test, expect, Page } from '@playwright/test';
import { clickSnapped } from './helpers';

// The clipboard permission is granted INSIDE the one test that needs it, not
// file-wide. WebKit has no 'clipboard-write' permission, and a file-level
// grant throws at CONTEXT CREATION ("browserContext.newPage: Unknown
// permission: clipboard-write") — which killed all five tests in this file the
// moment the ios-safari project was added, before a single page loaded. A
// per-test grant costs the same and confines the engine gap to the one
// assertion that depends on it (AMUX-3057).

/** Load '/' and wait for the app shell + its API layer (golden.spec idiom). */
async function settle(page: Page): Promise<void> {
  await page.goto('/');
  await expect(page.locator('#conn-status').first()).toBeAttached();
  await page.waitForFunction(() => typeof (window as any).apiCall === 'function');
  // Self-signed origin: SW cannot register → #sw-fail-bar overlays the bottom.
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
  // Fresh profile + zero workers auto-launches the walkthrough; skip it via
  // its own button, as a first-time user would.
  const wt = page.locator('#wt-overlay.open');
  await wt.waitFor({ state: 'visible', timeout: 8_000 }).catch(() => {});
  if (await wt.isVisible()) {
    await page.locator('#wt-tooltip .wt-skip').click();
    await expect(wt).toBeHidden();
  }
}

async function appToken(page: Page): Promise<string> {
  const tok = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(tok, 'served bootstrap must inject a non-empty auth token').toBeTruthy();
  return tok;
}

/** Mobile-first: no action may leave the page horizontally overflowed. */
async function expectNoOverflow(page: Page): Promise<void> {
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
  );
  expect(overflow, 'no horizontal overflow after the action').toBe(false);
}

test('worker delete: click answers with a confirm dialog; cancel keeps the worker', async ({ page, request }) => {
  await settle(page);
  const token = await appToken(page);
  const name = `fbk-del-${Date.now()}`;
  const created = await request.post('/api/workers', {
    headers: { Authorization: `Bearer ${token}` },
    data: { display_name: name, cwd: '/tmp', provider: 'claude-code' },
  });
  expect(created.status()).toBe(201);
  await page.reload();
  // The SPA renders the card in more than one container; act on the visible one.
  const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
  await expect(card).toBeVisible({ timeout: 10_000 });

  await card.locator('.card-menu-btn').click();
  // The open menu is PORTALED to <body> (escapes the card's overflow:hidden),
  // so locate it at document level, not inside the card.
  await page.locator('.card-menu.open .card-menu-item.danger', { hasText: 'Delete' }).click();
  // THE assertion this file exists for: the click must answer. The 2026-08-09
  // bug died before this dialog — no dialog, no request, no error.
  const modal = page.locator('#modal-backdrop.open');
  await expect(modal).toBeVisible({ timeout: 2_000 });
  await expect(page.locator('#modal-msg')).toContainText(`Delete worker "${name}"`);

  await modal.getByRole('button', { name: 'Cancel' }).click();
  await expect(modal).toBeHidden();
  await expect(card).toBeVisible(); // cancel path: nothing deleted
  await expectNoOverflow(page);
});

test('schedule enable toggle: card visibly re-renders into the disabled lane', async ({ page, request }) => {
  await settle(page);
  const token = await appToken(page);
  const title = `fbk-sched-${Date.now()}`;
  const created = await request.post('/api/schedules', {
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    data: { title, session: 'nobody', command: 'echo smoke', schedule_expr: 'daily at 9am', enabled: 1 },
  });
  expect([200, 201]).toContain(created.status());
  const id = (await created.json()).id;
  expect(id, 'created schedule returns an id').toBeTruthy();

  await page.click('#tab-scheduler');
  const card = page.locator(`[data-sched-id="${id}"]`);
  await expect(card).toBeVisible({ timeout: 10_000 });
  const box = card.locator('.sched-toggle-label input');
  await expect(box).toBeChecked();

  await box.click();
  // Visible response within ~1s: fetch + re-render leaves the card's toggle
  // unchecked (it re-renders into the disabled section).
  await expect(page.locator(`[data-sched-id="${id}"] .sched-toggle-label input`)).not.toBeChecked({ timeout: 5_000 });
  await expectNoOverflow(page);
});

test('schedule id copy button: toast confirms the copy', async ({ page, request, context, browserName }) => {
  // Chromium needs the permission for navigator.clipboard.writeText; WebKit
  // does not HAVE it as a permission (it allows the write under a user
  // gesture, which a real click is). Asking anyway is a hard error, so ask
  // only where the permission exists.
  if (browserName !== 'webkit') await context.grantPermissions(['clipboard-write']);
  await settle(page);
  const token = await appToken(page);
  const title = `fbk-copy-${Date.now()}`;
  const created = await request.post('/api/schedules', {
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    data: { title, session: 'nobody', command: 'echo smoke', schedule_expr: 'daily at 9am', enabled: 1 },
  });
  const id = (await created.json()).id;

  await page.click('#tab-scheduler');
  const badge = page.locator(`[data-sched-id="${id}"] .sched-id-badge`);
  await expect(badge).toBeVisible({ timeout: 10_000 });
  await badge.click();
  const toast = page.locator('#toast.visible');
  await expect(toast).toBeVisible({ timeout: 2_000 });
  await expect(toast).toContainText('copied');
  await expectNoOverflow(page);
});

test('board: create shows the card; Clear done visibly removes it', async ({ page }) => {
  await settle(page);
  const title = `fbk-board-${Date.now()}`;

  await page.click('#tab-board');
  await expect(page.locator('#board-view')).toBeVisible();
  await page.click('.board-new-btn');
  await expect(page.locator('#board-edit-overlay')).toHaveClass(/active/);
  await page.fill('#be-title', title);
  await page.selectOption('#be-status', 'done');
  await page.click('.be-save');
  await expect(page.locator('#board-edit-overlay')).not.toHaveClass(/active/);
  // Visible response 1: the card appears in the board DOM.
  await expect(page.locator('#board-view')).toContainText(title, { timeout: 5_000 });

  // Visible response 2: Clear done removes it from the DOM optimistically.
  // clickSnapped, not .click(): the button lives in the "done" column of a
  // scroll-snap kanban, ~1000px along at phone width, and a plain click races
  // the snap under WebKit — it silently misses and this assertion then blames
  // clearDone() (AF-47).
  await clickSnapped(
    page.locator('#board-view button', { hasText: 'Clear done' }).first(),
    'Clear done',
  );
  await expect(page.locator('#board-view')).not.toContainText(title, { timeout: 5_000 });
  await expectNoOverflow(page);
});

test('board: a delayed pre-clear poll cannot resurrect an archived card after recovery', async ({page, request}) => {
  await settle(page);
  const token = await appToken(page);
  const title = `fbk-clear-race-${Date.now()}`;
  const response = await request.post('/api/board', {
    headers:{Authorization:`Bearer ${token}`},
    data:{title,status:'done',type:'ops'},
  });
  expect(response.status()).toBe(201);
  const id = (await response.json()).id;
  await page.click('#tab-board');
  await page.evaluate(async () => { await (window as any).fetchBoard(); });
  await expect(page.locator('#board-view')).toContainText(title);
  let release!: () => void;
  let observed!: () => void;
  const gate = new Promise<void>(r => { release = r; });
  const captured = new Promise<void>(r => { observed = r; });
  let armed = true;
  await page.route('**/api/board?*', async route => {
    if (armed && route.request().url().includes('archived=0')) {
      armed = false;
      // Force an actual pre-commit body rather than an ETag 304: the stale
      // card must be present in the delayed response for this to prove fencing.
      const headers = {...route.request().headers()};
      delete headers['if-none-match'];
      const older = await route.fetch({headers});
      expect(older.status()).toBe(200);
      expect((await older.json()).some((card: any) => card.id === id)).toBe(true);
      observed();
      await gate;
      await route.fulfill({response:older});
    } else await route.continue();
  });
  await page.evaluate(() => { (window as any).__delayedBoardRead = (window as any).fetchBoard(); });
  await captured;
  const cleared = page.waitForResponse(r => r.url().endsWith('/api/board/clear-done') && r.request().method() === 'POST');
  await clickSnapped(page.locator('#board-view button', {hasText:'Clear done'}).first(), 'Clear done');
  expect((await cleared).status()).toBe(200);
  release();
  await page.evaluate(async () => { await (window as any).__delayedBoardRead; });
  await expect(page.locator('#board-view')).not.toContainText(title);
  const persisted = await request.get(`/api/board/${id}`, {headers:{Authorization:`Bearer ${token}`}});
  expect(persisted.status()).toBe(200);
  expect((await persisted.json()).archived).toBe(1);
  await page.unroute('**/api/board?*');
  await page.reload();
  await page.click('#tab-board');
  await expect(page.locator('#board-view')).not.toContainText(title);
});

test('alert settings toggle: save answers with a toast (new no-silent-actions feedback)', async ({ page }) => {
  await settle(page);
  await page.click('#settings-btn');
  // AMUX-2975 (ec031ce) grouped settings into tabs; the alert toggle's panel is
  // display:none until its tab is active, which timed out scrollIntoViewIfNeeded.
  // Reveal all panels (the same approach settings.spec.ts's openSettings uses and
  // that settings_alerts_config / settings_theme_toggle pass green on all three
  // engines) rather than click the tab: on WebKit a tab-button click left the
  // panel shown but the subsequent track-click's save/toast never fired, while the
  // CSS reveal matches the flow those tests already verify cross-engine.
  await page.addStyleTag({
    content: '#settings-menu .settings-tab-panel{display:block !important}',
  });
  const cb = page.locator('#alert-push-cb');
  await cb.scrollIntoViewIfNeeded();
  // Click the LABEL, not the inner .theme-track span: on WebKit clicking the span
  // toggled the checkbox visually but did not dispatch the input's `change` event,
  // so saveAlertConfig() (bound via onchange) never ran and no toast appeared.
  // Clicking the associated <label> reliably fires change on every engine, and is
  // equally "as a user would" — the whole toggle is the label.
  await page.locator('label:has(#alert-push-cb)').click();
  const toast = page.locator('#toast.visible');
  await expect(toast).toBeVisible({ timeout: 2_000 });
  await expect(toast).toContainText(/Alert settings saved|Failed to save alert settings/);
  // The happy path must be the one that actually happened on this server:
  await expect(toast).toContainText('Alert settings saved');
  await expectNoOverflow(page);
});
