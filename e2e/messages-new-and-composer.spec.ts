// Messages: start a new message, and write it in the worker input's components
// (Ethan, 2026-10-28: "i want to be able to create/add messages", "the input
// should be the same input components as here woker input"). The conversation
// API is stubbed, as in messages-worker-mention.spec.ts; the server half (a
// worker mentioned in a DM answers in that thread) is pinned by
// api::conversations unit tests.
import { test, expect, Page } from './fixtures';

const ME = { email: 'ethan@x.com', key: 'person:ethan@x.com', kind: 'person', name: 'ethan', scope_level: 'global', scope_name: '' };
const DM = 'p:doron@z.com|ethan@x.com';
const LIST = { measured: true, n_considered: 3, me: ME,
  channels: [{ conv: 'g:general', name: 'general', unread: 0, mentions: 0, last_ts: 0 }],
  people: [{ conv: DM, email: 'doron@z.com', name: 'doron', online: false, unread: 0, mentions: 0 }],
  workers: [{ conv: 'w:ed', name: 'ed', unread: 0, mentions: 0 }] };
const PEOPLE = [{ key: 'person:ethan@x.com', name: 'ethan', online: true }, { key: 'person:doron@z.com', name: 'doron', online: false }];

async function open(page: Page) {
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: LIST }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => r.fulfill({ json: {
    measured: true, n_considered: 0, posts: [], people: PEOPLE, workers: [],
    mentionable_workers: ['ed'], typing: [] } }));
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvNewMessage === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
}

test('New message picks a person, a channel or a worker and keeps the draft', async ({ page }) => {
  await open(page);
  await page.locator('#cv-new-btn').click();
  const list = page.locator('#cv-new-list');
  await expect(list.locator('.cv-item')).toHaveCount(3);
  await expect(list).toContainText('#general');
  await expect(list).toContainText('doron');
  await expect(list).toContainText('ed');
  await page.locator('#cv-new-to').fill('dor');
  await expect(list.locator('.cv-item')).toHaveCount(1);
  await page.locator('#cv-new-to').press('Enter');
  await expect(page.locator('#cv-title')).toHaveText('doron');
  await page.locator('#cv-input').fill('@ed get me customer A\'s reply from the inbox');
  // A second New message keeps what was typed and moves it to the new conversation.
  await page.locator('#cv-new-btn').click();
  await list.locator('.cv-item', { hasText: 'ed' }).click();
  await expect(page.locator('#cv-title')).toHaveText('ed');
  await expect(page.locator('#cv-input')).toHaveValue('@ed get me customer A\'s reply from the inbox');
});

test('the header + menu starts a new message from any view', async ({ page }) => {
  await open(page);
  await page.evaluate(() => (window as any).switchView('board'));
  await page.locator('#add-btn').click();
  await page.locator('#add-menu .card-menu-item', { hasText: 'New message' }).click();
  await expect(page.locator('#cv-new-to')).toBeFocused();
  await page.locator('#cv-new-list .cv-item', { hasText: '#general' }).click();
  await expect(page.locator('#messages-view')).toBeVisible();
  await expect(page.locator('#cv-title')).toHaveText('#general');
});

test('the Messages input is the worker input: box, grip, ⋮ menu and Send', async ({ page }) => {
  await open(page);
  await page.evaluate(dm => (0, eval)('_cvSelect(' + JSON.stringify(dm) + ')'), DM);
  for (const id of ['#cv-input', '#cv-thread-input']) {
    await expect(page.locator(id)).toHaveClass(/send-input/);
  }
  await expect(page.locator('#cv-composer-grip')).toBeAttached();
  await expect(page.locator('#cv-send')).toHaveText('Send');
  await page.locator('#cv-input').fill('draft to throw away');
  await page.locator('#cv-more-btn').click();
  const menu = page.locator('#cv-more-menu');
  await expect(menu).toBeVisible();
  for (const label of ['Attach file', 'Dictate', 'Voice note', 'Clear input']) await expect(menu).toContainText(label);
  await menu.locator('button', { hasText: 'Clear input' }).click();
  await expect(menu).toBeHidden();
  await expect(page.locator('#cv-input')).toHaveValue('');
  // @ still offers people and workers in the new box.
  await page.locator('#cv-input').pressSequentially('@e');
  await expect(page.locator('#cv-ac')).toContainText('ed');
});

test('on a phone the input stays one row and every control is a 44px target', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 740 });
  await open(page);
  await page.evaluate(dm => (0, eval)('_cvSelect(' + JSON.stringify(dm) + ')'), DM);
  const row = await page.locator('#cv-compose').boundingBox();
  expect(row!.height).toBeLessThanOrEqual(64);
  for (const sel of ['#cv-more-btn', '#cv-send']) {
    const b = (await page.locator(sel).boundingBox())!;
    expect(b.height, sel).toBeGreaterThanOrEqual(44);
    expect(b.width, sel).toBeGreaterThanOrEqual(44);
    expect(b.x + b.width, sel).toBeLessThanOrEqual(375);
  }
  await page.locator('#cv-pane .cv-back').click();
  const nb = (await page.locator('#cv-new-btn').boundingBox())!;
  expect(nb.height).toBeGreaterThanOrEqual(44);
});
