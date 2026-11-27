// Owner, 2026-11-27: the Mac app's Settings… "should open up a modal that's
// outside of the amux app", client settings with a tab for the server's. The
// app's Settings window loads /?settings_window=1 (desktop/amux-settings.swift
// SettingsWindowController): Settings fills that window, is all it shows, and
// nothing in it closes it. A plain load keeps the gear's dropdown.
import { test, expect, Page } from './fixtures';

async function load(page: Page, path: string) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_settings_tab'); });
  await page.route(/\/api\/email\/approvals(?:\?.*)?$/, r => r.fulfill({ json: { pending: [] } }));
  await page.goto(path);
  await page.waitForFunction(() => typeof (window as any)._settingsWindowMode === 'function');
}

async function box(page: Page) {
  return page.evaluate(() => {
    const m = document.getElementById('settings-menu')!, r = m.getBoundingClientRect();
    const hits = [[0.5, 0.05], [0.05, 0.5], [0.5, 0.5], [0.95, 0.95]].map(([x, y]) => {
      const t = document.elementFromPoint(innerWidth * x, innerHeight * y);
      return !!(t && m.contains(t));
    });
    return { top: r.top, left: r.left, width: r.width, height: r.height, vw: innerWidth, vh: innerHeight, hits,
      open: m.classList.contains('open'), mode: document.documentElement.classList.contains('settings-window'),
      tab: (m.querySelector('.settings-tab-btn.active') as HTMLElement | null)?.dataset.stab || '' };
  });
}

for (const [label, w, h] of [['desktop', 1280, 800], ['phone', 375, 740]] as const) {
  test(`the Settings window shows Settings alone, filling it (${label} width)`, async ({ page }) => {
    await page.setViewportSize({ width: w, height: h });
    await load(page, '/?settings_window=1');
    await expect(page.locator('#settings-menu')).toHaveClass(/open/);
    const b = await box(page);
    expect(b.mode).toBe(true);
    expect([b.top, b.left]).toEqual([0, 0]);
    expect(Math.abs(b.width - b.vw)).toBeLessThanOrEqual(1);
    expect(Math.abs(b.height - b.vh)).toBeLessThanOrEqual(1);
    // Every corner of the window is Settings: nothing of the dashboard shows.
    expect(b.hits).toEqual([true, true, true, true]);
    // A browser has no app bridge: no This Mac tab, and the server's tabs are there.
    await expect(page.locator('#settings-tab-mac')).toBeHidden();
    await expect(page.locator('.settings-tab-btn[data-stab="workers"]')).toBeVisible();
    await page.screenshot({ path: test.info().outputPath(`settings-window-${w}.png`) });

    // Switching tabs, clicking inside, and what used to close the dropdown leave it open.
    await page.locator('.settings-tab-btn[data-stab="workers"]').click();
    expect((await box(page)).tab).toBe('workers');
    await page.locator('#stab-workers').click({ position: { x: 5, y: 5 } });
    await page.evaluate(() => (window as any).closeSettings());
    await page.evaluate(() => document.body.dispatchEvent(new MouseEvent('click', { bubbles: true })));
    const after = await box(page);
    expect(after.open).toBe(true);
    expect(after.hits).toEqual([true, true, true, true]);
  });
}

test('a plain load keeps the gear dropdown, which closes on an outside click', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await load(page, '/');
  expect(await page.evaluate(() => (window as any)._settingsWindowMode())).toBe(false);
  await expect(page.locator('#settings-menu')).not.toHaveClass(/open/);
  await page.locator('#settings-btn').click();
  await expect(page.locator('#settings-menu')).toHaveClass(/open/);
  const b = await box(page);
  expect(b.mode).toBe(false);
  expect(b.width).toBeLessThan(b.vw / 2);
  await page.evaluate(() => document.body.dispatchEvent(new MouseEvent('click', { bubbles: true })));
  await expect(page.locator('#settings-menu')).not.toHaveClass(/open/);
});
