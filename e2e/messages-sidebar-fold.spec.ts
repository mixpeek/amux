// Messages sidebar folds (Ethan, 2026-10-13: "this in messages tab contents
// should be collapsible?"). Section headers fold their list and keep the open
// conversation and unread ones; on wider screens the whole sidebar folds to a
// rail. Both survive a reload.
import { test, expect } from './fixtures';

async function openMessages(page: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvToggleSec === 'function'
    && typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await expect(page.locator('#cv-side')).toBeVisible();
}

test('sections fold, keep open and unread items, and stay folded after a reload', async ({ page }) => {
  // Three channels from the server: one opened below, one unread, one plain.
  await page.route(/\/api\/conversations$/, (route) => route.fulfill({ json: {
    channels: [{ conv: 'g:a', name: 'fold-a' }, { conv: 'g:b', name: 'fold-b', unread: 2 }, { conv: 'g:c', name: 'fold-c' }],
    people: [], workers: [], totals: {}, me: { scope_level: 'global' },
  } }));
  await openMessages(page);
  await expect(page.locator('#cv-channels [data-cv="g:c"]')).toBeVisible();
  // Phones show a conversation in place of the list, so only wider screens
  // have an open conversation beside a folded section.
  const phone = page.viewportSize()!.width <= 600;
  if (!phone) {
    await page.route(/\/api\/conversations\/g%3Aa\/posts/, (route) => route.fulfill({ json: { posts: [], members: [] } }));
    await page.locator('#cv-channels [data-cv="g:a"]').click();
    await expect(page.locator('#cv-channels [data-cv="g:a"]')).toHaveClass(/active/);
  }
  const head = page.locator('#cv-side .cv-sec[data-sec="channels"]');
  await expect(head).toHaveAttribute('aria-expanded', 'true');
  await head.click();
  await expect(head).toHaveAttribute('aria-expanded', 'false');
  await expect(page.locator('#cv-channels [data-cv="g:c"]')).toBeHidden();
  if (!phone) await expect(page.locator('#cv-channels [data-cv="g:a"]'), 'the open conversation stays').toBeVisible();
  await expect(page.locator('#cv-channels [data-cv="g:b"]'), 'an unread one stays').toBeVisible();

  await page.reload();
  await page.waitForFunction(() => typeof (window as any)._cvToggleSec === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await expect(page.locator('#cv-side .cv-sec[data-sec="channels"]')).toHaveAttribute('aria-expanded', 'false');
  await page.locator('#cv-side .cv-sec[data-sec="channels"]').click();
  await expect(page.locator('#cv-side .cv-sec[data-sec="channels"]')).toHaveAttribute('aria-expanded', 'true');
});

test('the sidebar folds to a rail on wide screens; phones have no rail', async ({ page }) => {
  await openMessages(page);
  const toggle = page.locator('#cv-side-toggle');
  const vw = page.viewportSize()!.width;
  if (vw <= 600) {
    await expect(toggle).toBeHidden();
    return;
  }
  const before = (await page.locator('#cv-side').boundingBox())!.width;
  await toggle.click();
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await expect.poll(async () => Math.round((await page.locator('#cv-side').boundingBox())!.width)).toBeLessThanOrEqual(45);
  await expect(page.locator('#cv-search')).toBeHidden();
  const tb = (await toggle.boundingBox())!;
  expect(tb.width).toBeGreaterThanOrEqual(32);
  // The picture and the button both fit inside the rail (they sat side by
  // side and the rail clipped half of each).
  const rail = (await page.locator('#cv-side').boundingBox())!;
  for (const sel of ['#cv-me', '#cv-side-toggle']) {
    const b = (await page.locator(sel).boundingBox())!;
    expect(b.width, sel + ' has a size').toBeGreaterThan(0);
    expect(b.x, sel + ' left edge inside the rail').toBeGreaterThanOrEqual(rail.x);
    expect(b.x + b.width, sel + ' right edge inside the rail').toBeLessThanOrEqual(rail.x + rail.width + 0.5);
  }
  await toggle.click();
  await expect.poll(async () => Math.round((await page.locator('#cv-side').boundingBox())!.width)).toBe(Math.round(before));
  await expect(page.locator('#cv-search')).toBeVisible();
});
