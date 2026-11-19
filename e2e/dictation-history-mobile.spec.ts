// On a phone a dictation in History reads at full width (owner, 2026-11-18: the
// text was squeezed into a column of two or three words beside five action
// buttons). The time and the actions share one row; the text gets its own.
import { test, expect } from './fixtures';

const TEXT = 'Right, so, Sean is going to the office on the 14th of December, and I can fly out from Baltimore and be back the same night.';

test('a dictation in History reads at full width on a phone, actions on one row above it', async ({ page }, info) => {
  test.skip(info.project.name === 'desktop', 'a phone layout');
  await page.route(/\/api\/dictation\/history(?:\?.*)?$/, r => r.fulfill({ json: {
    items: [{ id: 1, ts: Date.now(), text: TEXT, session: 'dict-mobile-worker', ai_edited: false }], total_words: 24 } }));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any).setPeekTab === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
  await page.evaluate(() => { (window as any).openPeek('dict-mobile-worker'); (window as any).setPeekTab('dictation'); });
  const row = page.locator('.dict-row').first();
  await expect(row.locator('.dict-text')).toContainText('Baltimore');
  // Measured until the row settles: on ios-safari one CI run read the boxes
  // mid-layout (text at 356, actions ending at 373, AF-1295), and every local
  // repeat passed. A layout that never settles still fails here.
  await expect(async () => {
    const r = (await row.boundingBox())!;
    const text = (await row.locator('.dict-text').boundingBox())!;
    const acts = (await row.locator('.dict-acts').boundingBox())!;
    const time = (await row.locator('.dict-time').boundingBox())!;
    expect(text.width, 'the text spans the row').toBeGreaterThan(r.width * 0.8);
    expect(text.y, 'the text sits below the time and the actions').toBeGreaterThanOrEqual(acts.y + acts.height - 1);
    expect(Math.abs(acts.y - time.y), 'time and actions share the first row').toBeLessThan(acts.height);
  }).toPass({ timeout: 5000 });
  for (const b of await row.locator('.dict-acts button').all()) {
    const bb = (await b.boundingBox())!;
    expect(Math.min(bb.width, bb.height), 'a 44px touch target').toBeGreaterThanOrEqual(44);
    expect(bb.x + bb.width, 'inside the screen').toBeLessThanOrEqual(page.viewportSize()!.width + 1);
  }
  await page.screenshot({ path: info.outputPath('dictation-history-mobile.png') });
});
