// Worker Configurations is the one place an operator should be able to change
// every durable worker setting, board automation, gates, terminal-state
// availability, memory, rules, environment, skin, and connectors. This drives
// the real dashboard and real Rust API against each project's throwaway home.
import { test, expect } from './fixtures';
import { getSessionsResilient } from './lifecycle/evidence';

test.setTimeout(60_000);

test('worker Configurations edits the full board lifecycle and every scoped capability', async ({ page, request }, testInfo) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(token, 'served bootstrap must provide the API token').toBeTruthy();
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };

  // A fresh test home opens onboarding over the worker list. Dismiss it through
  // its own UI so the test follows the same path as a first-time operator.
  const walkthrough = page.locator('#wt-overlay.open');
  await walkthrough.waitFor({ state: 'visible', timeout: 2_000 }).catch(() => {});
  if (await walkthrough.isVisible()) await page.locator('#wt-tooltip .wt-skip').click();

  const name = `config-${testInfo.project.name}-${Date.now()}`;
  try {
    const created = await request.post('/api/sessions', {
      headers: auth,
      data: { name, dir: '/tmp', tags: ['e2e-configurations'] },
    });
    expect(created.status()).toBe(201);

    await page.reload();
    // CHAOS CELL: another worker can create pending permission grants while
    // this operator is opening Configurations. The real full-suite race grew
    // this global strip over the upward-opening worker menu and swallowed the
    // Peek click. Keep that concurrency shape deterministic in this spec.
    await page.locator('#email-approvals-banner').evaluate((el: HTMLElement) => {
      el.style.display = 'block';
      el.innerHTML = '<div style="height:240px">Concurrent permission request</div>';
    });
    const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
    await expect(card).toBeVisible({ timeout: 10_000 });
    await card.locator('.card-menu-btn').click();
    await page.locator('.card-menu.open .card-menu-item', { hasText: 'Peek terminal' }).click();

    await page.getByRole('button', { name: /Configurations$/ }).click();
    const panel = page.locator('#peek-scope-body');
    // A select or Save fires the config PATCH without awaiting it, and the
    // sessions list can take seconds to rebuild on a loaded runner, so a poll
    // started before the write landed could spend its whole budget on one
    // stale read. Wait for the write itself, then read the list, and give the
    // read the list's real latency (a single build can wait on the fleet
    // builder) rather than expect.poll's 5s default.
    const settled = { timeout: 20_000 };
    const configWrite = () => page.waitForResponse((r) =>
      r.url().includes(`/api/sessions/${name}/config`) && r.request().method() === 'PATCH');
    await expect(panel).toContainText('Every durable worker setting');
    // Board automation moved to the worker's Board tab (7f9c835f, Ethan
    // 2026-09-24: "all board configurations should be toggles on the worker
    // details board tab contents"); it is asserted there below.
    await expect(panel).not.toContainText('Task lifecycle');
    // Count measured after 7f9c835f (lifecycle switches gone) and the worker
    // type, automatic approval and external email controls arriving.
    await expect(panel.getByRole('switch')).toHaveCount(10);
    for (const key of [
      'name', 'description', 'task_label', 'groups', 'directory', 'branch',
      'provider', 'model', 'effort', 'mcp', 'yolo', 'isolated', 'cross_group',
      'pinned', 'advanced_environment',
    ]) {
      await expect(panel.locator(`[data-worker-config="${key}"]`), `missing ${key}`).toHaveCount(1);
    }

    // Shared text editor wiring: mutate a harmless durable field through the
    // new location and observe the API truth after the panel reconciles.
    await panel.locator('[data-worker-config="description"]').getByRole('button', { name: 'Edit' }).click();
    await page.locator('#edit-input').fill('configured entirely from the worker UI');
    await page.locator('#edit-overlay').getByRole('button', { name: 'Save' }).click();
    await expect.poll(async () => {
      const rows = await getSessionsResilient(request, auth);
      return (await rows.json()).find((s: any) => s.name === name)?.desc;
    }, settled).toBe('configured entirely from the worker UI');

    // Structured select wiring: MCP/browser tooling must be configurable here,
    // not by knowing CC_MCP and editing a file. Return it to disabled so the
    // throwaway worker has no hidden capability after the assertion.
    await panel.locator('[data-worker-config="mcp"]').getByRole('button', { name: 'Edit' }).click();
    await Promise.all([configWrite(), page.locator('#edit-select').selectOption('chrome')]);
    await expect.poll(async () => {
      const rows = await getSessionsResilient(request, auth);
      return (await rows.json()).find((s: any) => s.name === name)?.mcp;
    }, settled).toBe('chrome');
    await panel.locator('[data-worker-config="mcp"]').getByRole('button', { name: 'Edit' }).click();
    await Promise.all([configWrite(), page.locator('#edit-select').selectOption('')]);

    // Worker messaging has a hard group boundary; a configuration control
    // must not advertise a legacy allowance as an effective permission.
    const messaging = panel.locator('[data-worker-config="cross_group"]');
    await expect(messaging).toContainText('Shared groups only');
    await expect(messaging.getByRole('button', { name: 'Edit' })).toHaveCount(0);
    const widening = await request.patch('/api/sessions/' + name + '/config', {
      headers: auth, data: { send_allow: '*' },
    });
    expect(widening.status()).toBe(403);
    expect((await widening.json()).code).toBe('worker_group_boundary');

    // The server advertises seven worker-level capabilities. Every one must
    // open the shared editor; the old UI offered a button only for text and
    // told users to edit the other five "where they live".
    const tiles = panel.locator('.scope-tile');
    await expect(tiles).toHaveCount(7);
    for (let i = 0; i < 7; i += 1) {
      await tiles.nth(i).click();
      await expect(panel.getByRole('button', { name: /^Edit .+ at this level$/ })).toBeVisible();
      await tiles.nth(i).click();
      await expect(panel.getByRole('button', { name: /^Edit .+ at this level$/ })).toHaveCount(0);
    }

    // New workers explicitly own OFF values for all six automation toggles
    // (de344082), even when the fleet would enable them.
    await expect.poll(async () => {
      const rows = await getSessionsResilient(request, auth);
      const worker = (await rows.json()).find((s: any) => s.name === name);
      return [worker?.auto_drain_backlog, worker?.auto_drain_backlog_own,
        worker?.auto_pickup, worker?.auto_pickup_own];
    }, settled).toEqual([false, true, false, true]);

    // ONE BOARD SWITCH (Ethan, 2026-10-17: "a single toggle enable/disable
    // board. that should do all the board stuff"). The Board tab shows one
    // switch; On turns on every board key and Off turns every one off.
    await page.locator('#peek-tab-issues').click();
    const boardConfig = page.locator('#peek-board-config');
    const sw = boardConfig.locator('.board-one-switch');
    await expect(sw).toHaveCount(1);
    await expect(boardConfig.locator('.pbc-row, details.settings-fold')).toHaveCount(0);
    const allBoard = async () => {
      const rows = await getSessionsResilient(request, auth);
      const w = (await rows.json()).find((s: any) => s.name === name);
      return [w?.board_force_adherence, w?.board_decompose, w?.auto_pickup, w?.auto_continue, w?.auto_drain_backlog];
    };
    const agrees = async (on: boolean) => {
      await expect(sw).toHaveText(on ? 'On' : 'Off');
      await expect(sw).toHaveAttribute('aria-checked', String(on));
      // It looks like a switch (Ethan, 2026-10-21): a track, accent when on,
      // with the knob moved across; never a filled button.
      await expect(sw).not.toHaveClass(/primary/);
      const look = await sw.evaluate(el => {
        const t = getComputedStyle(el, '::before'), k = getComputedStyle(el, '::after');
        const probe = document.createElement('i'); probe.style.color = 'var(--accent)'; document.body.append(probe);
        const accent = getComputedStyle(probe).color; probe.remove();
        return {track: t.backgroundColor, accent, knobMoved: k.transform !== 'none', trackW: parseFloat(t.width)};
      });
      expect(look.trackW, 'a track is drawn').toBeGreaterThan(20);
      expect(look.track === look.accent, 'track is the accent colour only when on').toBe(on);
      expect(look.knobMoved, 'knob is across only when on').toBe(on);
    };
    await agrees(false);
    const box = await sw.boundingBox();
    expect(box!.height, 'a 44px touch target').toBeGreaterThanOrEqual(44);
    await Promise.all([configWrite(), sw.click()]);
    await expect.poll(allBoard, settled).toEqual([true, true, true, true, true]);
    await agrees(true);
    // A rebuild from fresh data keeps what the server holds.
    await page.evaluate(async () => { await (window as any).fetchSessions(); const el = document.getElementById('peek-board-config') as any; el._pbcHtml = ''; (window as any)._peekBoardPolicySync(); });
    await agrees(true);
    await Promise.all([configWrite(), sw.click()]);
    await expect.poll(allBoard, settled).toEqual([false, false, false, false, false]);
    await agrees(false);

    // Mobile guard: the longer tab name and configuration controls must not widen the
    // page beyond the viewport on any browser project.
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
    );
    expect(overflow).toBe(false);
  } finally {
    await request.delete(`/api/sessions/${name}`, { headers: auth }).catch(() => {});
  }
});
