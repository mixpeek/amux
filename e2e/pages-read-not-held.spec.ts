// A read made during a click returns as soon as the server answers, whatever
// happens to its receipt. In the Mac app IndexedDB stalled (idb-reopened,
// attachment-save-stalled), the receipt bookkeeping never finished, and the
// Pages tab (now Apps) sat on "Loading..." with /api/pages and /api/page-links both
// answered 200 (owner, 2026-10-28). Here the receipt step is made to hang
// forever, the stall, and the tab must still fill in.
import { test, expect } from './fixtures';

test('Apps loads on a click even when the interaction receipt never completes', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/#view=sessions');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  // The stall: acknowledging a receipt never resolves.
  await page.evaluate(() => { (window as any).eval('_interactions').acknowledge = () => new Promise(() => {}); });
  const tab = page.locator('#tab-pages');
  if (await tab.isVisible()) await tab.click();
  else await page.evaluate(() => (window as any).switchView('pages'));
  const list = page.locator('#pages-list');
  await expect(list).not.toHaveText(/^\s*Loading/, { timeout: 15_000 });
  // The shared sections drew (a card or their empty box), so /api/page-links
  // returned too. (Inside amux has no empty box of its own.)
  await expect(page.locator('#pub-apps')).not.toBeEmpty();
  await expect(page.locator('#pub-files')).not.toBeEmpty();
});
