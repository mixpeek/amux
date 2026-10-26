// A worker @mention in a direct message (Ethan, 2026-10-25, sandbox.amux.io:
// "@amux-helper can you make an mdai for @doron ..." in his DM with Doron
// rendered no mention and reached nobody; "this should work"). The server now
// offers workers in a DM, isolated ones included for a person, and says where
// each answer will be. The conversation API is stubbed (the test server has no
// workers); the server half is pinned by api::conversations unit tests.
import { test, expect, Page } from './fixtures';

const ME = { email: 'ethan@x.com', key: 'person:ethan@x.com', kind: 'person', name: 'ethan', scope_level: 'global', scope_name: '' };
const DM = 'p:doron@z.com|ethan@x.com';
const LIST = { measured: true, n_considered: 1, me: ME,
  channels: [{ conv: 'g:general', name: 'general', unread: 0, mentions: 0, last_ts: 0 }],
  people: [{ conv: DM, email: 'doron@z.com', name: 'doron', online: false, unread: 0, mentions: 0 }],
  workers: [] };
const now = Date.now();
const POST = { id: 1, conv: DM, author_key: 'person:ethan@x.com', author_name: 'ethan',
  body: '@amux-helper can you make an mdai for @doron that has the weather in Bergenfield NJ',
  mentions: ['worker:amux-helper', 'person:doron@z.com'],
  deliveries: [{ worker: 'amux-helper', ok: true, state: 'delivered', status: 200, detail: 'confirmed', reply: 'pane' }],
  ts: now - 60000 };
const PEOPLE = [{ key: 'person:ethan@x.com', name: 'ethan', online: true }, { key: 'person:doron@z.com', name: 'doron', online: false }];

async function open(page: Page, posts: object[]) {
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: LIST }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => r.fulfill({ json: {
    measured: true, n_considered: posts.length, posts, people: PEOPLE, workers: [],
    mentionable_workers: ['amux-helper', 'ed'], typing: [] } }));
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate(dm => (0, eval)('_cvSelect(' + JSON.stringify(dm) + ')'), DM);
}

test('a worker mention in a DM renders and its receipt says where the answer is', async ({ page }) => {
  await open(page, [POST]);
  const post = page.locator('.cv-post').first();
  await expect(post.locator('.cv-mention', { hasText: '@amux-helper' })).toBeVisible();
  await expect(post.locator('.cv-mention', { hasText: '@doron' })).toBeVisible();
  await expect(post.locator('.cv-deliv')).toHaveText('✓ delivered to amux-helper; it answers in its terminal');
});

test('a DM offers workers in the @ picker', async ({ page }) => {
  await open(page, []);
  const inp = page.locator('#cv-input');
  await inp.click();
  await inp.pressSequentially('@amux');
  const pick = page.locator('#cv-ac button', { hasText: 'amux-helper' });
  await expect(pick).toBeVisible();
  await expect(pick.locator('small')).toHaveText('worker');
  await pick.dispatchEvent('mousedown');
  await expect(inp).toHaveValue('@amux-helper ');
});
