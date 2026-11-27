import { test, expect } from './fixtures';

// The env scope editor lists the environment registry the server publishes on
// GET /api/scope (env capability `registry`), collapsed inside the hint line.
test('the env scope editor lists the scopable keys from the environment registry', async ({ page }, info) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const scope = await page.evaluate(() => fetch('/api/scope?level=global', { headers: (window as any)._authHeaders() }).then(r => r.json()));
  const env = scope.capabilities.find((c: any) => c.key === 'env');
  const scoped = (env.registry || []).filter((r: any) => r.scopable);
  expect(scoped.length).toBeGreaterThan(0);
  const backlog = env.registry.find((r: any) => r.key === 'AMUX_DISPATCH_BACKLOG_WHEN_IDLE');
  expect(backlog).toMatchObject({ scopable: true, precedence: 'process_first' });
  expect(env.registry.find((r: any) => r.key === 'AMUX_TMUX_COLS')).toMatchObject({ scopable: false });

  await page.evaluate(() => (window as any)._scopeEditOpen('global', '', 'env'));
  const modal = page.locator('#scope-edit-backdrop');
  await expect(modal).toHaveClass(/open/);
  const det = modal.locator('#scope-env-registry');
  await expect(det.locator('summary')).toHaveText(scoped.length + ' keys can be set per worker or group');
  await det.locator('summary').click();
  const row = det.locator('.scope-env-registry-row[data-key="AMUX_WORKSPACE_ISOLATION"]');
  await expect(row).toBeVisible();
  await expect(row).toContainText('worker > group > global');
  await expect(det.locator('.scope-env-registry-row[data-key="AMUX_TMUX_COLS"]')).toHaveCount(0);
  // Reopening must not stack a second list.
  await page.evaluate(() => (window as any)._scopeEditOpen('global', '', 'env'));
  await expect(modal.locator('#scope-env-registry')).toHaveCount(1);
  // Another capability shows no registry.
  await page.evaluate(() => (window as any)._scopeEditOpen('global', '', 'memory'));
  await expect(modal.locator('#scope-env-registry')).toHaveCount(0);
  // The Save button stays on screen at phone width.
  const save = modal.getByRole('button', { name: 'Save', exact: true });
  const box = (await save.boundingBox())!;
  expect(box.y + box.height).toBeLessThanOrEqual(page.viewportSize()!.height);
  await page.screenshot({ path: info.outputPath('scope-env-registry.png') });
});
