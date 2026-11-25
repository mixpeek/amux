// Owner, 2026-11-25: "consolidate all our settings to the amux Mac app
// settings". Inside the Mac app, Settings has a This Mac tab drawn from what
// the app sends (desktop/amux-settings.swift macSettingsState) and every change
// goes back through window.webkit.messageHandlers.amuxSettings. A browser has
// no such bridge, so it never shows the tab. desktop/test/mac-settings-e2e.py
// drives the real app; this pins the page side in every browser and on a phone.
import { test, expect, Page } from './fixtures';

const STATE = {
  workspaces: [{ name: 'Morning', summary: '2 windows', shows: ['Board', 'Calendar'], saved: 'Nov 25, 2026 at 9:00 AM', open: true },
               { name: `Ethan's "desk" <b id=inj>x</b>`, summary: '1 window', shows: ['Board'], saved: 'Nov 24, 2026 at 8:00 AM', open: false }],
  launch: 'last', split: 'right',
  toggles: { computerUse: true, quickAsk: true, openAtLogin: true, menuBar: true, notifications: true, dockBadge: true, snapToGrid: true, zoomAllWindows: false },
  computerUseNow: 'on, driver running', openAtLoginNow: 'on', spacesCanMove: true,
  permissions: [{ id: 'accessibility', name: 'Accessibility', for: 'computer use', state: 'on' },
                { id: 'screen', name: 'Screen Recording', for: 'previews', state: 'off' }],
  update: { built: 'built from abc123456', state: 'idle', detail: '', canInstall: false, busy: false },
  server: 'https://localhost:8824',
};

async function boot(page: Page, bridge: boolean) {
  await page.addInitScript(([on, st]) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    if (!on) return;
    (window as any).__sent = [];
    (window as any).webkit = { messageHandlers: { amuxSettings: { postMessage: (m: any) => {
      (window as any).__sent.push(m);
      const s = JSON.parse(JSON.stringify(st));
      if (m.action === 'toggle') s.toggles[m.name] = m.value;
      // The app answers asynchronously, as evaluateJavaScript does.
      setTimeout(() => (window as any)._macSettingsState(s), 20);
    } } } };
  }, [bridge, STATE] as const);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._macSettingsOpen === 'function');
}

test('a browser has no This Mac tab, and the app cannot open one there', async ({ page }) => {
  await boot(page, false);
  await page.evaluate(() => localStorage.setItem('amux_settings_tab', 'mac'));
  expect(await page.evaluate(() => (window as any)._macSettingsOpen())).toBe(false);
  await page.evaluate(() => (window as any).toggleSettings());
  await expect(page.locator('#settings-tab-mac')).toBeHidden();
  await expect(page.locator('.settings-tab-btn.active')).toHaveAttribute('data-stab', 'account');
});

test('inside the app, Settings… opens This Mac, draws the app state, and sends each change back', async ({ page }) => {
  await boot(page, true);
  expect(await page.evaluate(() => (window as any)._macSettingsOpen())).toBe(true);
  await expect(page.locator('#settings-tab-mac')).toBeVisible();
  await expect(page.locator('.settings-tab-btn.active')).toHaveAttribute('data-stab', 'mac');
  const host = page.locator('#mac-settings');
  await expect(host).toContainText('Morning');
  await expect(host).toContainText('open now');
  await expect(host.locator('[data-permission]')).toHaveCount(2);
  await expect(host.locator('[data-permission="screen"] .btn')).toHaveText('Turn On');
  await expect(page.locator('#mac-set-zoomAllWindows')).not.toBeChecked();
  await expect(page.locator('#mac-set-dockBadge')).toBeChecked();
  await page.screenshot({ path: test.info().outputPath('this-mac.png') });

  await page.locator('#mac-set-dockBadge + .theme-track').click();
  await expect.poll(() => page.evaluate(() => (window as any).__sent.find((m: any) => m.action === 'toggle'))).toEqual({ action: 'toggle', name: 'dockBadge', value: false });
  await expect(page.locator('#mac-set-dockBadge')).not.toBeChecked();

  await page.locator('#mac-set-split').selectOption('below');
  await page.locator('#mac-ws-name').fill('Evening');
  // A state push from the app mid-typing must not wipe the name.
  await page.evaluate(() => (window as any)._macSettingsSend('state'));
  await page.waitForTimeout(100);
  await expect(page.locator('#mac-ws-name')).toHaveValue('Evening');
  await page.locator('#mac-ws-save').click();
  await host.locator('[data-permission="screen"] .btn').click();
  const sent = await page.evaluate(() => (window as any).__sent.map((m: any) => m.action + ':' + m.name + ':' + (m.value ?? '')));
  expect(sent).toEqual(expect.arrayContaining(['splitDirection::below', 'save:Evening:', 'permission:screen:']));

  // A name from another Mac can hold quotes and markup: it shows as text and its buttons send it exactly.
  await expect(host).toContainText(`Ethan's "desk" <b id=inj>x</b>`);
  expect(await page.locator('#inj').count()).toBe(0);
  await host.locator('.mac-set-stack', { hasText: 'Ethan' }).getByRole('button', { name: 'Open' }).click();
  expect(await page.evaluate(() => (window as any).__sent.at(-1))).toEqual({ action: 'open', name: `Ethan's "desk" <b id=inj>x</b>`, value: undefined });

  // A re-render from a fresh app answer keeps the tab and its rows.
  await page.evaluate(() => (window as any)._macSettingsSend('state'));
  await expect(host.locator('[data-permission]')).toHaveCount(2);
});

test('This Mac fits a phone: no row wider than the screen', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 800 });
  await boot(page, true);
  await page.evaluate(() => (window as any)._macSettingsOpen());
  await expect(page.locator('#mac-settings')).toContainText('Computer use');
  // Every control and label, not only the rows: a row can fit while its buttons spill out of it.
  const over = await page.evaluate(() => [...document.querySelectorAll('#mac-settings .mac-set-label, #mac-settings .btn, #mac-settings .input')]
    .filter(e => { const r = e.getBoundingClientRect(); const box = e.closest('.mac-set-actions')?.getBoundingClientRect();
      return r.width > 0 && (box ? box.right : r.right) > window.innerWidth + 1; }).map(e => e.textContent || e.id));
  expect(over).toEqual([]);
  // The workspace's name and summary get the row's width, not a sliver beside its buttons.
  const label = await page.locator('#mac-settings .mac-set-stack .mac-set-label').first().boundingBox();
  expect(label!.width).toBeGreaterThan(250);
  await page.screenshot({ path: test.info().outputPath('this-mac-phone.png') });
});
