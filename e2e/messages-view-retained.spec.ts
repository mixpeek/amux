// Ethan, 2026-10-27: "make sure when i have a tab content open like a
// particular message in global message view like if im messaging someone and
// switch to worker view it retains the view of the msg".
// Conversations are stubbed (the test server has no workers or people).
import { test, expect, Page, allowUnusedRoute } from './fixtures';

const ME = { email: 'owner', key: 'person:owner', kind: 'person', name: 'owner', scope_level: 'global', scope_name: '' };
const LIST = { measured: true, n_considered: 3, me: ME,
  channels: [{ conv: 'g:general', name: 'general', unread: 0, mentions: 0, last_ts: 0 }],
  people: [{ conv: 'p:owner|dana', email: 'dana', name: 'dana', online: true, unread: 0, mentions: 0 }],
  workers: [] };
const now = Date.now();
const POSTS = Array.from({ length: 60 }, (_, i) => ({ id: i + 1, conv: 'p:owner|dana', author_key: 'person:owner', author_name: 'owner',
  body: 'dm post ' + (i + 1), mentions: [], deliveries: [], ts: now - (60 - i) * 60000 }));

async function setup(page: Page) {
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: LIST }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => {
    const conv = decodeURIComponent(r.request().url().split('/api/conversations/')[1].split('/')[0]);
    const posts = conv === 'p:owner|dana' ? POSTS : [];
    return r.fulfill({ json: { measured: true, n_considered: posts.length, posts, people: [ME], workers: [], mentionable_workers: [], typing: [] } });
  });
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  // The All messages case never opens a conversation.
  allowUnusedRoute(page, /\/api\/conversations\/[^/]+\/posts(\?.*)?$/);
  allowUnusedRoute(page, /\/api\/conversations\/[^/]+\/(read|typing)$/);
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
}

test('a conversation open in Messages is still open, scrolled and drafted, after visiting Workers', async ({ page }) => {
  await setup(page);
  await page.evaluate(() => (0, eval)("_cvSelect('p:owner|dana')"));
  await expect(page.locator('.cv-post').last()).toContainText('dm post 60');
  // Read back up the thread and start a reply.
  await page.locator('#cv-posts').evaluate(el => { el.scrollTop = 200; });
  await page.locator('#cv-input').fill('half-written reply');
  const before = await page.locator('#cv-posts').evaluate(el => el.scrollTop);

  await page.evaluate(() => (window as any).switchView('sessions'));
  await expect(page.locator('#messages-view')).toBeHidden();
  await page.evaluate(() => (window as any).switchView('messages'));

  await expect(page.locator('#messages-view')).toHaveClass(/cv-open/);
  await expect(page.locator('#cv-title')).toContainText('dana');
  await expect(page.locator('.cv-post').first()).toBeVisible();
  await expect(page.locator('#cv-input')).toHaveValue('half-written reply');
  expect(Math.abs(await page.locator('#cv-posts').evaluate(el => el.scrollTop) - before), 'the thread stays where it was read to').toBeLessThanOrEqual(2);
});

test('the All messages log keeps its page after visiting Workers', async ({ page }) => {
  // Two pages of history, from the real server.
  await page.goto('/');
  await page.evaluate(async () => {
    for (let i = 0; i < 30; i++) await fetch('/api/history', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text: 'retained-log ' + i, type: 'user', session: 'retain-probe' }) });
  });
  await setup(page);
  await page.evaluate(() => (0, eval)("_cvSelect('@all')"));
  await page.evaluate(() => (0, eval)("_msgsSetPageSize(25)"));
  await expect.poll(() => page.evaluate(() => (0, eval)('_msgsPageCount()'))).toBeGreaterThanOrEqual(2);
  await page.evaluate(() => (0, eval)("_msgsGoToPage(2)"));
  await expect.poll(() => page.evaluate(() => (0, eval)('_msgsPage'))).toBe(2);
  await page.evaluate(() => (window as any).switchView('sessions'));
  await page.evaluate(() => (window as any).switchView('messages'));
  await expect.poll(() => page.evaluate(() => (0, eval)('_msgsPage'))).toBe(2);
  await expect(page.locator('#msgs-log-pane')).toBeVisible();
});
