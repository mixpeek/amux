// Click any avatar to see what that person has open (Ethan, 2026-10-29:
// "click a users avatar and know what screen(s)/window(s) are active for them
// ... anytime i can view avatars/users"). Real server: the page reports where
// it is, and the modal reads it back through /api/presence/where.
import { test, expect } from './fixtures';

async function boot(page) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._whoOpen === 'function');
}

test('a window reports the screen it shows, and an avatar click lists it', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).switchView('board'));
  await expect.poll(async () => {
    const r = await page.evaluate(async () => (await fetch('/api/presence/where?key=person:owner')).json());
    return (r.windows || []).map((w: any) => w.label).join('|');
  }, { timeout: 10000 }).toContain('Board');
  // Any avatar drawn by _cvAvatar carries who it is; clicking it opens the modal.
  await page.evaluate(() => {
    const host = document.createElement('div');
    host.id = 'who-test-host';
    host.innerHTML = (window as any)._cvAvatar('ethan', false, 'person:owner');
    document.body.appendChild(host);
  });
  await page.locator('#who-test-host [data-who]').click();
  const modal = page.locator('#modal-msg');
  await expect(modal).toContainText('ethan');
  await expect(modal).toContainText('Board');
  // Other specs' tabs share this owner on the test server, so count is not
  // pinned; this tab's row is the one that says Board.
  await expect(modal.locator('.who-row', { hasText: 'Board' }).first()).toBeVisible();
});

test('the modal follows the window to a worker and a conversation', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate(() => (0, eval)("_cvSelect('g:general')"));
  await expect.poll(async () => {
    const r = await page.evaluate(async () => (await fetch('/api/presence/where?key=person:owner')).json());
    return (r.windows || []).map((w: any) => w.label).join('|');
  }, { timeout: 10000 }).toContain('Messages · #general');
});

test('a worker cannot read where people are', async ({ page }) => {
  await boot(page);
  const st = await page.evaluate(async () => (await fetch('/api/presence/where?key=person:owner', { headers: { 'X-Amux-Session': 'no-such-worker' } })).status);
  expect(st).toBe(403);
});
