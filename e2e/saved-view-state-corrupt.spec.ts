// A corrupt saved board view value is dropped, not thrown (implicit state
// machines inventory 2026-10-31, #37). The collapse keys were parsed with a
// bare JSON.parse at script load, so one bad value stopped the dashboard from
// starting at all.
import { test, expect } from './fixtures';

test('the dashboard starts and the board opens with corrupt saved collapse state', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_col_collapsed', '[backlog');
    localStorage.setItem('amux_board_collapsed', 'not json');
    localStorage.setItem('amux_status_collapsed', '["an array, not a map"]');
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('board'));
  await expect(page.locator('#board-view, #view-board, .board-columns').first()).toBeVisible();
  expect(errors, errors.join('\n')).toEqual([]);
  const kept = await page.evaluate(() => ['amux_col_collapsed', 'amux_board_collapsed', 'amux_status_collapsed'].map(k => localStorage.getItem(k)));
  // The bad values are gone (the column key is re-seeded when the board saves it).
  expect(kept[1]).toBeNull();
  expect(kept[2]).toBeNull();
  expect(kept[0] === null || Array.isArray(JSON.parse(kept[0]!))).toBe(true);
});
