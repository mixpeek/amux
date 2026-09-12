import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';

test('LC-COMPOSER-LAYOUT: mobile writing space and Queue controls stay aligned and reachable', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  await boot(page);
  const headers = await auth(page);
  const name = `lc-layout-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers, data: { name, dir: '/tmp' } })).status()).toBe(201);
  const input = page.locator('#peek-cmd-input');
  const more = page.locator('#peek-composer-more-btn');
  const main = page.locator('#peek-overlay .send-split-main');
  const arrow = page.locator('#peek-overlay .send-split-arrow');
  const press = (locator: typeof more) => info.project.use.hasTouch ? locator.tap() : locator.click();
  const measurements: unknown[] = [];
  try {
    await page.reload();
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await expect(input).toBeVisible();
    for (const size of [page.viewportSize()!, { width: 320, height: 568 }, { width: 393, height: 330 }, { width: 667, height: 375 }]) {
      await page.setViewportSize(size);
      for (const mode of ['Send', 'Queue']) {
        if ((await main.innerText()) !== mode) await press(arrow);
        await expect(main).toHaveText(mode);
        for (const draft of ['Review this change', 'Acceptance criteria and evidence\n'.repeat(10)]) {
          await input.fill(draft);
          await expect(input).toHaveValue(draft);
          // Wait for visualViewport/keyboard resizing to settle, using the
          // actual controls rather than an injected layout or fake geometry.
          await expect.poll(async () => {
            const box = (await main.boundingBox())!;
            return box.y + box.height;
          }).toBeLessThanOrEqual(size.height);
          const field = (await input.boundingBox())!;
          const row = (await page.locator('#peek-cmd-row').boundingBox())!;
          const action = (await main.boundingBox())!;
          const menu = (await more.boundingBox())!;
          expect(Math.abs(menu.y + menu.height - action.y - action.height)).toBeLessThanOrEqual(1);
          for (const control of [more, main, arrow]) {
            const box = (await control.boundingBox())!;
            expect(box.width).toBeGreaterThanOrEqual(44);
            expect(box.height).toBe(44);
            expect(box.x).toBeGreaterThanOrEqual(0);
            expect(box.x + box.width).toBeLessThanOrEqual(size.width);
          }
          if (size.width <= 600) {
            expect(field.width).toBeGreaterThanOrEqual(row.width - 1);
            expect(field.height).toBeLessThanOrEqual(160);
            expect(menu.y).toBeGreaterThanOrEqual(field.y + field.height);
            const expand = (await page.locator('#peek-input-expand').boundingBox())!;
            expect(expand.width).toBe(44);
            expect(expand.height).toBe(44);
            expect(expand.y + expand.height).toBeLessThanOrEqual(field.y + field.height + 1);
          }
          measurements.push({ size, mode, multiline: draft.includes('\n'), inputWidth: field.width, actionBottomDelta: menu.y + menu.height - action.y - action.height });
          if (mode === 'Send' && !draft.includes('\n')) await checkpoint(page, info, `short-composer-${size.width}-${size.height}`);
        }
      }
      await checkpoint(page, info, `queue-composer-${size.width}-${size.height}`);
      await press(more);
      const menu = page.locator('#peek-more-menu');
      await expect(menu).toBeVisible();
      const box = (await menu.boundingBox())!;
      expect(box.x).toBeGreaterThanOrEqual(0);
      expect(box.y).toBeGreaterThanOrEqual(0);
      expect(box.x + box.width).toBeLessThanOrEqual(size.width);
      expect(box.y + box.height).toBeLessThanOrEqual(size.height);
      await checkpoint(page, info, `attachment-menu-${size.width}-${size.height}`);
      const chooser = page.waitForEvent('filechooser');
      await press(menu.getByRole('button', { name: 'Attach file', exact: false }));
      await (await chooser).setFiles([]);
      await expect(input).toHaveValue('Acceptance criteria and evidence\n'.repeat(10));
    }
    // Collapsing a large draft must also reclaim its height.
    await input.fill('');
    await expect.poll(async () => (await input.boundingBox())!.height).toBeLessThanOrEqual(60);
    await info.attach('composer-layout-measurements', { contentType: 'application/json', body: JSON.stringify(measurements) });
  } finally {
    await deleteOwnedWorkers(page, request, headers, [name]);
  }
});
