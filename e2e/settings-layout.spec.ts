// Settings layout (Ethan, 2026-10-14): tabs Account, Connect, Device, Workers,
// Alerts. Connect lists connections first, folds Connection & security, and no
// longer carries Vault/Secrets, ChatGPT or API keys (a Vault tab will). Alerts
// folds Standing approvals. The style guide lives only in Device.
import { test, expect } from './fixtures';

test('settings tabs, folds and the style guide sit where the owner put them', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._settingsTab === 'function');
  await page.locator('#settings-btn').click();
  const menu = page.locator('#settings-menu');
  await expect(menu.locator('.settings-tab-btn')).toHaveText(['Account', 'Connect', 'Device', 'Workers', 'Alerts']);

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
