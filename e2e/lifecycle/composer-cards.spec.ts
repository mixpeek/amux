import { test, expect } from '../fixtures';
import { boot, auth, getSessionsResilient, deleteOwnedWorkers } from './evidence';

for (const mode of ['send', 'steer'] as const) {
  test(`LC-CARD-COMPOSER: ${mode} refusal keeps text, accepted retry keeps newly typed text`, async ({ page, request }, info) => {
    await boot(page);
    const headers = await auth(page);
    const name = `lc-card-compose-${info.project.name}-${Date.now()}`;
    expect((await request.post('/api/sessions', { headers, data: { name, dir: '/tmp' } })).status()).toBe(201);
    const rows = await (await getSessionsResilient(request, headers)).json();
    // Only physical runtime is a fixture; persistence and the actual composer
    // are real. A stopped worker does not expose a card composer.
    Object.assign(rows.find((s: any) => s.name === name), { running: true, status: 'active' });
    await page.route(/\/api\/sessions(?:\?.*)?$/, route => route.fulfill({ json: rows }));
    let release!: () => void;
    let accepted = false, calls = 0;
    let pending = new Promise<void>(resolve => { release = resolve; });
    await page.route(`**/api/sessions/${name}/${mode}`, async route => {
      calls++;
      await pending;
      await route.fulfill({ status: accepted ? 200 : 422, json: accepted ? { ok: true, id: 'confirmed-steer' } : { error: 'controlled permanent refusal' } });
    });
    try {
      await page.goto('/');
      const card = page.locator(`#cards .card[data-session="${name}"]`).locator('visible=true').first();
      await card.locator('.card-name').click();
      const input = card.locator('textarea.send-input');
      const send = card.locator('.send-split-main');
      await expect(input).toBeVisible();
      if (mode === 'steer') await card.locator('.send-split-arrow').click();
      await input.fill('Keep this refused card draft');
      await send.click();
      await expect.poll(() => calls).toBe(1);
      await expect(send).toBeDisabled();
      await expect(input).toHaveValue('Keep this refused card draft');
      release();
      await expect(send).toBeEnabled();
      await expect(input).toHaveValue('Keep this refused card draft');
      accepted = true;
      pending = new Promise<void>(resolve => { release = resolve; });
      await input.press('Control+Enter');
      await expect.poll(() => calls).toBe(2);
      await input.fill('New text written while the previous request is pending');
      release();
      await expect(send).toBeEnabled();
      await expect(input).toHaveValue('New text written while the previous request is pending');
    } finally {
      release();
      await page.unroute(/\/api\/sessions(?:\?.*)?$/);
      await deleteOwnedWorkers(page, request, headers, [name]);
    }
  });
}
