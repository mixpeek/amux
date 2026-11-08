// Messages search and discovery (owner, 2026-11-05: "improve the chat ux, we
// need better search/discovery"). The sidebar box searches as you type: a
// panel with filter chips (From, In, Time, Has) that write the server's
// operators, conversations whose name matches, and messages with the words
// marked; recent searches when empty; ↑ ↓ Enter Esc and "/". Enter opens a
// results page with counts per conversation. Real server, no route stubs,
// desktop and a 375px phone.
import { test, expect, Page, APIRequestContext } from './fixtures';

test.describe.configure({ mode: 'serial' });

async function api(request: APIRequestContext, tok: string, method: string, path: string, data?: unknown) {
  const r = await request.fetch(path, { method, data, headers: { Authorization: `Bearer ${tok}` } });
  return { status: r.status(), json: await r.json().catch(() => ({})) };
}
const phone = (page: Page) => (page.viewportSize()?.width || 1000) <= 600;

async function open(page: Page) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); localStorage.removeItem('amux_cv_recent_search'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvSelect === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.evaluate(() => (window as any)._cvSelect('g:general'));
  await expect(page.locator('#cv-pane')).toBeVisible();
  if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
  return page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
}

test('typing searches as you go, filters narrow it, and a hit opens its message', async ({ page, request }) => {
  const tok = await open(page);
  const w = 'quokka' + Date.now().toString(36);
  for (const body of [`the ${w} plan is ready`, `${w} notes at https://example.com/${w}`, `unrelated words`])
    expect((await api(request, tok, 'POST', '/api/conversations/g:general/posts', { body })).status).toBeLessThan(300);
  const box = page.locator('#cv-search'), pan = page.locator('#cv-sq');

  // Empty: the panel opens with the filters and how to use them.
  await box.click();
  await expect(pan).toBeVisible();
  await expect(pan.locator('.cv-sq-chip')).toHaveCount(4);
  // The panel stays on the screen, a phone's included.
  const r = await pan.boundingBox(); const vw = page.viewportSize()!.width;
  expect(r!.x).toBeGreaterThanOrEqual(0); expect(r!.x + r!.width).toBeLessThanOrEqual(vw);

  // Typing: both posts show, with the word marked, before any Enter.
  await box.pressSequentially(w.slice(0, 9), { delay: 20 });
  const hits = pan.locator('.cv-sq-hit');
  await expect(hits).toHaveCount(2);
  await expect(hits.locator('mark').first()).toBeVisible();
  await expect(pan.locator('.cv-sq-sec', { hasText: 'Messages' })).toContainText('2');

  // Has ▾ → A link: the operator lands in the box and only the link post is left.
  await pan.locator('.cv-sq-chip', { hasText: 'Has' }).click();
  await pan.locator('.cv-sq-row', { hasText: 'A link' }).click();
  await expect(box).toHaveValue(new RegExp('has:link'));
  await expect(hits).toHaveCount(1);
  await expect(hits.first()).toContainText('example.com');
  await expect(pan.locator('.cv-sq-chip.on', { hasText: 'Has: link' })).toBeVisible();
  // Time ▾ → Past 24 hours keeps it (it was just posted).
  await pan.locator('.cv-sq-chip', { hasText: 'Time' }).click();
  await pan.locator('.cv-sq-row', { hasText: 'Past 24 hours' }).click();
  await expect(box).toHaveValue(/after:24h/);
  await expect(hits).toHaveCount(1);
  // Clearing a chip removes its operator.
  await pan.locator('.cv-sq-chip.on', { hasText: 'Has' }).click();
  await expect(box).not.toHaveValue(/has:link/);
  await expect(hits).toHaveCount(2);

  // ↓ then Enter opens the first hit in its conversation.
  await box.press('ArrowDown');
  await expect(pan.locator('.cv-sq-row.sel')).toHaveCount(1);
  await box.press('Escape');
  await expect(pan).toBeHidden();
  await box.click();
  await expect(hits).toHaveCount(2);
  await hits.first().click();
  await expect(pan).toBeHidden();
  await expect(page.locator('#cv-pane')).toBeVisible();
  await expect(page.locator('#cv-title')).toContainText('general');
  await expect(page.locator('#cv-posts .cv-post, #cv-thread .cv-post', { hasText: w }).first()).toBeVisible();
});

test('a conversation is found by name, a bad filter is named, and recent searches come back', async ({ page }) => {
  await open(page);
  const box = page.locator('#cv-search'), pan = page.locator('#cv-sq');
  await box.click();
  await box.pressSequentially('gener');
  const conv = pan.locator('.cv-sq-row', { hasText: '#general' }).first();
  await expect(conv).toBeVisible();
  await box.fill('gener has:nope');
  await expect(pan.locator('.cv-sq-warn')).toContainText('has:nope');
  await box.fill('gener');
  await expect(conv).toBeVisible();
  await box.press('ArrowDown');
  await box.press('Enter');
  await expect(page.locator('#cv-title')).toContainText('general');
  // A search run with Enter is remembered and offered when the box is empty.
  if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
  await box.fill('');
  await box.fill('zz-recent-probe');
  await box.press('Enter');
  if (phone(page)) await page.evaluate(() => (window as any)._cvBack());
  await box.fill('');
  await box.click();
  await expect(pan.locator('.cv-sq-sec', { hasText: 'Recent searches' })).toBeVisible();
  await expect(pan.locator('.cv-sq-row', { hasText: 'zz-recent-probe' })).toBeVisible();
});

test('Enter opens every result with counts per conversation, and a count narrows to it', async ({ page, request }) => {
  const tok = await open(page);
  const w = 'wombat' + Date.now().toString(36);
  for (let i = 0; i < 3; i++) await api(request, tok, 'POST', '/api/conversations/g:general/posts', { body: `${w} number ${i}` });
  const box = page.locator('#cv-search');
  await box.fill(w);
  await box.press('Enter');
  await expect(page.locator('#cv-meta')).toHaveText(/^3 results in 1 conversation/);
  await expect(page.locator('#cv-posts .cv-post')).toHaveCount(3);
  await expect(page.locator('#cv-posts .cv-post-body mark.cv-hl').first()).toHaveText(w);
  await expect(page.locator('#cv-posts .cv-snippet')).toHaveCount(0);
  await page.locator('.cv-sr-bar .cv-sq-chip', { hasText: 'general' }).click();
  await expect(page.locator('#cv-title')).toContainText('in:#general');
  await expect(page.locator('.cv-sr-bar .cv-sq-chip.on')).toContainText('In: #general');
  await expect(page.locator('#cv-posts .cv-post')).toHaveCount(3);
});

test('"/" puts you in the search box from anywhere in Messages', async ({ page }) => {
  test.skip(phone(page), 'a phone has no keyboard shortcut');
  await open(page);
  await page.locator('#cv-posts').click({ position: { x: 5, y: 5 } });
  await page.evaluate(() => (document.activeElement as HTMLElement)?.blur());
  await page.keyboard.press('/');
  await expect(page.locator('#cv-search')).toBeFocused();
  await expect(page.locator('#cv-sq')).toBeVisible();
});
