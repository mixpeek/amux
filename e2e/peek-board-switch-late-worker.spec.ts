import { test, expect } from './fixtures';

// The peek's Board switch renders only once the worker is in the page's
// sessions list. A peek opened on a worker the list did not have yet kept the
// switch hidden until the Board tab's board fetch came back, however long that
// took. A sessions refresh now re-syncs it.
const BASE = [{ name: 'already-here', running: true, status: 'idle', dir: '/tmp', board_decompose: false }];
const LATE = { name: 'just-made', running: true, status: 'idle', dir: '/tmp', board_decompose: false };

test('a peek opened before the worker reaches the list shows its Board switch once the list has it', async ({ page }) => {
  let list: any[] = BASE;
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: list }));
  await page.goto('/#view=sessions');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any).fetchSessions === 'function');
  await expect(page.locator('.card[data-session="already-here"]')).toBeVisible({ timeout: 15_000 });
  // The board fetch the Board tab starts is held: only the sessions refresh can show the switch.
  let release = () => {};
  const held = new Promise<void>(r => { release = r; });
  await page.context().route(/\/api\/board(?:\?.*)?$/, async r => { await held; await r.fallback(); });
  await page.evaluate(() => (window as any).openPeek('just-made'));
  await page.locator('#peek-tab-issues').click();
  const sw = page.locator('#peek-board-config .board-one-switch');
  await expect(sw, 'no switch while the list lacks the worker').toHaveCount(0);
  list = [...BASE, LATE];
  await page.evaluate(() => (window as any).fetchSessions());
  try {
    await expect(sw).toHaveText('Off', { timeout: 5_000 });
  } finally {
    release();
  }
});
