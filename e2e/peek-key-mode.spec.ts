// Key mode (2026-11-17): Ctrl+; in the worker view sends arrows, Enter, Esc
// and the rest to the worker from the keyboard, as the key chips do. Keys
// requests are captured, never delivered.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

// The worker list is served by a route, so a sessions refresh returns the same
// worker. Pushing into the live `sessions` array raced a refresh on mobile: the
// worker vanished and an unknown worker reads as a terminal (PR #465 review).
async function boot(page: Page, name: string, renderer: 'terminal' | 'chat') {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name, dir: '/tmp/km', running: true, status: 'active', renderer, flags: '', steering: [] }] }));
  await page.goto('/');
  await page.waitForFunction(n => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === n), name);
}
// Opens the worker and checks the page still knows it, with its renderer.
async function openWorker(page: Page, name: string, renderer: 'terminal' | 'chat') {
  await page.evaluate(n => (globalThis as any).openPeek(n), name);
  await expect(page.locator('#peek-overlay')).toBeVisible();
  expect(await page.evaluate(n => {
    const s = (globalThis as any).eval('sessions').find((x: any) => x.name === n);
    return s && s.renderer;
  }, name)).toBe(renderer);
}

test('Ctrl+; forwards keys in order, Esc reaches the worker, typing turns it off', async ({ page }) => {
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
  await boot(page, 'km-w', 'terminal');
  await openWorker(page, 'km-w', 'terminal');
  const overlay = page.locator('#peek-overlay');
  const input = page.locator('#peek-cmd-input');
  const badge = page.locator('#peek-keymode-badge');
  await expect(overlay).toBeVisible();
  await input.focus();

  // The focused, empty composer says how to turn key mode on, on a device
  // that can hover with a fine pointer; a touch-only phone never gets the hint.
  const hint = page.locator('#peek-keymode-hint');
  const finePointer = await page.evaluate(() => matchMedia('(hover: hover) and (pointer: fine)').matches);
  if (!finePointer) await expect(hint).toHaveCount(0);
  if (test.info().project.name === 'desktop') {
    await expect(hint).toBeVisible();
    await expect(hint).toHaveText(/^(⌃;|Ctrl\+;) keys$/);
    await page.screenshot({ path: test.info().outputPath('key-mode-hint.png') });
  }

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
  await boot(page, 'km-chat', 'chat');
  await openWorker(page, 'km-chat', 'chat');
  // Still a chat worker at the moment the shortcut is pressed.
  expect(await page.evaluate(() => (globalThis as any)._workerRenderer('km-chat'))).toBe('chat');
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await page.keyboard.press('Control+;');
  await expect(page.locator('#peek-keymode-badge')).toHaveCount(0);
});

test('key mode follows its Hotkeys binding: rebinding moves the toggle, the hint and the badge', async ({ page }) => {
  const keys: string[] = [];
  await page.route(/\/api\/sessions\/km-r\/keys$/, async (route) => {
    keys.push(JSON.parse(route.request().postData() || '{}').keys);
    await route.fulfill({ json: { ok: true, detail: 'sent' } });
  });
  await boot(page, 'km-r', 'terminal');
  // What saving a new combination in the Hotkeys tab leaves in the list.
  await page.evaluate(() => {
    (globalThis as any).eval('_hkList').find((h: any) => h.id === 'hk-key-mode').trigger = 'Alt+Shift+K';
  });
  await openWorker(page, 'km-r', 'terminal');
  const badge = page.locator('#peek-keymode-badge');
  await page.locator('#peek-cmd-input').focus();
  if (test.info().project.name === 'desktop')
    await expect(page.locator('#peek-keymode-hint')).toHaveText(/^(⇧⌥K|⌥⇧K|Alt\+Shift\+K) keys$/);

  await page.keyboard.press('Control+;');
  await page.waitForTimeout(200);
  await expect(badge).toHaveCount(0);

  await page.keyboard.press('Alt+Shift+K');
  await expect(badge).toBeVisible();
  await expect(badge).toContainText(/(⌥⇧K|Alt\+Shift\+K) to stop/);
  await page.keyboard.press('ArrowDown');
  await expect.poll(() => keys).toEqual(['Down']);
  await page.keyboard.press('Alt+Shift+K');
  await expect(badge).toHaveCount(0);
});

test('key mode turns off after 10s idle unless its Hotkeys setting says stay on', async ({ page }) => {
  await page.clock.install();
  // The hotkeys pref is shared by the specs on this server: capture the save, keep it local.
  const saved: any[] = [];
  await page.route(/\/api\/prefs$/, async (route) => {
    if (route.request().method() !== 'POST') return route.fallback();
    saved.push(JSON.parse(route.request().postData() || '{}'));
    await route.fulfill({ json: { ok: true } });
  });
  await boot(page, 'km-i', 'terminal');
  const badge = page.locator('#peek-keymode-badge');

  // Default: idle turns it off.
  await openWorker(page, 'km-i', 'terminal');
  await page.locator('#peek-cmd-input').focus();
  await page.keyboard.press('Control+;');
  await expect(badge).toBeVisible();
  await page.clock.runFor(11000);
  await expect(badge).toHaveCount(0);
  await page.evaluate(() => (globalThis as any).closePeek());

  // Untick it in the Key mode hotkey's dialog.
  await page.evaluate("switchView('hotkeys')");
  await page.evaluate(() => (globalThis as any)._hkOpenDialog('hk-key-mode'));
  const dlg = page.locator('#hk-dialog');
  const idle = dlg.locator('#hk-km-idle');
  await expect(idle).toBeVisible();
  await expect(idle).toBeChecked();
  await dlg.locator('#hk-action').selectOption('search');
  await expect(idle).toBeHidden(); // only Key mode has it
  await dlg.locator('#hk-action').selectOption('key_mode');
  await idle.uncheck();
  await page.screenshot({ path: test.info().outputPath('key-mode-setting.png') });
  await dlg.locator('#hk-save').click();
  await expect(dlg).toBeHidden();
  const list = JSON.parse(saved.at(-1).value);
  expect(list.find((h: any) => h.id === 'hk-key-mode').idle_off).toBe(false);

  // Now it stays on through a long pause, and still turns off by the shortcut.
  await openWorker(page, 'km-i', 'terminal');
  await page.locator('#peek-cmd-input').focus();
  await page.keyboard.press('Control+;');
  await expect(badge).toBeVisible();
  await page.clock.runFor(60000);
  await expect(badge).toBeVisible();
  await page.keyboard.press('Control+;');
  await expect(badge).toHaveCount(0);
});
