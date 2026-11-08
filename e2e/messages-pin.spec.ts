// Pinning a message inside a channel or DM (Ethan, 2026-10-29: "be able to pin
// individual messages to inside a channel or dm"). The real server: a post is
// pinned from its actions, shows as pinned, the header's 📌 count lists it and
// goes to it, and unpinning takes it out. The count sits in the header's one
// row on a phone.
import { test, expect } from './fixtures';

test('a message is pinned in a channel, listed from the header, and unpinned', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function' && typeof _cvSelect === 'function'");
  const tag = Math.random().toString(36).slice(2, 8);
  const post = async (body: string) => page.evaluate(async ([b]) => {
    const r = await fetch(API + '/api/conversations/' + encodeURIComponent('g:general') + '/posts', {
      method: 'POST', headers: _authHeaders({ 'Content-Type': 'application/json' }), body: JSON.stringify({ body: b }) });
    return (await r.json()).id as number;
  }, [body]);
  const keep = await post('pin me ' + tag);
  await post('other chatter ' + tag);
  await page.evaluate("switchView('messages')");
  await page.evaluate("_cvSelect('g:general')");
  const row = page.locator('#cv-posts .cv-post[data-id="' + keep + '"]');
  await expect(row).toBeVisible();
  // Relative to what #general already holds, so a retry on the same server passes.
  const before = await page.evaluate(() => ((_cv.info || {}).pins || []).length);

  // Pin it from its actions (the hover toolbar on desktop, the ⋯ sheet on a phone).
  if (page.viewportSize()!.width <= 600) {
    await row.locator('.cv-more').click();
    await page.locator('.cv-sheet-item', { hasText: 'Pin' }).click();
  } else {
    await row.hover();
    await row.getByRole('button', { name: 'Pin', exact: true }).click();
  }
  await expect(row.locator('.cv-post-pinned')).toBeVisible();
  await expect(page.locator('#cv-pins-btn')).toBeVisible();
  await expect(page.locator('#cv-pins-n')).toHaveText(String(before + 1));

  // The count is in the header row, not a new bar.
  const head = (await page.locator('.cv-head:has(#cv-pins-btn)').boundingBox())!;
  const btn = (await page.locator('#cv-pins-btn').boundingBox())!;
  expect(btn.y + btn.height / 2).toBeGreaterThanOrEqual(head.y);
  expect(btn.y + btn.height / 2).toBeLessThanOrEqual(head.y + head.height);

  // The list names it, and choosing it goes to it.
  await page.locator('#cv-pins-btn').click();
  const item = page.locator('.cv-pin-item', { hasText: 'pin me ' + tag });
  await expect(item).toBeVisible();
  await item.click();
  await expect(row).toBeInViewport();

  // Survives a reload of the conversation.
  await page.evaluate("_cvSelect('g:general')");
  await expect(page.locator('#cv-posts .cv-post[data-id="' + keep + '"] .cv-post-pinned')).toBeVisible();

  // Unpin.
  const again = page.locator('#cv-posts .cv-post[data-id="' + keep + '"]');
  if (page.viewportSize()!.width <= 600) {
    await again.locator('.cv-more').click();
    await page.locator('.cv-sheet-item', { hasText: 'Unpin' }).click();
  } else {
    await again.hover();
    await again.getByRole('button', { name: 'Unpin', exact: true }).click();
  }
  await expect(again.locator('.cv-post-pinned')).toHaveCount(0);
  if (before) await expect(page.locator('#cv-pins-n')).toHaveText(String(before));
  else await expect(page.locator('#cv-pins-btn')).toBeHidden();
});
