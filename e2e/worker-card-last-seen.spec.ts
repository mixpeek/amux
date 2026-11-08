// A card's time is when the worker last did anything. last_activity is the last
// interaction, so a worker on a long autonomous run read "WORKING 12h ago"
// (2026-11-01, amux-frustrations).
import { test, expect } from './fixtures';

test('a card times a worker by its last progress, falling back to its last interaction', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._lastSeen === 'function');
  const seen = await page.evaluate(() => {
    const f = (window as any)._lastSeen;
    return [
      f({ last_activity: 100, last_progress_at: 900.7 }),
      f({ last_activity: 900, last_progress_at: 100 }),
      f({ last_activity: 100 }),
      f({}),
    ];
  });
  expect(seen).toEqual([900, 900, 100, 0]);
});
