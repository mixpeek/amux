// Pin a message from a worker's Messages tab (Ethan, 2026-11-01). The pin lives
// on the server, shows on the row and in the 📌 list, and survives a reload.
import { test, expect } from './fixtures';

test('a message pinned in a worker tab is marked, listed, kept and can be unpinned', async ({ page }, testInfo) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  // workerIndex: runs started in the same millisecond shared one worker's pins.
  const name = 'zz-pin-' + testInfo.workerIndex + '-' + Date.now();
  await page.goto('/');
  const status = await page.evaluate(async n => {
    const post = (u: string, b: object) => fetch(u, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) });
    await post('/api/sessions', { name: n, dir: '/tmp', start: false });
    return (await post('/api/history', { text: 'pin me please', session: n })).status;
  }, name);
  expect(status).toBeLessThan(300);
  const open = async () => {
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
    await page.evaluate(n => { const g = window as any; g.openPeek(n); g.setPeekTab('messages'); }, name);
    await expect(page.locator('#peek-messages-list .pm-row').first()).toBeVisible();
  };
  await open();
  const row = page.locator('#peek-messages-list .pm-row').first();
  await row.locator('.pm-dots').click();
  await row.locator('.pm-menu-pin').click();
  await expect(row.locator('.pm-pin-mark')).toBeVisible();
  await expect(page.locator('#pm-pins-n')).toHaveText('1');

  await open();
  await expect(page.locator('#pm-pins-n')).toHaveText('1');
  await page.locator('#pm-pins-btn').click();
  await expect(page.locator('.cv-pin-item')).toContainText('pin me please');
  await page.locator('.cv-pin-item').first().click();
  await expect(page.locator('#peek-cmd-input')).toHaveValue('pin me please');

  await page.evaluate(() => (window as any).setPeekTab('messages'));
  await row.locator('.pm-dots').click();
  await expect(row.locator('.pm-menu-pin')).toHaveText('Unpin');
  await row.locator('.pm-menu-pin').click();
  await expect(row.locator('.pm-pin-mark')).toHaveCount(0);
  await expect(page.locator('#pm-pins-btn')).toBeHidden();

  // Unpin from the list too: an older pin may not be on the current page.
  await row.locator('.pm-dots').click();
  await row.locator('.pm-menu-pin').click();
  await expect(page.locator('#pm-pins-n')).toHaveText('1');
  await page.locator('#pm-pins-btn').click();
  await page.locator('.pm-pin-unpin').first().click();
  await expect(page.locator('.pm-pin-row')).toHaveCount(0);
  await expect(page.locator('#pm-pins-btn')).toBeHidden();
  await expect(row.locator('.pm-pin-mark')).toHaveCount(0);
});
