import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';
import { createHash } from 'node:crypto';

const digest = (bytes: Buffer) => createHash('sha256').update(bytes).digest('hex');

test('LC-UPLOAD: real multi-chunk bytes, image preview, worker switching and removal', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  await boot(page);
  const headers = await auth(page);
  const suffix = `${info.project.name}-${Date.now()}`;
  const names = [`lc-upload-${suffix}`, `lc-upload-peer-${suffix}`];
  for (const name of names) expect((await request.post('/api/sessions', { headers, data: { name, dir: '/tmp' } })).status()).toBe(201);
  const open = async (name: string) => {
    await page.goto('/');
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
  };
  const file = { name: `résumé-${suffix}-long-file-name-for-phone-layout.txt`, mimeType: 'text/plain',
    buffer: Buffer.from(`Amux upload ${suffix}\n`.repeat(170000)) };
  const picture = { name: 'pixel.png', mimeType: 'image/png', buffer: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64') };
  try {
    await open(names[0]);
    await page.locator('#peek-composer-more-btn').click();
    const choose = page.waitForEvent('filechooser');
    await page.locator('#peek-more-menu').getByRole('button', { name: 'Attach file', exact: false }).click();
    await (await choose).setFiles([file, picture]);
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(2);
    await expect(page.locator('#peek-attach-bar .uploading')).toHaveCount(0, { timeout: 90_000 });
    await expect(page.locator('#peek-attach-bar .failed')).toHaveCount(0);
    const uploads = await page.evaluate(() => eval('peekFiles').map((f: any) => ({ name: f.name, path: f.path, url: f.url, totalChunks: f.totalChunks })));
    expect(uploads[0].totalChunks).toBeGreaterThan(1);
    for (const [i, input] of [file, picture].entries()) {
      const downloaded = await request.get(uploads[i].url, { headers });
      expect(downloaded.ok()).toBeTruthy();
      expect(digest(await downloaded.body())).toBe(digest(input.buffer));
    }
    await expect(page.locator('#peek-attach-bar img')).toBeVisible();
    expect(await page.locator('#peek-attach-bar img').evaluate((img: HTMLImageElement) => img.naturalWidth)).toBe(1);
    await checkpoint(page, info, 'real-upload-chips');
    // Switch using UI close/open rather than reloading: in-memory File objects
    // cannot be persisted through a reload by the browser.
    await page.getByRole('button', { name: 'Close worker', exact: true }).click();
    await page.locator(`.card[data-session="${names[1]}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);
    await page.getByRole('button', { name: 'Close worker', exact: true }).click();
    await page.locator(`.card[data-session="${names[0]}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(2);
    await page.locator('#peek-attach-bar .chip-remove').first().click();
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
    await page.locator('#peek-composer-more-btn').click();
    await page.locator('#peek-more-menu').getByRole('button', { name: 'Clear input', exact: false }).click();
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);
    await info.attach('upload-byte-proof', { body: JSON.stringify(uploads.map((u: any, i: number) => ({ ...u, bytes: [file, picture][i].buffer.length, sha256: digest([file, picture][i].buffer) })), null, 2), contentType: 'application/json' });
  } finally { await deleteOwnedWorkers(page, request, headers, names); }
});
