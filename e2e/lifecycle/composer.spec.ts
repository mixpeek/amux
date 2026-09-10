import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';

for (const outcome of ['refused', 'accepted', 'queued'] as const) {
  test(`LC-COMPOSER: ${outcome} delivery preserves the draft until the server answers`, async ({ page, request }, info) => {
    test.setTimeout(60_000);
    await boot(page);
    const headers = await auth(page);
    const name = `lc-compose-${info.project.name}-${Date.now()}`;
    expect((await request.post('/api/sessions', { headers, data: { name, dir: '/tmp' } })).status()).toBe(201);
    let release!: () => void;
    const pending = new Promise<void>(resolve => { release = resolve; });
    let requests = 0;
    const beacons: any[] = [];
    page.on('request', r => { if (r.url().endsWith('/api/client-debug') && r.method() === 'POST') { const data = r.postDataJSON(); if (data.kind === 'composer-delivery') beacons.push(data); } });
    // A controlled transport failure/delay exercises the shipped send handler.
    // No model is started; this is not counted as successful live coordination.
    await page.route(`**/api/sessions/${name}/send`, async route => {
      requests++;
      await pending;
      await route.fulfill({ status: outcome === 'refused' ? 422 : outcome === 'queued' ? 503 : 200,
        json: outcome !== 'accepted' ? { error: 'lifecycle controlled refusal' } : { ok: true, submitted: true } });
    });
    try {
      await page.reload();
      const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
      await card.locator('.card-menu-btn').click();
      await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
      await page.locator('#peek-composer-more-btn').click();
      const chooser = page.waitForEvent('filechooser');
      await page.locator('#peek-more-menu').getByRole('button', { name: 'Attach file', exact: false }).click();
      await (await chooser).setFiles({ name: 'retained-draft.txt', mimeType: 'text/plain', buffer: Buffer.from('Keep this upload with its draft.') });
      await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
      await expect(page.locator('#peek-attach-bar .uploading')).toHaveCount(0);
      await expect(page.locator('#peek-attach-bar .failed')).toHaveCount(0);
      const input = page.locator('#peek-cmd-input');
      const send = page.locator('#peek-overlay .send-split-main');
      const message = `Preserve this ${outcome} message ${name}`;
      await input.fill(message);
      await send.click();
      await expect.poll(() => requests).toBe(1);
      await expect(input, 'pending delivery is not permission to discard the draft').toHaveValue(message);
      await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
      await expect(send, 'a second tap must not submit the same pending message').toBeDisabled();
      release();
      await expect(send).toBeEnabled();
      if (outcome === 'refused') {
        await expect(input).toHaveValue(message);
        await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
        await expect.poll(() => beacons.some(b => b.verdict === 'unconfirmed' && b.draft_retained)).toBe(true);
        await expect(page.locator('#toast')).toContainText(/failed|error|not confirmed/i);
        await checkpoint(page, info, 'refused-draft-retained');
        await page.getByRole('button', { name: 'Close worker', exact: true }).click();
        await page.reload();
        await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
        await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
        await expect(input, 'failed send must also survive a reload').toHaveValue(message);
      } else {
        await expect(input).toHaveValue('');
        await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);
        if (outcome === 'queued') {
          expect(await page.evaluate(() => eval('offlineQueue').some((q: any) => q.options.body.includes('Preserve this queued message')))).toBe(true);
          await expect(page.locator('#toast')).toContainText(/queued/i);
        }
      }
      expect(requests).toBe(1);
    } finally {
      release();
      await deleteOwnedWorkers(page, request, headers, [name]);
    }
  });
}
