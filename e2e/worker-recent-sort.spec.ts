import { test, expect, Page } from './fixtures';

// ha-bed-lights, 2026-11-24: the owner started a worker from their phone, it
// finished and went idle, and neither "Recent activity" nor "Last message from
// me" put it at the top. "Recent activity" ranked busy lanes above idle ones
// before looking at recency; "Last message from me" read last_human_ts, which
// the create-time prompt never set (server half, session_verbs.rs).
const workers = [
  { name: 'busy-old', status: 'active', last_activity: 1000, last_human_ts: 1_000_000 },
  { name: 'waiting-old', status: 'waiting', last_activity: 2000, last_human_ts: 2_000_000 },
  { name: 'just-started', status: 'idle', last_activity: 5000, last_human_ts: 5_000_000 },
  { name: 'never-messaged', status: 'active', last_activity: 4000, last_human_ts: 0 },
  { name: 'pinned-old', status: 'idle', pinned: true, last_activity: 1, last_human_ts: 1 },
].map(w => ({ running: true, pinned: false, dir: '/tmp/', provider: 'codex', tags: [], ...w }));

async function prepare(page: Page, sort: string) {
  await page.addInitScript(({ workers, sort }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_sessions_cache', JSON.stringify(workers));
    localStorage.removeItem('amux_frozen');
    localStorage.removeItem('amux_card_order');
    localStorage.setItem('amux_layout', 'list');
    localStorage.setItem('amux_sort_mode', sort);
  }, { workers, sort });
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: workers }));
  await page.goto('/');
  await expect(page.locator('#cards .card[data-session]')).toHaveCount(workers.length);
}

const names = (page: Page) => page.locator('#cards .card[data-session]')
  .evaluateAll(nodes => nodes.map(n => (n as HTMLElement).dataset.session));

test('Recent activity puts the newest activity first, even when that worker is idle', async ({ page }, testInfo) => {
  await prepare(page, 'natural');
  await expect(page.locator('#tile-sort-btn')).toHaveAttribute('title', 'Sort: Recent activity');
  expect(await names(page)).toEqual(['pinned-old', 'just-started', 'never-messaged', 'waiting-old', 'busy-old']);
  await page.screenshot({ path: testInfo.outputPath('recent-activity.png'), fullPage: true });
});

test('Last message from me puts the worker last messaged first, never-messaged last', async ({ page }) => {
  await prepare(page, 'human');
  await expect(page.locator('#tile-sort-btn')).toHaveAttribute('title', 'Sort: Last message from me');
  expect(await names(page)).toEqual(['pinned-old', 'just-started', 'waiting-old', 'busy-old', 'never-messaged']);
});
