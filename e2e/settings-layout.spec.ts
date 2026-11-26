// Settings layout (Ethan, 2026-10-14; regrouped 2026-11-26: "a seperate
// settings from mac/client vs server"): This device and This Mac (in the
// app), then Server (Account, Workers, Alerts, Connect). Connect lists connections first, folds Connection & security, and no
// longer carries Vault/Secrets, ChatGPT or API keys (a Vault tab will). Alerts
// folds Standing approvals. The style guide lives only in Device.
import { test, expect } from './fixtures';

test('settings tabs, folds and the style guide sit where the owner put them', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._settingsTab === 'function');
  await page.locator('#settings-btn').click();
  const menu = page.locator('#settings-menu');
  // This Mac (#settings-tab-mac) is in the markup but hidden outside the Mac app.
  await expect(menu.locator('.settings-tab-btn:visible')).toHaveText(['This device', 'Account', 'Workers', 'Alerts', 'Connect']);
  await expect(menu.locator('.settings-tab-group')).toHaveText(['Server']);
  // Every tab fits the menu's width at desktop size, so none hides off the edge.
  if ((page.viewportSize()?.width || 0) >= 1024) {
    const fits = await menu.locator('#settings-tabs').evaluate(el => el.scrollWidth <= el.clientWidth + 1);
    expect(fits, 'all tabs visible without scrolling on desktop').toBe(true);
  }
  const groups = await menu.locator('.settings-tab-btn').evaluateAll(bs => bs.map(b => b.dataset.stab + ':' + b.dataset.group));
  expect(groups).toEqual(['device:device', 'mac:device', 'account:server', 'workers:server', 'notifications:server', 'integrations:server']);
  // One row of tabs on a phone too: every label and tab shares one line.
  const strip = await menu.locator('#settings-tabs').boundingBox();
  const mids = await menu.locator('.settings-tab-group, .settings-tab-btn:visible').evaluateAll(es => es.map(e => { const r = e.getBoundingClientRect(); return r.top + r.height / 2; }));
  const firstMid = mids[0];
  expect(mids.every(m => Math.abs(m - firstMid) < 4), 'the tab strip is one row: ' + mids.join(',')).toBe(true);
  expect(strip!.height, 'and no taller than one row of tabs').toBeLessThan(64);
  // The device's own notification switches are in Device, not in the server's Alerts.
  await expect(page.locator('#stab-device #settings-device-notifs-section')).toHaveCount(1);
  await expect(page.locator('#stab-notifications #settings-device-notifs-section')).toHaveCount(0);
  await expect(page.locator('#settings-tab-mac')).toBeHidden();

  // Account: no style guide here.
  await expect(page.locator('#style-guide-btn')).toBeHidden();

  // Connect: connections first, security folded, the moved sections gone.
  await menu.locator('.settings-tab-btn[data-stab="integrations"]').click();
  const panel = page.locator('#stab-integrations');
  const first = await panel.locator('> .settings-section, > section').first().getAttribute('id');
  expect(first, 'connections are at the top of Connect').toBe('settings-connections-section');
  await expect(page.locator('#connection-security details.settings-fold')).not.toHaveAttribute('open', '');
  await expect(page.locator('#connection-owner-token')).toBeHidden();
  for (const id of ['settings-vault-section', 'settings-chatgpt-section']) {
    await expect(page.locator('#' + id), id + ' is not in Settings').toBeHidden();
  }
  await expect(page.locator('#style-guide-btn')).toBeHidden();

  // Device: the style guide is here.
  await menu.locator('.settings-tab-btn[data-stab="device"]').click();
  await expect(page.locator('#style-guide-btn')).toBeVisible();

  // Workers: every other server setting is one button away.
  await menu.locator('.settings-tab-btn[data-stab="workers"]').click();
  await page.locator('#settings-server-env-btn').click();
  await expect(page.locator('#scope-edit-backdrop')).toHaveClass(/open/);
  await expect(page.locator('#scope-edit-title')).toHaveText('Global · env');
  await page.evaluate(() => (window as any)._scopeEditClose());

  // Alerts: standing approvals folded.
  await menu.locator('.settings-tab-btn[data-stab="notifications"]').click();
  await expect(page.locator('#standing-approvals-section details.settings-fold')).not.toHaveAttribute('open', '');
  await expect(page.locator('#standing-approvals-list')).toBeHidden();
  await page.locator('#standing-approvals-section summary').click();
  await expect(page.locator('#standing-approvals-list')).toBeVisible();

  // Sent to sign in: the security fold opens itself.
  await page.evaluate(() => (window as any)._openConnectionSecurity());
  await expect(page.locator('#connection-owner-token')).toBeVisible();
});
