// Found filming on demo builds (long-form-videos, 2026-11-02): a worker with no
// board read "IDLE · NEVER", and a card preview could show "### Updated request".
import { test, expect } from './fixtures';

test('idle text and card previews read as plain words', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._idleMovedSuffix === 'function');
  const r = await page.evaluate(() => {
    const w = window as any;
    return {
      noBoard: w._idleMovedSuffix({ board_used: false }),
      isolated: w._idleMovedSuffix({ isolated: true }),
      boardNever: w._idleMovedSuffix({ board_used: true }),
      noBoardTitle: w._idleMovedTitle({ board_used: false }),
      heading: w._descPreviewLine('### Updated request\nfix it'),
      plain: w._descPreviewLine('fix the thing'),
      hash: w._descPreviewLine('#4521 is the card'),
      empty: w._descPreviewLine(undefined),
    };
  });
  expect(r.noBoard).toBe('');
  expect(r.isolated).toBe('');
  expect(r.boardNever, 'a worker with a board may still have moved no card').toBe(' · never');
  expect(r.noBoardTitle).not.toContain('board card');
  expect(r.heading).toBe('Updated request');
  expect(r.plain).toBe('fix the thing');
  expect(r.hash, 'only a real heading mark is removed').toBe('#4521 is the card');
  expect(r.empty).toBe('');
});
