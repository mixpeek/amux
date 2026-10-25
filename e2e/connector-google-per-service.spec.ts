// Each Google service is judged by the grant it runs on (2026-10-24). Gmail has
// its own grant; Calendar and Drive use the connector grant. When only the
// connector grant expires, Gmail must still read as working. The account
// list is a fake shaped like GET /api/connectors/accounts.
import { test, expect } from './fixtures';

const svc = (id: string, label: string) => ({ id, label, category: 'Google', auth: 'oauth2', status: 'connected', env_keys: [], oauth: { scopes: '' } });
const CONNECTORS = [svc('google-gmail', 'Gmail'), svc('google-calendar', 'Google Calendar'), svc('google-drive', 'Google Drive')];
const ok = { status: 'ok' };
const ACCOUNTS = { measured: true, accounts: [
  // Connector grant dead, Gmail grant alive: Gmail works, Calendar and Drive do not.
  { account: 'split@example.com', families: { gmail: 'ok', google: 'needs_reauth' }, canary: { gmail: ok }, needs_reauth: true },
  // Both grants dead.
  { account: 'gone@example.com', families: { gmail: 'needs_reauth', google: 'needs_reauth' }, canary: {}, needs_reauth: true },
  // Both alive.
  { account: 'fine@example.com', families: { gmail: 'ok', google: 'ok' }, canary: { gmail: ok, calendar: ok, drive: ok }, needs_reauth: false },
] };

test('a dead Calendar/Drive grant does not mark a working Gmail as expired', async ({ page }) => {
  await page.route(/\/api\/connectors(?:\?.*)?$/, r => r.fulfill({ json: { connectors: CONNECTORS } }));
  await page.route(/\/api\/connectors\/accounts(?:\?.*)?$/, r => r.fulfill({ json: ACCOUNTS }));
  await page.goto('/?view=connectors');
  const row = (who: string) => page.locator('.cx-acct').filter({ hasText: who }).first();
  const chip = (who: string, label: string) => row(who).locator('.cx-svc', { hasText: label });
  await expect(row('split@example.com')).toBeVisible({ timeout: 15000 });

  await expect(chip('split@example.com', 'Gmail')).toHaveClass(/cx-ok/);
  await expect(chip('split@example.com', 'Calendar')).toHaveClass(/cx-bad/);
  await expect(chip('split@example.com', 'Drive')).toHaveClass(/cx-bad/);
  await expect(row('split@example.com'), 'still offers the repair').toContainText('Reconnect');

  for (const label of ['Gmail', 'Calendar', 'Drive']) {
    await expect(chip('gone@example.com', label)).toHaveClass(/cx-bad/);
    await expect(chip('fine@example.com', label)).toHaveClass(/cx-ok/);
  }
});
