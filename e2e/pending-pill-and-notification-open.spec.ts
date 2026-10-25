// Ethan, 2026-10-20: "when i click pending it should show me the modal thing in
// addition to the thing in the bottom", and "when i get a notification it
// should click into it in the board".
import {test, expect} from './fixtures';

test.beforeEach(async ({page}) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/', {waitUntil: 'domcontentloaded'});
  await page.waitForFunction(() => typeof (window as any)._connPillClick === 'function'
    && typeof (window as any)._notifPush === 'function');
});

test('a pending tap opens the status modal and the bottom checklist', async ({page}) => {
  await page.evaluate("drafts.push({name: 'pending-probe', cmd: ''})");
  await page.evaluate(() => (window as any)._connPillClick());
  await expect(page.locator('#conn-hist-modal')).toBeVisible();
  await expect(page.locator('#sync-banner')).toHaveClass(/active/);
});

test('with nothing pending the tap opens only the status modal', async ({page}) => {
  await page.evaluate('drafts.length = 0; offlineQueue.length = 0');
  await page.evaluate(() => (window as any)._connPillClick());
  await expect(page.locator('#conn-hist-modal')).toBeVisible();
  await expect(page.locator('#sync-banner')).not.toHaveClass(/active/);
});

test('a notification about a card opens that card on the board', async ({page}) => {
  await page.evaluate(() => (window as any)._notifPush('!', 'needs you', 'SCHED-717 failed', 'mxp-gs12', 'AMUX-6258'));
  const banner = page.locator('#notif-banners .notif-banner').last();
  await banner.click();
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#issue=AMUX-6258');
  // And the same card from the notification panel's stored item.
  const item: any = await page.evaluate('_notifItems[0]');
  expect(item.card).toBe('AMUX-6258');
});
