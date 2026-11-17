// Key mode (2026-11-17): Ctrl+; in the worker view sends arrows, Enter, Esc
// and the rest to the worker from the keyboard, as the key chips do. Keys
// requests are captured, never delivered.
import { test, expect } from './fixtures';

test('Ctrl+; forwards keys in order, Esc reaches the worker, typing turns it off', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  const keys: string[] = [];
  const beacons: any[] = [];
  await page.route(/\/api\/sessions\/km-w\/keys$/, async (route) => {
    keys.push(JSON.parse(route.request().postData() || '{}').keys);
    await route.fulfill({ json: { ok: true, detail: 'sent' } });
  });
  page.on('request', (r) => {
    if (r.url().endsWith('/api/client-debug') && r.method() === 'POST') {
      try { const b = JSON.parse(r.postData() || '{}'); if (b.kind === 'interrupt-key') beacons.push(b); } catch (e) {}
    }
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(() => {
    const g = globalThis as any;
    g.eval('sessions').push({ name: 'km-w', status: 'active', running: true, renderer: 'terminal' });
    g.openPeek('km-w');
  });
  const overlay = page.locator('#peek-overlay');
  const input = page.locator('#peek-cmd-input');
  const badge = page.locator('#peek-keymode-badge');
  await expect(overlay).toBeVisible();
  await input.focus();

  // Off: arrows stay with the composer.
  await page.keyboard.press('ArrowDown');
  await page.waitForTimeout(300);
  expect(keys).toEqual([]);
  await expect(badge).toHaveCount(0);

  // On: keys go to the worker, in order, even with the composer focused.
  await page.keyboard.press('Control+;');
  await expect(badge).toBeVisible();
  await expect(badge).toContainText('Keys → worker');
  for (const k of ['ArrowDown', 'ArrowDown', 'ArrowUp', 'Tab', 'Shift+Tab', 'Space', 'y', '2', 'Enter', 'Escape'])
    await page.keyboard.press(k);
  await expect.poll(() => keys).toEqual(['Down', 'Down', 'Up', 'Tab', 'BTab', 'Space', 'y', '2', 'Enter', 'Escape']);
  await expect(overlay).toBeVisible(); // Esc went to the worker, not to close the view
  await expect(input).toHaveValue('');
  await expect.poll(() => beacons.map(b => [b.session, b.keys, b.source])).toContainEqual(['km-w', 'Escape', 'key-mode']);
  await expect(badge).toContainText('Esc');
  await page.screenshot({ path: test.info().outputPath('key-mode-on.png') });

  // Any other key turns it off and reaches the composer.
  await page.keyboard.press('h');
  await expect(badge).toHaveCount(0);
  await expect(input).toHaveValue('h');
  await page.keyboard.press('Enter');
  await page.waitForTimeout(300);
  expect(keys.length, 'Enter after typing is a newline again, not a key').toBe(10);

  // Ctrl+; toggles off; Escape then closes the view as before.
  await page.keyboard.press('Control+;');
  await expect(badge).toBeVisible();
  await page.keyboard.press('Control+;');
  await expect(badge).toHaveCount(0);
  await page.keyboard.press('Escape');
  await expect(overlay).not.toBeVisible();
  expect(keys.length).toBe(10);
});

test('key mode refuses a worker that is not a terminal', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(() => {
    const g = globalThis as any;
    g.eval('sessions').push({ name: 'km-chat', status: 'active', running: true, renderer: 'chat' });
    g.openPeek('km-chat');
  });
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await page.keyboard.press('Control+;');
  await expect(page.locator('#peek-keymode-badge')).toHaveCount(0);
});
