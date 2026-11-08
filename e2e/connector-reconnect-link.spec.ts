// The link in a connector alert (Ethan, 2026-11-08: "an owner alert with a
// one-tap re-consent link the moment one cannot be refreshed"). Opening
// /#reconnect=<family>:<account> lands on Connectors and asks once; Reconnect
// starts that account's grant, Cancel starts nothing. Every project, the
// 375px phone included.
import { test, expect } from './fixtures';

const ACCOUNT = 'broken@example.org';

test('a reconnect link asks once, then starts the grant for that account', async ({ page }) => {
  const starts: string[] = [];
  // A remembered last worker, as on the owner's own browser: restoring it at
  // boot ran over the prompt the first time this link was opened live.
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_peek_state', JSON.stringify({ session: 'restored-w', ts: Date.now() }));
    sessionStorage.setItem('peekState', JSON.stringify({ session: 'restored-w', ts: Date.now() }));
  });
  await page.route(/\/api\/connectors\/google\/auth\?/, (r) => {
    starts.push(new URL(r.request().url()).searchParams.get('account') || '');
    return r.fulfill({ json: { ok: true, auth: 'oauth2', authorize_url: '/#consent-opened', redirect_uri_registered: true } });
  });
  await page.goto('/#reconnect=google:' + encodeURIComponent(ACCOUNT));
  const modal = page.locator('#modal-backdrop.open');
  await expect(modal).toBeVisible({ timeout: 15_000 });
  await expect(page.locator('#modal-msg')).toContainText(ACCOUNT);
  // Still up once the boot restore has had its turn.
  await page.waitForTimeout(1500);
  await expect(modal).toBeVisible();
  await expect(page.locator('#peek-overlay.active')).toHaveCount(0);
  await expect(page.locator('#tab-connectors')).toHaveClass(/active/);
  expect(starts).toHaveLength(0);
  // Big enough to tap on a phone (the dialog's shared buttons).
  const btn = page.locator('#modal-btns button', { hasText: 'Reconnect' });
  expect((await btn.boundingBox())!.height).toBeGreaterThanOrEqual(34);
  const popup = page.waitForEvent('popup', { timeout: 5_000 }).catch(() => null);
  await btn.click();
  await expect.poll(() => starts).toEqual([ACCOUNT]);
  // The consent page opens in a new tab, or in this one when the popup is blocked.
  const p = await popup;
  if (!p) await expect.poll(() => page.url()).toContain('#consent-opened');
});

test('cancelled, a reconnect link starts nothing; a malformed one is ignored', async ({ page }) => {
  let starts = 0;
  page.on('request', (r) => { if (/\/api\/connectors\/[^/]+\/auth\?/.test(r.url())) starts++; });
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/#reconnect=google:' + encodeURIComponent(ACCOUNT));
  await expect(page.locator('#modal-backdrop.open')).toBeVisible({ timeout: 15_000 });
  await page.locator('#modal-btns button', { hasText: 'Cancel' }).click();
  await expect(page.locator('#modal-backdrop.open')).toHaveCount(0);
  await page.goto('/#reconnect=Bad Family:' + encodeURIComponent(ACCOUNT));
  await page.waitForTimeout(1500);
  await expect(page.locator('#modal-backdrop.open')).toHaveCount(0);
  expect(starts).toBe(0);
});
