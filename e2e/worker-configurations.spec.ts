// Worker Configurations is the one place an operator should be able to change
// every durable worker setting, board automation, gates, terminal-state
// availability, memory, rules, environment, skin, and connectors. This drives
// the real dashboard and real Rust API against each project's throwaway home.
import { test, expect } from './fixtures';

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
    await expect(panel).toContainText('Every durable worker setting');
    await expect(panel).toContainText('Task lifecycle');
    await expect(panel.getByRole('switch')).toHaveCount(8);
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
      const rows = await request.get('/api/sessions', { headers: auth });
      return (await rows.json()).find((s: any) => s.name === name)?.desc;
    }).toBe('configured entirely from the worker UI');

    // Structured select wiring: MCP/browser tooling must be configurable here,
    // not by knowing CC_MCP and editing a file. Return it to disabled so the
    // throwaway worker has no hidden capability after the assertion.
    await panel.locator('[data-worker-config="mcp"]').getByRole('button', { name: 'Edit' }).click();
    await page.locator('#edit-select').selectOption('chrome');
    await expect.poll(async () => {
      const rows = await request.get('/api/sessions', { headers: auth });
      return (await rows.json()).find((s: any) => s.name === name)?.mcp;
    }).toBe('chrome');
    await panel.locator('[data-worker-config="mcp"]').getByRole('button', { name: 'Edit' }).click();
    await page.locator('#edit-select').selectOption('');

    // Permission value path: unlike the old all-or-nothing toggle, the UI can
    // express an exact allow-list and clear it again.
    await panel.locator('[data-worker-config="cross_group"]').getByRole('button', { name: 'Edit' }).click();
    await page.locator('#edit-input').fill('e2e-destination');
    await page.locator('#edit-overlay').getByRole('button', { name: 'Save' }).click();
    await expect.poll(async () => {
      const rows = await request.get('/api/sessions', { headers: auth });
      return (await rows.json()).find((s: any) => s.name === name)?.spans_groups_value;
    }).toBe('e2e-destination');
    await panel.locator('[data-worker-config="cross_group"]').getByRole('button', { name: 'Edit' }).click();
    await page.locator('#edit-input').fill('');
    await page.locator('#edit-overlay').getByRole('button', { name: 'Save' }).click();
    for (const label of [
      'Backlog → To Do',
      'To Do → In Progress',
      'Continue non-terminal work',
      'Pickup / continue master',
    ]) {
      await expect(panel).toContainText(label);
    }

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

    // Default path: both queue transitions are on for every worker without a
    // redundant per-worker key.
    await expect.poll(async () => {
      const rows = await request.get('/api/sessions', { headers: auth });
      const worker = (await rows.json()).find((s: any) => s.name === name);
      return [worker?.auto_drain_backlog, worker?.auto_drain_backlog_own,
        worker?.auto_pickup, worker?.auto_pickup_own];
    }).toEqual([true, false, true, false]);

    // Explicit opt-out path: turn backlog drain off without changing To Do
    // pickup or the master switch.
    let backlogRow = panel.locator('[data-config-section="task-lifecycle"] .worker-config-row', { hasText: 'Backlog → To Do' }).first();
    await backlogRow.getByRole('switch').click();
    await expect.poll(async () => {
      const rows = await request.get('/api/sessions', { headers: auth });
      const worker = (await rows.json()).find((s: any) => s.name === name);
      return [worker?.auto_drain_backlog, worker?.auto_drain_backlog_own];
    }).toEqual([false, true]);

    // Inheritance path: remove the worker override and prove the runtime falls
    // back to the fleet default (backlog and To Do are both auto-driven).
    backlogRow = panel.locator('[data-config-section="task-lifecycle"] .worker-config-row', { hasText: 'Backlog → To Do' }).first();
    await backlogRow.getByRole('button', { name: 'Inherit' }).click();
    await expect.poll(async () => {
      const rows = await request.get('/api/sessions', { headers: auth });
      const worker = (await rows.json()).find((s: any) => s.name === name);
      return [worker?.auto_drain_backlog, worker?.auto_drain_backlog_own];
    }).toEqual([true, false]);

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
